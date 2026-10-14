//! Per-CPU GDT and TSS (a static for the boot CPU, heap for the others).
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
//! Secondary CPUs get the same layout in their own tables ([`alloc_ap`]),
//! with a guarded kernel stack (`task::kstack`) as their IST1 stack.

use alloc::boxed::Box;
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

/// One CPU's GDT and TSS.
#[repr(C, align(16))]
struct CpuTables {
    gdt: Gdt,
    tss: Tss,
}

impl CpuTables {
    const fn new() -> Self {
        Self {
            gdt: Gdt([0; GDT_ENTRIES]),
            tss: Tss {
                _reserved0: 0,
                rsp: [0; 3],
                _reserved1: 0,
                ist: [0; 7],
                _reserved2: 0,
                _reserved3: 0,
                iomap_base: size_of::<Tss>() as u16, // no I/O bitmap
            },
        }
    }
}

struct Cpu0(UnsafeCell<CpuTables>);

// SAFETY: written once by the boot CPU before anything else can run; later
// accessed only by the CPU itself (ltr sets the busy bit).
unsafe impl Sync for Cpu0 {}

static CPU0: Cpu0 = Cpu0(UnsafeCell::new(CpuTables::new()));

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
pub(super) fn boot_tss() -> *mut Tss {
    // SAFETY: only the address is taken.
    unsafe { &raw mut (*CPU0.0.get()).tss }
}

#[repr(C, packed)]
struct DescriptorPtr {
    limit: u16,
    base: u64,
}

/// Build and load the GDT and TSS on the boot CPU; reload all segment
/// registers. Called once, early, while still on the boot tables.
pub(super) fn init() {
    let df_top = DF_STACK.0.get() as u64 + DF_STACK_SIZE as u64;
    // SAFETY: single-threaded boot; nothing else references CPU0 yet.
    unsafe { load(CPU0.0.get(), df_top) };
}

/// A secondary CPU's GDT + TSS, allocated by the boot CPU ([`alloc_ap`])
/// and loaded by the AP itself ([`load_ap`]).
pub(super) struct ApTables(*mut CpuTables, u64);

// SAFETY: handed from the boot CPU to the one AP that uses it.
unsafe impl Send for ApTables {}

/// Allocate (and leak: CPUs never go away) a secondary CPU's GDT + TSS
/// with IST1 = `df_stack_top` and `rsp0` = `rsp0`.
pub(super) fn alloc_ap(df_stack_top: u64, rsp0: u64) -> ApTables {
    let t: *mut CpuTables = Box::leak(Box::new(CpuTables::new()));
    // SAFETY: freshly allocated; the TSS is packed, so write unaligned.
    unsafe {
        core::ptr::addr_of_mut!((*t).tss.rsp)
            .cast::<u64>()
            .write_unaligned(rsp0);
    }
    ApTables(t, df_stack_top)
}

/// On the secondary CPU: load its tables and reload all segment registers
/// (this clears GS_BASE: the per-CPU block is set up afterwards). Returns
/// the TSS.
pub(super) fn load_ap(t: ApTables) -> *mut Tss {
    // SAFETY: allocated by `alloc_ap`, lives forever, used by this CPU only.
    unsafe {
        load(t.0, t.1);
        &raw mut (*t.0).tss
    }
}

/// Fill `t` (selectors per the table above, TSS IST1 = `df_top`) and load
/// it on this CPU.
///
/// # Safety
/// `t` must live forever and be used by this CPU only.
unsafe fn load(t: *mut CpuTables, df_top: u64) {
    // SAFETY: caller guarantees exclusive, 'static access.
    unsafe {
        let tss = &raw mut (*t).tss;
        core::ptr::addr_of_mut!((*tss).ist)
            .cast::<u64>()
            .add(IST_DOUBLE_FAULT as usize - 1)
            .write_unaligned(df_top);

        let gdt = &mut (*t).gdt.0;
        gdt[1] = KCODE64;
        gdt[2] = KDATA;
        gdt[3] = UCODE32;
        gdt[4] = UDATA;
        gdt[5] = UCODE64;
        let (lo, hi) = tss_descriptor(tss as u64, size_of::<Tss>() as u32 - 1);
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
