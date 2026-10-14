//! Arch → core hooks for user-mode traps (Phase 1 contract).
//!
//! The arch layer (`arch/<arch>/`) owns the entry/exit assembly, the
//! [`arch::ctx::TrapFrame`] layout, page tables and the timer. It calls the
//! functions below; it never implements policy itself. The arch-neutral core
//! (syscall layer, address space, scheduler) replaces the bodies in this
//! file; the signatures are the contract and must not change without
//! updating both sides.
//!
//! Rules the arch code can rely on:
//! * Every hook runs on the current thread's kernel stack with the user
//!   register state in `frame` (on the same stack). Hooks may block / switch
//!   threads; when they return, the arch restores `frame` and returns to
//!   user mode, so a hook changes user state only by editing `frame`.
//! * Hooks may be entered with interrupts enabled or disabled; they must not
//!   assume either (`SpinLock` masks IRQs itself).
//! * The kernel never dereferences user virtual addresses directly: all user
//!   memory access goes through the address space (`mm`) and the direct map,
//!   so a fault in kernel mode is always a kernel bug and is fatal.
//!
//! Once the loader has created the process, syscalls go to
//! `syscall::dispatch` and user faults to the process address space
//! (`mm::vm::AddressSpace::handle_fault`). Before that (the arch self-tests
//! `patina.selftest=user|userfault|timer|switch` run without a process)
//! the minimal fallback remains: `write(1, …)` prints, `exit_group(n)`
//! exits with `n`, anything else is `ENOSYS`, user faults are fatal. The
//! tick only counts until the scheduler runs (`task::sched::run`); then it
//! also expires sleepers and accounts quanta, and reschedules happen in
//! [`before_return_to_user`].

use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::current::nr;
use patina_abi::errno;
use patina_abi::signal::{
    BUS_ADRALN, FPE_FLTINV, FPE_INTDIV, SEGV_ACCERR, SEGV_MAPERR, SIGBUS, SIGFPE, SIGILL, SIGSEGV,
    SIGTRAP, SigInfo,
};
#[cfg(target_arch = "aarch64")]
use patina_abi::signal::{ILL_ILLOPC, TRAP_BRKPT};
#[cfg(target_arch = "x86_64")]
use patina_abi::signal::{ILL_ILLOPN, TRAP_TRACE};

use crate::arch::ctx::TrapFrame;
use crate::arch::sigframe;
use crate::task::signal;

/// Timer tick rate requested from `arch::timer::init` (Hz).
pub const TICK_HZ: u64 = 100;

/// Kind of access that faulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Exec,
}

/// Why a user fault happened, as far as the arch can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// No translation (page not mapped).
    NotPresent,
    /// Mapped but the access is not permitted (`SEGV_ACCERR`).
    Permission,
    /// Misaligned access that the hardware traps (`SIGBUS`).
    Alignment,
    /// Anything else (address size fault, external abort, ...).
    Other,
}

/// A synchronous exception from user mode that is not a syscall or a page
/// fault. `user_exception` maps these to signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exception {
    /// `#UD` / undefined instruction → `SIGILL`.
    IllegalInstruction,
    /// `INT3` / `BRK` → `SIGTRAP`.
    Breakpoint,
    /// `#DE` → `SIGFPE` (x86 only).
    DivideError,
    /// FP exception trap → `SIGFPE`.
    FloatingPoint,
    /// Anything else; `code` is the arch-specific reason (ESR / vector).
    Other { code: u64 },
}

/// Number of timer ticks seen (diagnostics / self-tests).
pub static TICKS: AtomicU64 = AtomicU64::new(0);

/// Inter-processor interrupt kinds (Phase 2 SMP contract, see
/// `arch/mod.rs` `smp`). The arch delivers them to [`ipi`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiKind {
    /// Re-check the runqueue / pending signals: the target CPU runs the
    /// normal `before_return_to_user` path (or leaves its idle wait).
    Reschedule,
    /// Stop this CPU for good (exit_group / panic): mask IRQs and halt.
    Stop,
}

/// Number of IPIs received on all CPUs (diagnostics / self-tests).
pub static IPIS: AtomicU64 = AtomicU64::new(0);

/// An IPI arrived on this CPU (IRQs masked, EOI already sent by the arch,
/// `frame` is the interrupted context — user or kernel). For
/// `Reschedule` nothing needs to happen here beyond returning: a CPU that
/// was idle re-checks its runqueue, a CPU that was in user mode runs
/// [`before_return_to_user`] on the way back. `Stop` never returns.
pub fn ipi(_frame: &mut TrapFrame, kind: IpiKind) {
    IPIS.fetch_add(1, Ordering::Relaxed);
    match kind {
        IpiKind::Reschedule => crate::task::sched::ipi_reschedule(),
        // Records the CPU in the scheduler's STOPPED mask, then halts.
        IpiKind::Stop => crate::task::sched::ipi_stop(),
    }
}

/// A syscall from user mode. `frame.syscall_nr()` / `frame.syscall_args()`
/// give the request; the result goes back with `frame.set_syscall_ret()`.
pub fn syscall(frame: &mut TrapFrame) {
    if crate::task::try_process().is_some() {
        crate::syscall::dispatch(frame);
        return;
    }
    let n = frame.syscall_nr();
    let a = frame.syscall_args();
    let ret: isize = match n {
        nr::WRITE if a[0] == 1 || a[0] == 2 => {
            // Self-test only: the real core resolves user pointers through
            // the address space. Here the user page is known to be mapped
            // and readable through the direct map by the self-test.
            crate::selftest_user::user_write(a[1] as usize, a[2] as usize)
        }
        nr::EXIT_GROUP | nr::EXIT => crate::exit(a[0] as i32),
        _ => errno::Errno(errno::ENOSYS).to_ret(),
    };
    frame.set_syscall_ret(ret as u64);
}

/// A user-mode memory fault at `addr`. Returns normally once the fault has
/// been resolved (e.g. a lazily mapped page was filled in) so the arch can
/// retry the access. Otherwise the fault becomes a forced signal on the
/// current thread (SIGSEGV with `SEGV_MAPERR` when no VMA covers `addr`,
/// `SEGV_ACCERR` when one does but forbids the access; SIGBUS/`BUS_ADRALN`
/// for alignment faults), delivered by [`before_return_to_user`]. Without
/// a process (arch self-tests) the fault is fatal.
pub fn user_fault(frame: &mut TrapFrame, addr: usize, access: Access, kind: FaultKind) {
    let Some(p) = crate::task::try_process() else {
        crate::kprintln!(
            "[patina] fatal: user {:?} fault ({:?}) at {:#x}",
            access,
            kind,
            addr
        );
        frame.dump();
        crate::exit(128 + SIGSEGV)
    };
    if kind != FaultKind::Alignment && p.space.lock().handle_fault(addr, access) {
        return;
    }
    let (sig, code) = if kind == FaultKind::Alignment {
        (SIGBUS, BUS_ADRALN)
    } else if p.space.lock().find(addr).is_some() {
        (SIGSEGV, SEGV_ACCERR)
    } else {
        (SIGSEGV, SEGV_MAPERR)
    };
    signal::force_fault(
        SigInfo::fault(sig, code, addr as u64),
        Some(sigframe::fault_info(frame, addr as u64)),
    );
}

/// A non-fault, non-syscall synchronous exception from user mode: forced
/// signal as Linux sends it (SIGILL `ILL_ILLOPN` on x86_64 / `ILL_ILLOPC`
/// on aarch64 with `si_addr` = pc; SIGTRAP: x86 `int3` is `SI_KERNEL`
/// with rip after the `int3`, aarch64 `BRK` is `TRAP_BRKPT` with pc at the
/// `BRK`; x86 `#DE` SIGFPE `FPE_INTDIV` at pc; x86 `#GP` SIGSEGV and `#SS`
/// SIGBUS with `SI_KERNEL`).
pub fn user_exception(frame: &mut TrapFrame, exc: Exception) {
    if crate::task::try_process().is_none() {
        crate::kprintln!("[patina] fatal: user exception {:?}", exc);
        frame.dump();
        crate::exit(128 + SIGILL)
    }
    let pc = frame.pc();
    #[cfg(target_arch = "x86_64")]
    const ILL_CODE: i32 = ILL_ILLOPN;
    #[cfg(target_arch = "aarch64")]
    const ILL_CODE: i32 = ILL_ILLOPC;
    let info = match exc {
        Exception::IllegalInstruction => SigInfo::fault(SIGILL, ILL_CODE, pc),
        #[cfg(target_arch = "x86_64")]
        Exception::Breakpoint => SigInfo::kernel(SIGTRAP),
        #[cfg(target_arch = "aarch64")]
        Exception::Breakpoint => SigInfo::fault(SIGTRAP, TRAP_BRKPT, pc),
        Exception::DivideError => SigInfo::fault(SIGFPE, FPE_INTDIV, pc),
        Exception::FloatingPoint => SigInfo::fault(SIGFPE, FPE_FLTINV, pc),
        #[cfg(target_arch = "x86_64")]
        Exception::Other { code } => match code >> 32 {
            1 => SigInfo::fault(SIGTRAP, TRAP_TRACE, pc),
            12 => SigInfo::kernel(SIGBUS),
            13 => SigInfo::kernel(SIGSEGV),
            _ => SigInfo::fault(SIGILL, ILL_CODE, pc),
        },
        #[cfg(target_arch = "aarch64")]
        Exception::Other { .. } => SigInfo::fault(SIGILL, ILL_CODE, pc),
    };
    signal::force_fault(info, Some(sigframe::exception_info(frame)));
}

/// Timer tick (IRQs masked). `from_user` is true when the interrupt
/// arrived in user mode. The core never switches here: the tick wakes
/// expired sleepers and, when the quantum is used up, sets `need_resched`;
/// the switch happens in [`before_return_to_user`], which the arch runs
/// right after this hook for an interrupt from user mode (the kernel is not
/// preemptible, so an interrupt from kernel mode only leaves the flag).
pub fn timer_tick(_frame: &mut TrapFrame, _from_user: bool) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    crate::task::sched::tick();
}

/// Called by the arch on every return to user mode (after a syscall, fault
/// or interrupt handled in `frame`), with interrupts disabled. Runs a
/// pending reschedule (the thread may switch out here and resume later
/// with `frame` intact on its kernel stack), then signal delivery for the
/// thread about to return (`task::signal::deliver`: dequeue one
/// deliverable signal, build its `rt_sigframe` by editing `frame`, resolve
/// syscall restarts; a few loads when nothing is pending).
pub fn before_return_to_user(frame: &mut TrapFrame) {
    crate::task::sched::before_return_to_user();
    if crate::task::try_process().is_some() {
        signal::deliver(frame);
    }
}
