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
//! Until the core lands, the bodies here are just enough for the arch
//! self-tests (`patina.selftest=user|userfault|timer|switch`): `write(1, …)`
//! prints, `exit_group(n)` exits with `n`, anything else is `ENOSYS`, user
//! faults are fatal, the tick only counts.

use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::current::nr;
use patina_abi::errno;

use crate::arch::ctx::TrapFrame;

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
/// fault. Phase 2 maps these to signals.
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

/// A syscall from user mode. `frame.syscall_nr()` / `frame.syscall_args()`
/// give the request; the result goes back with `frame.set_syscall_ret()`.
pub fn syscall(frame: &mut TrapFrame) {
    let n = frame.syscall_nr();
    let a = frame.syscall_args();
    let ret: isize = match n {
        nr::WRITE if a[0] == 1 || a[0] == 2 => {
            // Self-test only: the real core resolves user pointers through
            // the address space. Here the user page is known to be mapped
            // and readable through the direct map by the self-test.
            crate::selftest::user_write(a[1] as usize, a[2] as usize)
        }
        nr::EXIT_GROUP | nr::EXIT => crate::exit(a[0] as i32),
        _ => errno::ENOSYS.to_ret(),
    };
    frame.set_syscall_ret(ret as u64);
}

/// A user-mode memory fault at `addr`. Returns normally once the fault has
/// been resolved (e.g. a lazily mapped page was filled in) so the arch can
/// retry the access; on an unrecoverable fault the core terminates the
/// program itself (dump + exit) and does not return.
pub fn user_fault(frame: &mut TrapFrame, addr: usize, access: Access, kind: FaultKind) {
    crate::kprintln!(
        "[patina] fatal: user {:?} fault ({:?}) at {:#x}",
        access,
        kind,
        addr
    );
    frame.dump();
    crate::exit(128 + 11)
}

/// A non-fault, non-syscall synchronous exception from user mode.
pub fn user_exception(frame: &mut TrapFrame, exc: Exception) {
    crate::kprintln!("[patina] fatal: user exception {:?}", exc);
    frame.dump();
    crate::exit(128 + 4)
}

/// Timer tick. `from_user` is true when the interrupt arrived in user mode
/// (the core may switch threads directly from here); when it arrived in
/// kernel mode the core only records that a reschedule is due.
pub fn timer_tick(_frame: &mut TrapFrame, _from_user: bool) {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Called by the arch on every return to user mode (after a syscall, fault
/// or interrupt handled in `frame`), with interrupts disabled. The core uses
/// it for deferred reschedules and, in Phase 2, signal delivery.
pub fn before_return_to_user(_frame: &mut TrapFrame) {}
