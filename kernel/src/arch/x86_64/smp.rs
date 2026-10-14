//! SMP: CPU enumeration, AP bring-up (INIT-SIPI-SIPI), IPIs.
//!
//! ## Enumeration ([`enumerate`], from `early_main` on the boot tables)
//!
//! First source that yields at least one enabled CPU wins:
//! 1. ACPI MADT via `hvm_start_info.rsdp_paddr`;
//! 2. ACPI MADT via an RSDP found by the classic scan (first KiB of the
//!    EBDA, then `0xE0000-0xFFFFF`; SeaBIOS on `pc`/`q35`);
//! 3. ACPI MADT from fw_cfg (`etc/acpi/rsdp` + `etc/acpi/tables`, table
//!    pointers taken as offsets into the tables blob: QEMU `microvm` with
//!    PVH boot, whose qboot firmware does not install ACPI in memory);
//! 4. MP table (`_MP_` floating pointer in the EBDA, the top KiB of base
//!    memory or `0xF0000-0xFFFFF`; Firecracker);
//! 5. fw_cfg `FW_CFG_NB_CPUS`, APIC ids assumed `0..n`;
//! 6. one CPU.
//!
//! Logical CPU 0 is the boot CPU; the others follow in table order.
//! Running this before the frame allocator exists matters: VMM e820 maps
//! may report the pages holding the ACPI tables as RAM.
//!
//! ## Bring-up ([`start_secondaries`])
//!
//! The real-mode trampoline (below) is copied to [`TRAMPOLINE_PA`] (in the
//! low 1 MiB, which boot.rs reserves and keeps direct-mapped). It switches
//! to protected mode and then to long mode on its own small page tables at
//! [`TRAMP_PML4_PA`] (identity map of the low 2 MiB + a copy of the kernel
//! PML4's upper half), jumps to [`__ap_entry64`] at its kernel VA, which
//! loads the kernel CR3 and the AP's own stack and calls [`ap_main`]. The
//! boot CPU allocates everything the AP needs (stacks, GDT/TSS, per-CPU
//! block) and starts one AP at a time (the trampoline data area is shared).

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::acpi::{self, CpuList, Mem};
use super::cpu::{self, MAX_CPUS};
use super::entry::{IPI_RESCHEDULE_VECTOR, IPI_STOP_VECTOR, IPI_TLB_FLUSH_VECTOR};
use super::gdt::{self, ApTables};
use super::lapic::{self, ICR_FIXED, ICR_INIT, ICR_LEVEL_ASSERT, ICR_STARTUP, ICR_TRIGGER_LEVEL};
use super::{PHYS_OFFSET, entry, exceptions, fwcfg, irq, paging, percpu, timer};
use crate::sync::SpinLock;
use crate::trap::IpiKind;

/// Physical page the AP trampoline is copied to (SIPI vector 0x08).
pub const TRAMPOLINE_PA: usize = 0x8000;
/// Trampoline page tables: PML4, PDPT, PD (identity map of the low 2 MiB).
pub const TRAMP_PML4_PA: usize = 0x9000;
const TRAMP_PDPT_PA: usize = 0xa000;
const TRAMP_PD_PA: usize = 0xb000;

// ---------------------------------------------------------------------------
// Enumeration.

static CPU_COUNT: AtomicUsize = AtomicUsize::new(1);
static APIC_IDS: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
static ONLINE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// Enumeration source (see `source_name`) and its table address.
static SOURCE: AtomicU8 = AtomicU8::new(0);
static SOURCE_ADDR: AtomicU64 = AtomicU64::new(0);
static RSDP_ADDR: AtomicU64 = AtomicU64::new(0);

const SRC_NONE: u8 = 0;
const SRC_MADT_START_INFO: u8 = 1;
const SRC_MADT_SCAN: u8 = 2;
const SRC_MADT_FWCFG: u8 = 3;
const SRC_MP: u8 = 4;
const SRC_FWCFG_NB_CPUS: u8 = 5;

fn source_name(s: u8) -> &'static str {
    match s {
        SRC_MADT_START_INFO => "acpi madt",
        SRC_MADT_SCAN => "acpi madt",
        SRC_MADT_FWCFG => "acpi madt in fw_cfg etc/acpi/tables",
        SRC_MP => "mp table",
        SRC_FWCFG_NB_CPUS => "fw_cfg nb_cpus, apic ids assumed 0..n",
        _ => "default, no table found",
    }
}

fn rsdp_origin(s: u8) -> &'static str {
    match s {
        SRC_MADT_START_INFO => ", rsdp from start_info",
        SRC_MADT_SCAN => ", rsdp from bios-area scan",
        SRC_MADT_FWCFG => ", rsdp from fw_cfg etc/acpi/rsdp",
        _ => "",
    }
}

/// CPUs the platform reports (>= 1, capped at `MAX_CPUS`).
pub fn cpu_count() -> usize {
    CPU_COUNT.load(Ordering::Relaxed)
}

/// APIC id of logical CPU `cpu`.
pub fn apic_id(cpu: usize) -> u32 {
    APIC_IDS[cpu].load(Ordering::Relaxed)
}

/// Whether `cpu` has been started (the boot CPU always is).
pub fn is_online(cpu: usize) -> bool {
    cpu < MAX_CPUS && ONLINE[cpu].load(Ordering::Acquire)
}

/// Read a little-endian u16 from low physical memory (boot tables).
fn phys_u16(pa: u64) -> u16 {
    let mut b = [0u8; 2];
    if acpi::Phys.read(pa, &mut b) {
        u16::from_le_bytes(b)
    } else {
        0
    }
}

/// EBDA base from the BDA word at 0x40E, if plausible.
fn ebda() -> Option<u64> {
    let e = (phys_u16(0x40e) as u64) << 4;
    (0x8_0000..0xa_0000).contains(&e).then_some(e)
}

/// Size of the fw_cfg ACPI blob buffer (QEMU pads `etc/acpi/tables` on
/// pc/q35 to 128 KiB, but the tables themselves are much smaller).
const FWCFG_BUF: usize = 64 * 1024;

struct FwBuf(core::cell::UnsafeCell<[u8; FWCFG_BUF]>);
// SAFETY: only used by the boot CPU during `enumerate`.
unsafe impl Sync for FwBuf {}
static FW_TABLES: FwBuf = FwBuf(core::cell::UnsafeCell::new([0; FWCFG_BUF]));

fn try_fwcfg_madt(out: &mut CpuList) -> Option<u64> {
    if !fwcfg::present() {
        return None;
    }
    let (rsel, rsize) = fwcfg::find_file("etc/acpi/rsdp")?;
    let (tsel, tsize) = fwcfg::find_file("etc/acpi/tables")?;
    // SAFETY: single-threaded boot; nothing else uses the buffer.
    let buf = unsafe { &mut *FW_TABLES.0.get() };
    // Tables first (truncated if larger than the buffer: the padding at
    // the end of QEMU's blob is not needed), then the RSDP right after
    // them so the parser can reach it by "address" in the same space.
    let n = fwcfg::read_item(tsel, tsize, 0, &mut buf[..FWCFG_BUF - 64]);
    let rn = fwcfg::read_item(rsel, rsize, 0, &mut buf[n..n + 36]);
    let blob = acpi::Blob(&buf[..n + rn]);
    acpi::madt_cpus(&blob, n as u64, out)
}

/// Enumerate the CPUs (see the module docs). Runs once on the boot CPU,
/// on the boot page tables (low 4 GiB direct-mapped), console up.
///
/// `skip` (kernel parameter `patina.smp.skip=N`, a test knob) skips the
/// first N sources of the chain, to exercise the later ones.
pub(super) fn enumerate(start_info_rsdp: usize, skip: u8) {
    let mut list = CpuList::new();
    let mut src = SRC_NONE;
    let mut addr = 0u64;
    let want = |src: u8, s: u8| src == SRC_NONE && s > skip;

    if want(src, SRC_MADT_START_INFO)
        && start_info_rsdp != 0
        && let Some(m) = acpi::madt_cpus(&acpi::Phys, start_info_rsdp as u64, &mut list)
    {
        (src, addr) = (SRC_MADT_START_INFO, m);
        RSDP_ADDR.store(start_info_rsdp as u64, Ordering::Relaxed);
    }
    if want(src, SRC_MADT_SCAN) {
        list.clear();
        let rsdp = ebda()
            .and_then(|e| acpi::scan_rsdp(e, e + 1024))
            .or_else(|| acpi::scan_rsdp(0xe_0000, 0x10_0000));
        if let Some(r) = rsdp
            && let Some(m) = acpi::madt_cpus(&acpi::Phys, r, &mut list)
        {
            (src, addr) = (SRC_MADT_SCAN, m);
            RSDP_ADDR.store(r, Ordering::Relaxed);
        }
    }
    if want(src, SRC_MADT_FWCFG) {
        list.clear();
        if let Some(m) = try_fwcfg_madt(&mut list) {
            (src, addr) = (SRC_MADT_FWCFG, m);
        }
    }
    if want(src, SRC_MP) {
        list.clear();
        let base_top = (phys_u16(0x413) as u64) << 10;
        let mp = ebda()
            .and_then(|e| acpi::scan_mp(e, e + 1024))
            .or_else(|| {
                (0x8_0000..=0xa_0000)
                    .contains(&base_top)
                    .then(|| acpi::scan_mp(base_top - 1024, base_top))
                    .flatten()
            })
            .or_else(|| acpi::scan_mp(0x9_fc00, 0xa_0000))
            .or_else(|| acpi::scan_mp(0xf_0000, 0x10_0000));
        if let Some((_, cfg)) = mp
            && acpi::mp_cpus(cfg, &mut list)
        {
            (src, addr) = (SRC_MP, cfg);
        }
    }
    if want(src, SRC_FWCFG_NB_CPUS) && fwcfg::present() {
        list.clear();
        let n = fwcfg::read_u16(fwcfg::SEL_NB_CPUS) as usize;
        if n >= 1 {
            for id in 0..n.min(MAX_CPUS) {
                list.ids[id] = id as u32;
            }
            list.n = n.min(MAX_CPUS);
            list.dropped = n.saturating_sub(MAX_CPUS);
            src = SRC_FWCFG_NB_CPUS;
        }
    }

    // CPU 0 = the boot CPU, then the others in table order.
    let bsp = cpu::apic_id();
    let mut n = 1;
    APIC_IDS[0].store(bsp, Ordering::Relaxed);
    for &id in &list.ids[..list.n] {
        if id != bsp && n < MAX_CPUS {
            APIC_IDS[n].store(id, Ordering::Relaxed);
            n += 1;
        }
    }
    ONLINE[0].store(true, Ordering::Release);
    CPU_COUNT.store(n, Ordering::Relaxed);
    SOURCE.store(src, Ordering::Relaxed);
    SOURCE_ADDR.store(addr, Ordering::Relaxed);
    crate::kprintln!(
        "[patina] smp: {} cpu{} ({}{}{}{}), boot cpu apic id {}{}{}",
        n,
        if n == 1 { "" } else { "s" },
        source_name(src),
        alloc_free_hex(addr),
        rsdp_origin(src),
        alloc_free_hex(RSDP_ADDR.load(Ordering::Relaxed)),
        bsp,
        if list.disabled > 0 {
            ", some entries disabled"
        } else {
            ""
        },
        if list.dropped > 0 {
            ", cpus beyond MAX_CPUS ignored"
        } else {
            ""
        },
    );
}

/// " @ 0x..." without the heap (enumeration runs before it exists).
struct HexBuf([u8; 24], usize);

impl HexBuf {
    fn empty() -> Self {
        HexBuf([0; 24], 0)
    }
}

impl core::fmt::Display for HexBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Only ASCII is ever written.
        f.write_str(core::str::from_utf8(&self.0[..self.1]).unwrap_or(""))
    }
}

fn alloc_free_hex(v: u64) -> HexBuf {
    let mut b = HexBuf::empty();
    if v == 0 {
        return b;
    }
    let prefix = b" @ 0x";
    b.0[..prefix.len()].copy_from_slice(prefix);
    let mut i = prefix.len();
    let digits = (64 - v.leading_zeros()).div_ceil(4).max(1);
    for d in (0..digits).rev() {
        b.0[i] = b"0123456789abcdef"[((v >> (d * 4)) & 0xf) as usize];
        i += 1;
    }
    b.1 = i;
    b
}

// ---------------------------------------------------------------------------
// Trampoline.

/// CR4 for the AP's long-mode switch: PAE | PGE | OSFXSR | OSXMMEXCPT.
const AP_CR4: u32 = (1 << 5) | (1 << 7) | (1 << 9) | (1 << 10);
/// CR0 bits set: PE | MP | NE | WP | PG; cleared: EM | TS | NW | CD.
const AP_CR0_SET: u32 = (1 << 0) | (1 << 1) | (1 << 5) | (1 << 16) | (1 << 31);
const AP_CR0_CLEAR: u32 = (1 << 2) | (1 << 3) | (1 << 29) | (1 << 30);
/// EFER: LME | NXE (the kernel tables use NX).
const AP_EFER: u32 = (1 << 8) | (1 << 11);

global_asm!(
    r#"
    .set TRAMP, {tramp}

    // Copied to TRAMP and entered by SIPI in real mode at CS = TRAMP >> 4,
    // IP = 0. Everything is addressed relative to the start of the copy.
    .section .rodata.ap_trampoline, "a"
    .balign 16
    .global __ap_tramp_start
__ap_tramp_start:
    .code16
    cli
    cld
    movw    %cs, %ax
    movw    %ax, %ds
    movl    $1, tr_started - __ap_tramp_start
    lgdtl   tr_gdtr - __ap_tramp_start
    movl    %cr0, %eax
    orl     $1, %eax
    movl    %eax, %cr0
    ljmpl   $0x08, $(TRAMP + tr32 - __ap_tramp_start)

    .code32
tr32:
    movw    $0x10, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movl    %cr4, %eax
    orl     ${cr4}, %eax
    movl    %eax, %cr4
    movl    ${pml4}, %eax
    movl    %eax, %cr3
    movl    ${efer_msr}, %ecx
    rdmsr
    orl     ${efer}, %eax
    wrmsr
    movl    %cr0, %eax
    andl    ${cr0_clear_inv}, %eax
    orl     ${cr0_set}, %eax
    movl    %eax, %cr0
    ljmpl   $0x18, $(TRAMP + tr64 - __ap_tramp_start)

    .code64
tr64:
    movl    $(TRAMP + tr_data - __ap_tramp_start), %esi
    movq    0(%rsi), %rbx               // kernel CR3
    movq    8(%rsi), %rsp               // this AP's kernel stack top
    movq    16(%rsi), %rax              // __ap_entry64 (kernel VA)
    movq    24(%rsi), %rdi              // cpu index
    jmpq    *%rax

    .balign 8
tr_gdt:
    .quad   0
    .quad   0x00cf9a000000ffff          // 0x08: code32
    .quad   0x00cf92000000ffff          // 0x10: data
    .quad   0x00af9a000000ffff          // 0x18: code64
tr_gdt_end:
    .balign 4
    .word   0
tr_gdtr:
    .word   tr_gdt_end - tr_gdt - 1
    .long   TRAMP + tr_gdt - __ap_tramp_start
    .balign 8
    .global __ap_tramp_data
__ap_tramp_data:
tr_data:
    .quad   0                           // +0  kernel CR3
    .quad   0                           // +8  stack top
    .quad   0                           // +16 entry
    .quad   0                           // +24 cpu index
tr_started:
    .long   0                           // +32 set by the AP in real mode
    .long   0
    .global __ap_tramp_end
__ap_tramp_end:

    // Kernel-VA continuation, still on the trampoline tables (whose upper
    // half is the kernel's): switch to the kernel tables, call ap_main.
    .section .text.ap_entry, "ax"
    .global __ap_entry64
__ap_entry64:
    movq    %rbx, %cr3
    xorl    %ebp, %ebp
    call    {ap_main}
    ud2
"#,
    tramp = const TRAMPOLINE_PA,
    pml4 = const TRAMP_PML4_PA,
    cr4 = const AP_CR4,
    efer_msr = const cpu::MSR_EFER,
    efer = const AP_EFER,
    cr0_set = const AP_CR0_SET,
    cr0_clear_inv = const !AP_CR0_CLEAR,
    ap_main = sym ap_main,
    options(att_syntax)
);

unsafe extern "C" {
    static __ap_tramp_start: u8;
    static __ap_tramp_data: u8;
    static __ap_tramp_end: u8;
    fn __ap_entry64();
}

/// Offsets in the trampoline data area.
const D_CR3: usize = 0;
const D_STACK: usize = 8;
const D_ENTRY: usize = 16;
const D_CPU: usize = 24;
const D_STARTED: usize = 32;

/// What the boot CPU prepared for one AP.
struct ApBoot {
    tables: Option<ApTables>,
    percpu: usize,
    stack_top: usize,
}

static AP_BOOT: SpinLock<ApBoot> = SpinLock::new(ApBoot {
    tables: None,
    percpu: 0,
    stack_top: 0,
});
/// `entry` passed to `start_secondaries`.
static AP_ENTRY: AtomicUsize = AtomicUsize::new(0);

fn low_ptr(pa: usize) -> *mut u8 {
    (PHYS_OFFSET + pa) as *mut u8
}

/// Copy the trampoline to low memory and build its page tables.
fn install_trampoline() {
    let start = &raw const __ap_tramp_start as usize;
    let end = &raw const __ap_tramp_end as usize;
    assert!(end - start <= 4096, "AP trampoline larger than a page");
    // SAFETY: the low 1 MiB is reserved for this (boot.rs) and
    // direct-mapped RW; nothing else uses these four pages.
    unsafe {
        core::ptr::copy_nonoverlapping(start as *const u8, low_ptr(TRAMPOLINE_PA), end - start);
        for pa in [TRAMP_PML4_PA, TRAMP_PDPT_PA, TRAMP_PD_PA] {
            core::ptr::write_bytes(low_ptr(pa), 0, 4096);
        }
        let pml4 = low_ptr(TRAMP_PML4_PA).cast::<u64>();
        let kroot = (PHYS_OFFSET + paging::kernel_root().0) as *const u64;
        // P | RW tables; the identity leaf is a 2 MiB P | RW | PS page
        // (executable: no NX).
        pml4.write_volatile(TRAMP_PDPT_PA as u64 | 0x3);
        low_ptr(TRAMP_PDPT_PA)
            .cast::<u64>()
            .write_volatile(TRAMP_PD_PA as u64 | 0x3);
        low_ptr(TRAMP_PD_PA).cast::<u64>().write_volatile(0x83);
        // Kernel half: the kernel PML4's upper 256 entries (all present
        // and never changed after boot, see `paging::populate_kernel_pml4`).
        for i in 256..512 {
            pml4.add(i).write_volatile(kroot.add(i).read_volatile());
        }
    }
}

fn data_ptr(off: usize) -> *mut u8 {
    let d = &raw const __ap_tramp_data as usize - &raw const __ap_tramp_start as usize;
    low_ptr(TRAMPOLINE_PA + d + off)
}

/// Busy-wait `ns` nanoseconds.
fn delay_ns(ns: u64) {
    let t0 = timer::now_ns();
    while timer::now_ns() - t0 < ns {
        core::hint::spin_loop();
    }
}

fn send_icr(dest: u32, low: u32) {
    let f = irq::save_disable();
    lapic::send_icr(dest, low);
    irq::restore(f);
}

/// Start every secondary CPU; each runs `entry(cpu)` with IRQs masked.
/// Returns how many started.
pub fn start_secondaries(entry: fn(usize) -> !) -> usize {
    let n = cpu_count();
    if n <= 1 {
        return 0;
    }
    AP_ENTRY.store(entry as usize, Ordering::Release);
    // The BSP's LAPIC must be up for the ICR (normally already done by
    // timer::init).
    lapic::init(super::entry::SPURIOUS_VECTOR);
    install_trampoline();
    let kernel_cr3 = paging::kernel_root().0 as u64;
    let sipi_vector = (TRAMPOLINE_PA >> 12) as u32;
    let mut started = 0;
    #[allow(clippy::needless_range_loop)] // `cpu` is also the CPU index
    for cpu in 1..n {
        let id = apic_id(cpu);
        if !lapic::is_x2apic() && id > 0xfe {
            crate::kprintln!(
                "[patina] smp: cpu {} (apic id {}) not addressable in xapic mode, skipped",
                cpu,
                id
            );
            continue;
        }
        let stack = crate::task::kstack::alloc().expect("smp: no memory for an AP stack");
        let df = crate::task::kstack::alloc().expect("smp: no memory for an AP IST stack");
        {
            let mut b = AP_BOOT.lock();
            b.tables = Some(gdt::alloc_ap(df.top as u64, stack.top as u64));
            b.percpu = percpu::alloc_ap() as usize;
            b.stack_top = stack.top;
        }
        // SAFETY: trampoline data area in the reserved low page.
        unsafe {
            data_ptr(D_CR3).cast::<u64>().write_volatile(kernel_cr3);
            data_ptr(D_STACK)
                .cast::<u64>()
                .write_volatile(stack.top as u64);
            data_ptr(D_ENTRY)
                .cast::<u64>()
                .write_volatile(__ap_entry64 as *const () as u64);
            data_ptr(D_CPU).cast::<u64>().write_volatile(cpu as u64);
            data_ptr(D_STARTED).cast::<u32>().write_volatile(0);
        }
        core::sync::atomic::fence(Ordering::SeqCst);

        // INIT (assert, then deassert for old xAPICs), 10 ms, SIPI x2.
        send_icr(id, ICR_INIT | ICR_LEVEL_ASSERT | ICR_TRIGGER_LEVEL);
        delay_ns(10_000_000);
        if !lapic::is_x2apic() {
            send_icr(id, ICR_INIT | ICR_TRIGGER_LEVEL);
        }
        let t0 = timer::now_ns();
        for _ in 0..2 {
            send_icr(id, ICR_STARTUP | sipi_vector);
            delay_ns(200_000);
            if ONLINE[cpu].load(Ordering::Acquire) {
                break;
            }
        }
        while !ONLINE[cpu].load(Ordering::Acquire) && timer::now_ns() - t0 < 1_000_000_000 {
            core::hint::spin_loop();
        }
        if ONLINE[cpu].load(Ordering::Acquire) {
            started += 1;
        } else {
            // SAFETY: as above.
            let reached = unsafe { data_ptr(D_STARTED).cast::<u32>().read_volatile() } != 0;
            crate::kprintln!(
                "[patina] smp: cpu {} (apic id {}) did not come online within 1 s ({})",
                cpu,
                id,
                if reached {
                    "trampoline reached"
                } else {
                    "trampoline not reached"
                }
            );
            // It may still be using the trampoline: do not reuse it.
            break;
        }
    }
    started
}

/// Rust entry of a secondary CPU: on its own stack, kernel CR3, IRQs
/// masked, temporary trampoline GDT.
extern "C" fn ap_main(cpu: usize) -> ! {
    let (tables, pc, stack_top) = {
        // Plain spin: GS is not set up yet. AP_BOOT's SpinLock does not use
        // `cpu::id()`, and the BSP is spinning on ONLINE without holding it.
        let mut b = AP_BOOT.lock();
        (
            b.tables.take().expect("smp: AP tables"),
            b.percpu,
            b.stack_top,
        )
    };
    let tss = gdt::load_ap(tables);
    // SAFETY: allocated for this CPU by start_secondaries.
    unsafe { percpu::init_ap(pc as *mut percpu::PerCpu, cpu, tss) };
    cpu::set_kernel_stack(stack_top);
    exceptions::load();
    cpu::init_features();
    entry::init_syscall();
    timer::init_cpu(); // also enables this CPU's LAPIC
    ONLINE[cpu].store(true, Ordering::Release);
    // SAFETY: stored by start_secondaries before the SIPI.
    let entry: fn(usize) -> ! =
        unsafe { core::mem::transmute::<usize, fn(usize) -> !>(AP_ENTRY.load(Ordering::Acquire)) };
    entry(cpu)
}

// ---------------------------------------------------------------------------
// IPIs.

/// Send `kind` to `cpu` (fixed delivery, physical destination). Safe with
/// IRQs masked and spinlocks held; never allocates or blocks (it spins on
/// the xAPIC delivery-status bit, bounded).
pub fn send_ipi(cpu: usize, kind: IpiKind) {
    if !is_online(cpu) {
        return;
    }
    let vector = match kind {
        IpiKind::Reschedule => IPI_RESCHEDULE_VECTOR,
        IpiKind::Stop => IPI_STOP_VECTOR,
        IpiKind::TlbFlush => IPI_TLB_FLUSH_VECTOR,
    };
    send_icr(apic_id(cpu), ICR_FIXED | ICR_LEVEL_ASSERT | vector as u32);
}

/// `send_ipi` to every other online CPU.
pub fn send_ipi_others(kind: IpiKind) {
    let me = cpu::id();
    for c in 0..cpu_count() {
        if c != me {
            send_ipi(c, kind);
        }
    }
}

// ---------------------------------------------------------------------------
// Self-test (`patina.selftest=smp`).

static GO: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static AP_SEEN: AtomicUsize = AtomicUsize::new(0);
/// The boot CPU's (CR4, EFER); every AP must match.
static BSP_CR4: AtomicU64 = AtomicU64::new(0);
static BSP_EFER: AtomicU64 = AtomicU64::new(0);
static AP_MISMATCH: AtomicUsize = AtomicUsize::new(0);

fn selftest_ap(cpu: usize) -> ! {
    crate::kprintln!(
        "[patina] smp: cpu {} online (apic id {})",
        cpu::id(),
        cpu::apic_id()
    );
    assert_eq!(cpu::id(), cpu, "cpu::id() wrong on an AP");
    // SAFETY: EFER exists on every x86_64 CPU.
    let efer = unsafe { cpu::rdmsr(cpu::MSR_EFER) };
    if cpu::read_cr4() != BSP_CR4.load(Ordering::Relaxed)
        || efer != BSP_EFER.load(Ordering::Relaxed)
    {
        crate::kprintln!(
            "[patina] smp: cpu {} cr4 {:#x} efer {:#x} differ from the boot cpu's",
            cpu,
            cpu::read_cr4(),
            efer
        );
        AP_MISMATCH.fetch_add(1, Ordering::Relaxed);
    }
    AP_SEEN.fetch_add(1, Ordering::Release);
    irq::enable();
    // Halt between interrupts (ticks, IPIs) rather than spin: under TCG a
    // spinning vCPU steals time from the others.
    while !GO[cpu].load(Ordering::Acquire) {
        cpu::wait_for_interrupt();
    }
    loop {
        cpu::wait_for_interrupt();
    }
}

/// `patina.selftest=smp`: start the APs, IPI them, check their ticks, a
/// sub-tick deadline on the boot CPU, then stop them. Exits 0.
pub fn selftest() -> ! {
    use crate::trap;
    let n = cpu_count();
    crate::kprintln!(
        "[patina] selftest smp: {} cpus ({}{})",
        n,
        source_name(SOURCE.load(Ordering::Relaxed)),
        alloc_free_hex(SOURCE_ADDR.load(Ordering::Relaxed))
    );
    timer::init();
    BSP_CR4.store(cpu::read_cr4(), Ordering::Relaxed);
    // SAFETY: EFER exists on every x86_64 CPU.
    BSP_EFER.store(unsafe { cpu::rdmsr(cpu::MSR_EFER) }, Ordering::Relaxed);
    let started = start_secondaries(selftest_ap);
    assert_eq!(started, n - 1, "selftest smp: not every AP started");
    let t0 = timer::now_ns();
    while AP_SEEN.load(Ordering::Acquire) < started {
        assert!(
            timer::now_ns() - t0 < 1_000_000_000,
            "selftest smp: APs did not reach entry"
        );
        core::hint::spin_loop();
    }

    assert_eq!(
        AP_MISMATCH.load(Ordering::Relaxed),
        0,
        "selftest smp: AP control registers differ"
    );

    // Reschedule IPIs: each AP must take one.
    let ipis0 = trap::IPIS.load(Ordering::Relaxed);
    for c in 1..n {
        send_ipi(c, IpiKind::Reschedule);
    }
    let t0 = timer::now_ns();
    let want = ipis0 + started as u64;
    while trap::IPIS.load(Ordering::Relaxed) < want {
        assert!(
            timer::now_ns() - t0 < 100_000_000,
            "selftest smp: only {} of {} IPIs arrived within 100 ms",
            trap::IPIS.load(Ordering::Relaxed) - ipis0,
            started
        );
        core::hint::spin_loop();
    }
    let ipi_us = (timer::now_ns() - t0) / 1000;
    let ipis = trap::IPIS.load(Ordering::Relaxed) - ipis0;

    // Every AP's tick runs: at least 3 ticks within 200 ms (100 Hz).
    let before: [u64; MAX_CPUS] = core::array::from_fn(|c| timer::cpu_stats(c).0);
    let t0 = timer::now_ns();
    for (c, &b) in before.iter().enumerate().take(n).skip(1) {
        while timer::cpu_stats(c).0 < b + 3 {
            assert!(
                timer::now_ns() - t0 < 2_000_000_000,
                "selftest smp: cpu {} tick not running ({} ticks)",
                c,
                timer::cpu_stats(c).0
            );
            core::hint::spin_loop();
        }
    }

    // Deadline on the boot CPU: 2 ms ahead must fire within 3 ms (the tick
    // period is 10 ms), before the next periodic tick. Each attempt syncs
    // to just after a tick so the tick is not what wakes us. Up to 5
    // attempts, the best must be <= 3 ms: under TCG the host's timer
    // granularity (macOS QEMU rounds poll timeouts to 1 ms) occasionally
    // delays an interrupt by more than 1 ms.
    let mut best = u64::MAX;
    let mut worst = 0;
    for _ in 0..5 {
        let ticks = timer::cpu_stats(0).0;
        while timer::cpu_stats(0).0 == ticks {
            cpu::wait_for_interrupt();
        }
        let fires0 = timer::cpu_stats(0).1;
        let ticks0 = timer::cpu_stats(0).0;
        let set = timer::now_ns();
        timer::set_deadline(Some(set + 2_000_000));
        while timer::cpu_stats(0).1 == fires0 {
            cpu::wait_for_interrupt();
        }
        let took = timer::now_ns() - set;
        assert!(
            took >= 2_000_000,
            "selftest smp: 2 ms deadline fired early, after {} us",
            took / 1000
        );
        assert_eq!(
            timer::cpu_stats(0).0,
            ticks0,
            "selftest smp: a periodic tick, not the deadline, woke the CPU ({} us)",
            took / 1000
        );
        best = best.min(took);
        worst = worst.max(took);
        if best <= 3_000_000 {
            break;
        }
    }
    let deadline_us = best / 1000;
    assert!(
        best <= 3_000_000,
        "selftest smp: 2 ms deadline took at best {} us (worst {} us)",
        deadline_us,
        worst / 1000
    );
    // Cancel: a deadline 2 ms ahead, cancelled, must not fire.
    let fires1 = timer::cpu_stats(0).1;
    timer::set_deadline(Some(timer::now_ns() + 2_000_000));
    timer::set_deadline(None);
    let t0 = timer::now_ns();
    while timer::now_ns() - t0 < 5_000_000 {
        cpu::wait_for_interrupt();
    }
    assert_eq!(
        timer::cpu_stats(0).1,
        fires1,
        "selftest smp: cancelled deadline fired"
    );

    // Stop the APs.
    for go in &GO[1..n] {
        go.store(true, Ordering::Release);
    }
    let stops0 = trap::IPIS.load(Ordering::Relaxed);
    send_ipi_others(IpiKind::Stop);
    let t0 = timer::now_ns();
    while trap::IPIS.load(Ordering::Relaxed) < stops0 + started as u64 {
        assert!(
            timer::now_ns() - t0 < 100_000_000,
            "selftest smp: Stop IPIs not taken"
        );
        core::hint::spin_loop();
    }
    let total = trap::IPIS.load(Ordering::Relaxed) - ipis0;
    crate::kprintln!(
        "[patina] selftest smp: reschedule ipis {} in {} us, deadline irq {} us late, ap ticks {:?}",
        ipis,
        ipi_us,
        timer::last_deadline_lateness(0) / 1000,
        (1..n)
            .map(|c| timer::cpu_stats(c).0)
            .collect::<alloc::vec::Vec<_>>()
    );
    crate::kprintln!(
        "[patina] selftest smp: ok ({} cpus, {} ipis, deadline {} us)",
        n,
        total,
        deadline_us
    );
    crate::exit(0)
}
