//! Per-CPU block, reached through GS in kernel mode.
//!
//! While a CPU runs kernel code, `GS_BASE` points at its [`PerCpu`] and
//! `KERNEL_GS_BASE` holds the user GS base; every entry from ring 3
//! (SYSCALL, interrupt or exception with CS.RPL = 3) starts with `swapgs` and
//! every return to ring 3 ends with one. Kernel-mode entries do not swap.
//! The entry asm uses the fixed offsets below (`gs:OFF_*`).
//!
//! The boot CPU's block is a static; each secondary CPU allocates its own
//! (`alloc_ap` on the boot CPU, `init_ap` from `smp::ap_main`).

use core::cell::UnsafeCell;
use core::mem::offset_of;

use super::cpu::{self, MSR_GS_BASE, MSR_KERNEL_GS_BASE};
use super::gdt::{self, Tss};

#[repr(C, align(64))]
pub struct PerCpu {
    /// Address of this block (so Rust can get a pointer with one `gs:` load).
    pub self_ptr: u64,
    /// Kernel stack top for SYSCALL entry (mirrors `tss.rsp[0]`).
    pub kernel_stack_top: u64,
    /// User RSP, parked here by the SYSCALL entry until the frame is built.
    pub user_rsp_scratch: u64,
    /// Logical CPU index.
    pub cpu_id: u64,
    /// This CPU's TSS.
    pub tss: *mut Tss,
}

pub const OFF_SELF: usize = offset_of!(PerCpu, self_ptr);
pub const OFF_KSTACK: usize = offset_of!(PerCpu, kernel_stack_top);
pub const OFF_USER_RSP: usize = offset_of!(PerCpu, user_rsp_scratch);
pub const OFF_CPU_ID: usize = offset_of!(PerCpu, cpu_id);

struct Cell(UnsafeCell<PerCpu>);
// SAFETY: each CPU only touches its own block (through GS).
unsafe impl Sync for Cell {}

static BOOT_CPU: Cell = Cell(UnsafeCell::new(PerCpu {
    self_ptr: 0,
    kernel_stack_top: 0,
    user_rsp_scratch: 0,
    cpu_id: 0,
    tss: core::ptr::null_mut(),
}));

/// Point GS at `pc` on the current CPU (kernel GS live, user GS base 0).
///
/// # Safety
/// `pc` must stay valid forever and belong to this CPU only.
unsafe fn init(pc: *mut PerCpu, cpu_id: u64, tss: *mut Tss) {
    // SAFETY: caller guarantees exclusive, 'static access.
    unsafe {
        (*pc).self_ptr = pc as u64;
        (*pc).cpu_id = cpu_id;
        (*pc).tss = tss;
        cpu::wrmsr(MSR_GS_BASE, pc as u64);
        cpu::wrmsr(MSR_KERNEL_GS_BASE, 0);
    }
}

/// Set up the boot CPU's block. Must run after `gdt::init` (loading the GS
/// selector clears GS_BASE) and before anything calls `cpu::id()`.
pub(super) fn init_boot_cpu() {
    // SAFETY: single-threaded boot; BOOT_CPU is a static used only by CPU 0.
    unsafe { init(BOOT_CPU.0.get(), 0, gdt::boot_tss()) };
}

/// Allocate (and leak: CPUs never go away) a block for a secondary CPU.
/// Called by the boot CPU; the AP then calls [`init_ap`] on it.
pub(super) fn alloc_ap() -> *mut PerCpu {
    alloc::boxed::Box::leak(alloc::boxed::Box::new(PerCpu {
        self_ptr: 0,
        kernel_stack_top: 0,
        user_rsp_scratch: 0,
        cpu_id: 0,
        tss: core::ptr::null_mut(),
    }))
}

/// On secondary CPU `cpu_id`: point GS at `pc` (from [`alloc_ap`]). Must
/// run after `gdt::load_ap` (loading the GS selector clears GS_BASE).
///
/// # Safety
/// `pc` must come from `alloc_ap` and be used by this CPU only.
pub(super) unsafe fn init_ap(pc: *mut PerCpu, cpu_id: usize, tss: *mut Tss) {
    // SAFETY: caller's guarantee.
    unsafe { init(pc, cpu_id as u64, tss) };
}

/// This CPU's block.
#[inline]
pub fn current() -> *mut PerCpu {
    let p: u64;
    // SAFETY: GS_BASE points at this CPU's PerCpu in kernel mode.
    unsafe {
        core::arch::asm!("mov {}, gs:[{off}]", out(reg) p, off = const OFF_SELF,
                         options(nostack, readonly, preserves_flags))
    };
    p as *mut PerCpu
}

#[inline]
pub fn cpu_id() -> usize {
    let v: u64;
    // SAFETY: GS_BASE points at this CPU's PerCpu in kernel mode.
    unsafe {
        core::arch::asm!("mov {}, gs:[{off}]", out(reg) v, off = const OFF_CPU_ID,
                         options(nostack, readonly, preserves_flags))
    };
    v as usize
}

/// Kernel stack for entries from ring 3: SYSCALL (`kernel_stack_top`) and
/// interrupts/exceptions (`TSS.rsp0`).
pub(super) fn set_kernel_stack(top: u64) {
    let pc = current();
    // SAFETY: this CPU's block and TSS; the caller keeps IRQs off or no
    // entry from user mode can happen concurrently on this CPU. The TSS is
    // packed, so rsp0 is written unaligned.
    unsafe {
        (*pc).kernel_stack_top = top;
        let tss = (*pc).tss;
        core::ptr::addr_of_mut!((*tss).rsp)
            .cast::<u64>()
            .write_unaligned(top);
    }
}
