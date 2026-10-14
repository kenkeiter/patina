//! GDT and TSS for the boot CPU.
//!
//! Selector layout (fixed; SYSCALL/SYSRET in Phase 1 depend on it):
//!
//! | sel  | descriptor                         |
//! |------|------------------------------------|
//! | 0x00 | null                               |
//! | 0x08 | kernel code, 64-bit, DPL0          |
//! | 0x10 | kernel data, DPL0                  |
//! | 0x18 | user code, 32-bit, DPL3 (unused; SYSRET base) |
//! | 0x20 | user data, DPL3  (SYSRET SS = base + 8)  |
//! | 0x28 | user code, 64-bit, DPL3 (SYSRET CS = base + 16) |
//! | 0x30 | TSS (16 bytes)                     |
//!
//! STAR for Phase 1: `STAR[47:32] = 0x08`, `STAR[63:48] = 0x18 | 3`.
//!
//! The TSS has IST1 = a dedicated 16 KiB double-fault stack, so a kernel
//! stack overflow (guard page -> #PF -> #DF) still produces a dump.
//! `rsp0` (ring-3 -> ring-0 stack) is set per thread from Phase 1.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
pub const USER_CS32: u16 = 0x18 | 3;
pub const USER_DS: u16 = 0x20 | 3;
pub const USER_CS: u16 = 0x28 | 3;
pub const TSS_SEL: u16 = 0x30;

/// IST index (1-based, as used in IDT gates) of the double-fault stack.
pub const IST_DOUBLE_FAULT: u8 = 1;
const DF_STACK_SIZE: usize = 16 * 1024;

#[repr(C, packed(4))]
pub struct Tss {
    _reserved0: u32,
    pub rsp: [u64; 3],
    _reserved1: u64,
    pub ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    pub iomap_base: u16,
}

const _: () = assert!(size_of::<Tss>() == 104);

const GDT_ENTRIES: usize = 8; // 6 segment descriptors + 2 slots for the TSS

#[repr(C, align(16))]
struct Gdt([u64; GDT_ENTRIES]);

#[repr(C, align(16))]
struct DfStack([u8; DF_STACK_SIZE]);

struct Cpu0 {
    gdt: UnsafeCell<Gdt>,
    tss: UnsafeCell<Tss>,
}

// SAFETY: written once by the boot CPU before anything else can run; later
// accessed only by the CPU itself (ltr sets the busy bit).
unsafe impl Sync for Cpu0 {}

static CPU0: Cpu0 = Cpu0 {
    gdt: UnsafeCell::new(Gdt([0; GDT_ENTRIES])),
    tss: UnsafeCell::new(Tss {
        _reserved0: 0,
        rsp: [0; 3],
        _reserved1: 0,
        ist: [0; 7],
        _reserved2: 0,
        _reserved3: 0,
        iomap_base: size_of::<Tss>() as u16, // no I/O bitmap
    }),
};

/// Separate static so it lands in .bss rather than .data.
struct DfStackCell(UnsafeCell<DfStack>);
// SAFETY: only ever used by the CPU as an IST stack.
unsafe impl Sync for DfStackCell {}
static DF_STACK: DfStackCell = DfStackCell(UnsafeCell::new(DfStack([0; DF_STACK_SIZE])));

const KCODE64: u64 = 0x00af_9a00_0000_ffff;
const KDATA: u64 = 0x00cf_9200_0000_ffff;
const UCODE32: u64 = 0x00cf_fa00_0000_ffff;
const UDATA: u64 = 0x00cf_f200_0000_ffff;
const UCODE64: u64 = 0x00af_fa00_0000_ffff;

fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let low = (limit as u64 & 0xffff)
        | ((base & 0xff_ffff) << 16)
        | (0x89u64 << 40) // present, DPL0, type 9 = available 64-bit TSS
        | (((limit as u64 >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);
    let high = base >> 32;
    (low, high)
}

/// The boot CPU's TSS (its `rsp[0]` is updated by `cpu::set_kernel_stack`).
/// Phase 2: one GDT/TSS per CPU.
pub(super) fn boot_tss() -> *mut Tss {
    CPU0.tss.get()
}

#[repr(C, packed)]
struct DescriptorPtr {
    limit: u16,
    base: u64,
}

/// Build and load the GDT and TSS on the boot CPU; reload all segment
/// registers. Called once, early, while still on the boot tables.
pub(super) fn init() {
    // SAFETY: single-threaded boot; nothing else references CPU0 yet.
    unsafe {
        let tss = &mut *CPU0.tss.get();
        let df_top = DF_STACK.0.get() as u64 + DF_STACK_SIZE as u64;
        tss.ist[IST_DOUBLE_FAULT as usize - 1] = df_top;

        let gdt = &mut (*CPU0.gdt.get()).0;
        gdt[1] = KCODE64;
        gdt[2] = KDATA;
        gdt[3] = UCODE32;
        gdt[4] = UDATA;
        gdt[5] = UCODE64;
        let (lo, hi) = tss_descriptor(CPU0.tss.get() as u64, size_of::<Tss>() as u32 - 1);
        gdt[6] = lo;
        gdt[7] = hi;

        let ptr = DescriptorPtr {
            limit: (size_of::<Gdt>() - 1) as u16,
            base: gdt.as_ptr() as u64,
        };
        asm!(
            "lgdt [{ptr}]",
            // Reload CS with a far return.
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {ds:e}",
            "mov es, {ds:e}",
            "mov ss, {ds:e}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:e}",
            "mov gs, {tmp:e}",
            "ltr {tss:x}",
            ptr = in(reg) &ptr,
            cs = in(reg) KERNEL_CS as u64,
            ds = in(reg) KERNEL_DS as u64,
            tss = in(reg) TSS_SEL,
            tmp = out(reg) _,
            options(preserves_flags)
        );
    }
}
