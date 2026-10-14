//! Signal generation and delivery (design §6).
//!
//! # State
//! * handler table: `Process::sigactions` (process-wide, `CLONE_SIGHAND`);
//! * process-wide pending set: [`SHARED`] + its bit mirror [`SHARED_BITS`]
//!   (`kill`, host-injected signals, process timers). There is one process
//!   per VM, so these are statics rather than `Process` fields;
//! * per thread: `Thread::sigq` + bit mirror `Thread::sig_pending`
//!   (`tgkill`, synchronous fault signals, thread timers), the blocked
//!   mask `Thread::blocked`, and `Thread::sig` (sigaltstack, the
//!   `rt_sigsuspend` saved mask).
//!
//! The bit mirrors are written under the queue's lock and read lock-free,
//! so the return-to-user fast path ([`deliver`]) is three relaxed-ish
//! loads when nothing is pending. Standard signals (1..=31) coalesce: a
//! second instance while one is pending is dropped. Real-time signals
//! (32..=64) queue one `SigInfo` per instance (up to [`MAX_QUEUED`] per
//! queue).
//!
//! # Sending
//! [`send_to_thread`] / [`send_to_process`] queue a `SigInfo` and then
//! `sched::kick` the chosen thread (wakes it with
//! `WakeReason::Interrupted` if it is in `wait::block_interruptible`).
//! Signals whose disposition ignores them (`SIG_IGN`, or `SIG_DFL` for
//! SIGCHLD/SIGURG/SIGWINCH/SIGCONT) are dropped at send time unless the
//! target blocks them, as Linux's `sig_ignored`. Synchronous faults use
//! [`force_fault`] (Linux `force_sig_fault`): a blocked or ignored fault
//! signal is unblocked and reset to `SIG_DFL`, so it terminates.
//!
//! # Delivery and syscall restart
//! [`deliver`] runs from `trap::before_return_to_user` after the
//! reschedule, IRQs masked, on the thread's kernel stack. Blocking
//! syscalls end an interruptible wait with an internal `ERESTART*` code;
//! `syscall::dispatch` records it in `Thread::restart` and [`deliver`]
//! resolves it exactly like Linux's `arch_do_signal_or_restart`:
//! with a handler, `ERESTARTNOHAND`/`ERESTART_RESTARTBLOCK` → `EINTR`,
//! `ERESTARTSYS` → restart if `SA_RESTART` else `EINTR`,
//! `ERESTARTNOINTR` → restart; without a handler every code restarts
//! (`ERESTART_RESTARTBLOCK` through `restart_syscall` and
//! `Thread::restart_block`). Codes 512..=516 never reach user mode.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::current::MINSIGSTKSZ;
use patina_abi::errno::{EINTR, EINVAL, ENOMEM, EPERM, Errno};
use patina_abi::signal::{
    DefaultAction, SIG_DFL, SIG_IGN, SIGBUS, SIGFPE, SIGILL, SIGKILL, SIGRTMIN, SIGSEGV, SIGSYS,
    SIGTRAP, SS_AUTODISARM, SS_DISABLE, SS_ONSTACK, SaFlags, SigAction, SigInfo, SigSet, StackT,
    UNBLOCKABLE, default_action, sigbit,
};

use super::{Thread, ThreadKind, ThreadState, current, process, sched};
use crate::arch::ctx::TrapFrame;
use crate::arch::sigframe;
use crate::sync::SpinLock;

/// Internal syscall results (`include/linux/errno.h`), never visible to
/// user mode. Returned as `Err(Errno(..))` by blocking syscalls.
pub const ERESTARTSYS: i32 = 512;
pub const ERESTARTNOINTR: i32 = 513;
pub const ERESTARTNOHAND: i32 = 514;
pub const ERESTART_RESTARTBLOCK: i32 = 516;

/// Whether `e` is one of the internal restart codes.
pub const fn is_restart(e: i32) -> bool {
    matches!(
        e,
        ERESTARTSYS | ERESTARTNOINTR | ERESTARTNOHAND | ERESTART_RESTARTBLOCK
    )
}

/// Max queued entries per pending queue (real-time signals; standard ones
/// coalesce). Linux's limit is `RLIMIT_SIGPENDING` per user.
pub const MAX_QUEUED: usize = 1024;

/// Signals Linux dequeues first from a thread's own queue
/// (`SYNCHRONOUS_MASK`).
const SYNCHRONOUS: SigSet = sigbit(SIGSEGV)
    | sigbit(SIGBUS)
    | sigbit(SIGILL)
    | sigbit(SIGTRAP)
    | sigbit(SIGFPE)
    | sigbit(SIGSYS);

/// Hardware fault details carried with a synchronous signal into the
/// frame: aarch64 `addr` = FAR (`sigcontext.fault_address`), `code` = ESR
/// (`esr_context`); x86_64 `addr` = CR2, `code` = error code, `trapno` =
/// vector (`sigcontext.cr2/err/trapno`).
#[derive(Clone, Copy, Debug, Default)]
pub struct FaultInfo {
    pub addr: u64,
    pub code: u64,
    pub trapno: u64,
}

/// One pending signal instance.
#[derive(Clone, Copy, Debug)]
pub struct Queued {
    pub info: SigInfo,
    pub fault: Option<FaultInfo>,
}

/// A pending set: the instances in arrival order plus their bitmap.
pub struct PendingQueue {
    q: Vec<Queued>,
    bits: SigSet,
}

impl Default for PendingQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingQueue {
    pub const fn new() -> PendingQueue {
        PendingQueue {
            q: Vec::new(),
            bits: 0,
        }
    }

    pub fn bits(&self) -> SigSet {
        self.bits
    }

    /// Queue `item`; false if it was coalesced/dropped.
    fn push(&mut self, item: Queued) -> bool {
        let sig = item.info.si_signo;
        let bit = sigbit(sig);
        if (sig < SIGRTMIN && self.bits & bit != 0) || self.q.len() >= MAX_QUEUED {
            return false;
        }
        self.q.push(item);
        self.bits |= bit;
        true
    }

    /// Remove the oldest instance of `sig`.
    fn take(&mut self, sig: i32) -> Option<Queued> {
        let i = self.q.iter().position(|x| x.info.si_signo == sig)?;
        let item = self.q.remove(i);
        if !self.q.iter().any(|x| x.info.si_signo == sig) {
            self.bits &= !sigbit(sig);
        }
        Some(item)
    }

    /// Remove the lowest-numbered pending signal in `allowed`
    /// (synchronous ones first when `sync_first`).
    fn dequeue(&mut self, allowed: SigSet, sync_first: bool) -> Option<Queued> {
        let avail = self.bits & allowed;
        if avail == 0 {
            return None;
        }
        let pick = if sync_first && avail & SYNCHRONOUS != 0 {
            avail & SYNCHRONOUS
        } else {
            avail
        };
        self.take(pick.trailing_zeros() as i32 + 1)
    }
}

/// `Thread::saved_mask` value meaning "none" (a real mask never contains
/// SIGKILL).
pub const NO_SAVED_MASK: SigSet = u64::MAX;

/// Install a temporary mask `m` for the current thread, remembering the
/// current one as the saved mask (kept if one is already saved), as
/// Linux's `set_user_sigmask`.
pub fn set_temp_mask(m: SigSet) {
    let t = current();
    if t.saved_mask.load(Ordering::Relaxed) == NO_SAVED_MASK {
        t.saved_mask.store(t.blocked(), Ordering::Relaxed);
    }
    t.blocked.store(m & !UNBLOCKABLE, Ordering::SeqCst);
}

/// Take the saved mask, if any.
fn take_saved_mask(t: &Thread) -> Option<SigSet> {
    let m = t.saved_mask.swap(NO_SAVED_MASK, Ordering::Relaxed);
    (m != NO_SAVED_MASK).then_some(m)
}

/// Restore the saved mask now (the temporary mask's syscall completed
/// without being interrupted; Linux `restore_saved_sigmask`).
pub fn restore_saved_mask() {
    let t = current();
    if let Some(m) = take_saved_mask(t) {
        t.blocked.store(m & !UNBLOCKABLE, Ordering::SeqCst);
    }
}

/// Process-wide pending set.
static SHARED: SpinLock<PendingQueue> = SpinLock::new(PendingQueue::new());
/// Bits of [`SHARED`].
static SHARED_BITS: AtomicU64 = AtomicU64::new(0);

/// `restart_syscall` continuation (Linux `struct restart_block`), set by a
/// syscall that returns `ERESTART_RESTARTBLOCK`.
#[derive(Clone, Copy, Debug)]
pub enum RestartBlock {
    /// `nanosleep`/relative `clock_nanosleep`: sleep until `deadline`
    /// (monotonic ns), write the remaining time to `rem` (0 = none) if
    /// interrupted again.
    Nanosleep { deadline: u64, rem: usize },
    /// `FUTEX_WAIT(_BITSET)` with a timeout, absolute monotonic deadline.
    FutexWait {
        uaddr: usize,
        val: u32,
        bits: u32,
        deadline: u64,
    },
}

/// What the arch frame builder needs (`arch::sigframe::setup_frame`).
pub struct SigDelivery<'a> {
    pub signo: i32,
    pub info: &'a SigInfo,
    pub action: SigAction,
    /// Mask to save in `uc_sigmask` (restored by `rt_sigreturn`).
    pub mask: SigSet,
    /// The thread's sigaltstack as stored (saved into `uc_stack`).
    pub altstack: StackT,
    pub fault: Option<FaultInfo>,
}

/// Why `setup_frame` failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The frame could not be written (bad or overflowing stack).
    Fault,
    /// The handler has no `SA_RESTORER` (patina has no vDSO trampoline).
    NoRestorer,
}

// ---------------------------------------------------------------------------
// sigaltstack helpers (Linux `on_sig_stack`, `sas_ss_flags`).

/// Whether `sp` is on the alternate stack (never with `SS_AUTODISARM`).
pub fn on_sig_stack(st: &StackT, sp: u64) -> bool {
    st.ss_flags & SS_AUTODISARM == 0 && on_sig_stack_raw(st, sp)
}

/// `__on_sig_stack`: the range check alone.
pub fn on_sig_stack_raw(st: &StackT, sp: u64) -> bool {
    sp > st.ss_sp && sp - st.ss_sp <= st.ss_size
}

/// `sas_ss_flags(sp)`: `SS_DISABLE` if no stack, `SS_ONSTACK` when `sp` is
/// on it, else 0.
pub fn sas_ss_flags(st: &StackT, sp: u64) -> i32 {
    if st.ss_size == 0 {
        SS_DISABLE
    } else if on_sig_stack(st, sp) {
        SS_ONSTACK
    } else {
        0
    }
}

/// Linux `do_sigaltstack` for the current thread with user `sp`: returns
/// the old stack as `oss` reports it, then installs `ss` if given.
pub fn do_sigaltstack(ss: Option<StackT>, sp: u64) -> Result<StackT, Errno> {
    let t = current();
    let mut st = t.sig.lock();
    let cur = st.altstack;
    let old = StackT {
        ss_sp: cur.ss_sp,
        ss_flags: sas_ss_flags(&cur, sp) | (cur.ss_flags & SS_AUTODISARM),
        _pad: 0,
        ss_size: cur.ss_size,
    };
    if let Some(n) = ss {
        if on_sig_stack(&cur, sp) {
            return Err(Errno(EPERM));
        }
        let mode = n.ss_flags & !SS_AUTODISARM;
        if mode != 0 && mode != SS_DISABLE && mode != SS_ONSTACK {
            return Err(Errno(EINVAL));
        }
        let (sp, size) = if mode == SS_DISABLE {
            (0, 0)
        } else {
            if (n.ss_size as usize) < MINSIGSTKSZ {
                return Err(Errno(ENOMEM));
            }
            (n.ss_sp, n.ss_size)
        };
        st.altstack = StackT {
            ss_sp: sp,
            ss_flags: n.ss_flags,
            _pad: 0,
            ss_size: size,
        };
    }
    Ok(old)
}

// ---------------------------------------------------------------------------
// Sending.

fn action(sig: i32) -> SigAction {
    process().sigactions.lock()[sig as usize]
}

/// Linux `sig_task_ignored` (minus the init/`SIGNAL_UNKILLABLE` rule:
/// patina's pid 1 dies from `SIG_DFL` signals, design §6).
fn ignored_by(a: &SigAction, sig: i32) -> bool {
    sig != SIGKILL
        && (a.sa_handler == SIG_IGN
            || (a.sa_handler == SIG_DFL
                && matches!(
                    default_action(sig),
                    DefaultAction::Ignore | DefaultAction::Continue
                )))
}

/// Whether `t` has a pending signal it does not block (the
/// interruptible-wait check; SeqCst pairs with `begin_block_interruptible`).
pub fn deliverable(t: &Thread) -> bool {
    (t.sig_pending.load(Ordering::SeqCst) | SHARED_BITS.load(Ordering::SeqCst)) & !t.blocked() != 0
}

/// Queue `info` for thread `t` (`tgkill`/`tkill`, per-thread timers).
/// Returns false if it was dropped (ignored, or already pending). SIGKILL
/// goes to the whole process.
pub fn send_to_thread(t: &Arc<Thread>, info: SigInfo) -> bool {
    let sig = info.si_signo;
    if sig == SIGKILL {
        return send_to_process(info);
    }
    let bit = sigbit(sig);
    let blocked = t.blocked() & bit != 0;
    if !blocked && ignored_by(&action(sig), sig) {
        return false;
    }
    {
        let mut q = t.sigq.lock();
        if !q.push(Queued { info, fault: None }) {
            return false;
        }
        t.sig_pending.store(q.bits(), Ordering::SeqCst);
    }
    if !blocked {
        sched::kick(t);
    }
    true
}

/// [`send_to_thread`] by tid; false if no such thread.
pub fn send_to_tid(tid: u32, info: SigInfo) -> bool {
    match super::thread_by_tid(tid) {
        Some(t) => send_to_thread(&t, info),
        None => false,
    }
}

/// Queue `info` on the process (`kill`, host injection, process timers)
/// and kick one thread that does not block it: a running one first, then
/// one in an interruptible wait, then any (Linux `complete_signal`).
/// Returns false if it was dropped.
pub fn send_to_process(info: SigInfo) -> bool {
    let sig = info.si_signo;
    let bit = sigbit(sig);
    let threads: Vec<Arc<Thread>> = super::threads()
        .into_iter()
        .filter(|t| matches!(t.kind, ThreadKind::User) && t.state() != ThreadState::Dead)
        .collect();
    let all_block = !threads.is_empty() && threads.iter().all(|t| t.blocked() & bit != 0);
    if !all_block && ignored_by(&action(sig), sig) {
        return false;
    }
    {
        let mut q = SHARED.lock();
        if !q.push(Queued { info, fault: None }) {
            return false;
        }
        SHARED_BITS.store(q.bits(), Ordering::SeqCst);
    }
    let eligible = || threads.iter().filter(|t| t.blocked() & bit == 0);
    let target = eligible()
        .find(|t| t.state() == ThreadState::Running)
        .or_else(|| eligible().find(|t| t.interruptible_seq().is_some()))
        .or_else(|| eligible().next());
    if let Some(t) = target {
        sched::kick(t);
    }
    true
}

/// Force a synchronous signal on the current thread (Linux
/// `force_sig_info_to_task`): if the thread blocks it or the process
/// ignores it, the disposition is reset to `SIG_DFL` and it is unblocked,
/// so it is delivered on the way back to user mode. `fault` carries the
/// hardware details for the frame.
pub fn force_fault(info: SigInfo, fault: Option<FaultInfo>) {
    let t = current();
    let sig = info.si_signo;
    let bit = sigbit(sig);
    {
        let mut tab = process().sigactions.lock();
        let a = &mut tab[sig as usize];
        let blocked = t.blocked() & bit != 0;
        if blocked || a.sa_handler == SIG_IGN {
            a.sa_handler = SIG_DFL;
            if blocked {
                t.blocked.fetch_and(!bit, Ordering::SeqCst);
            }
        }
    }
    let mut q = t.sigq.lock();
    q.push(Queued { info, fault });
    t.sig_pending.store(q.bits(), Ordering::SeqCst);
}

/// `force_sig(sig)` with `SI_KERNEL` (bad `rt_sigreturn` frame, ...).
pub fn force_sig(sig: i32) {
    force_fault(SigInfo::kernel(sig), None);
}

/// Linux `force_sigsegv`: a frame for `sig` could not be built.
fn force_sigsegv(sig: i32) {
    if sig == SIGSEGV {
        process().sigactions.lock()[SIGSEGV as usize].sa_handler = SIG_DFL;
    }
    force_sig(SIGSEGV);
}

// ---------------------------------------------------------------------------
// Delivery.

fn dequeue(t: &Thread) -> Option<Queued> {
    let allowed = !t.blocked();
    {
        let mut q = t.sigq.lock();
        if let Some(x) = q.dequeue(allowed, true) {
            t.sig_pending.store(q.bits(), Ordering::SeqCst);
            return Some(x);
        }
    }
    if SHARED_BITS.load(Ordering::SeqCst) & allowed == 0 {
        return None;
    }
    let mut q = SHARED.lock();
    let x = q.dequeue(allowed, false);
    SHARED_BITS.store(q.bits(), Ordering::SeqCst);
    x
}

/// `SIG_DFL` with a terminating default action: end the VM with
/// `128 + sig`. Fault signals keep the Phase 1 diagnostic dump.
fn terminate(sig: i32, q: &Queued, frame: &TrapFrame) -> ! {
    if q.fault.is_some() || q.info.si_code == patina_abi::signal::SI_KERNEL {
        crate::kprintln!(
            "[patina] fatal: {} (si_code {:#x}, addr {:#x}) at pc {:#x}",
            patina_abi::signal::name(sig).unwrap_or("signal"),
            q.info.si_code,
            q.info.addr(),
            frame.pc()
        );
        frame.dump();
        if let Some(p) = super::try_process()
            && let Some(s) = p.space.try_lock()
        {
            s.dump();
        }
    }
    crate::exit_signal(sig)
}

/// Resolve a pending syscall restart when a handler runs.
fn restart_for_handler(frame: &mut TrapFrame, code: i32, act: &SigAction) {
    let restart = match code {
        ERESTARTNOHAND | ERESTART_RESTARTBLOCK => false,
        ERESTARTSYS => act.flags().contains(SaFlags::RESTART),
        _ => true, // ERESTARTNOINTR
    };
    if restart {
        sigframe::rewind_syscall(frame, None);
    } else {
        frame.set_syscall_ret(Errno(EINTR).to_ret() as u64);
    }
}

/// Signal delivery on the way back to user mode (IRQs masked; called
/// from `trap::before_return_to_user` after the reschedule). Delivers at
/// most one handler per call (the handler's mask normally blocks further
/// ones; others go on a later return), drops ignored signals, terminates
/// on `SIG_DFL` terminate, and resolves syscall restarts.
pub fn deliver(frame: &mut TrapFrame) {
    let t = current();
    let restart = t.restart.load(Ordering::Relaxed);
    // Fast path: nothing pending that is not blocked, no restart.
    if restart == 0
        && t.saved_mask.load(Ordering::Relaxed) == NO_SAVED_MASK
        && (t.sig_pending.load(Ordering::Relaxed) | SHARED_BITS.load(Ordering::Relaxed))
            & !t.blocked.load(Ordering::Relaxed)
            == 0
    {
        return;
    }
    t.restart.store(0, Ordering::Relaxed);
    let mut restart = restart;
    // Bounded: a failed frame forces SIGSEGV, whose own failure is fatal.
    for _ in 0..4 {
        let Some(q) = dequeue(t) else { break };
        let sig = q.info.si_signo;
        let act = action(sig);
        if sig == SIGKILL {
            terminate(sig, &q, frame);
        }
        match act.sa_handler {
            SIG_IGN => continue,
            SIG_DFL => match default_action(sig) {
                // SIGSTOP & co.: no job control in a single-process VM;
                // treated as ignored.
                DefaultAction::Ignore | DefaultAction::Continue | DefaultAction::Stop => continue,
                DefaultAction::Terminate | DefaultAction::Core => terminate(sig, &q, frame),
            },
            _ => {}
        }
        if restart != 0 {
            restart_for_handler(frame, restart, &act);
            restart = 0;
        }
        let saved = take_saved_mask(t);
        let alt = t.sig.lock().altstack;
        let old_mask = saved.unwrap_or(t.blocked());
        let d = SigDelivery {
            signo: sig,
            info: &q.info,
            action: act,
            mask: old_mask,
            altstack: alt,
            fault: q.fault,
        };
        match sigframe::setup_frame(frame, &d) {
            Ok(()) => {
                if alt.ss_flags & SS_AUTODISARM != 0 {
                    t.sig.lock().altstack = StackT {
                        ss_flags: SS_DISABLE,
                        ..StackT::default()
                    };
                }
                let mut m = t.blocked() | act.sa_mask;
                if !act.flags().contains(SaFlags::NODEFER) {
                    m |= sigbit(sig);
                }
                t.blocked.store(m & !UNBLOCKABLE, Ordering::SeqCst);
                if act.flags().contains(SaFlags::RESETHAND) {
                    process().sigactions.lock()[sig as usize].sa_handler = SIG_DFL;
                }
                return;
            }
            Err(e) => {
                if e == SetupError::NoRestorer {
                    crate::kprintln!(
                        "[patina] signal {}: handler {:#x} has no SA_RESTORER (no vDSO trampoline)",
                        sig,
                        act.sa_handler
                    );
                }
                if let Some(m) = saved {
                    t.saved_mask.store(m, Ordering::Relaxed);
                }
                force_sigsegv(sig);
            }
        }
    }
    // No handler ran: restart the interrupted syscall.
    if restart != 0 {
        let nr =
            (restart == ERESTART_RESTARTBLOCK).then_some(patina_abi::current::nr::RESTART_SYSCALL);
        sigframe::rewind_syscall(frame, nr);
    }
    if let Some(m) = take_saved_mask(t) {
        t.blocked.store(m & !UNBLOCKABLE, Ordering::SeqCst);
    }
}

/// `rt_sigreturn`: restore the user state saved by [`deliver`] from the
/// frame on the user stack. On a bad frame SIGSEGV is forced (Linux
/// `badframe`). The restored `frame` (including the return register) is
/// what user mode resumes with.
pub fn sigreturn(frame: &mut TrapFrame) {
    let t = current();
    *t.restart_block.lock() = None;
    t.restart.store(0, Ordering::Relaxed);
    match sigframe::restore_frame(frame) {
        Ok((mask, stack)) => {
            t.blocked.store(mask & !UNBLOCKABLE, Ordering::SeqCst);
            // Linux `restore_altstack`: errors (EPERM while on the stack,
            // EINVAL, ENOMEM) are ignored.
            let _ = do_sigaltstack(Some(stack), frame.sp());
        }
        Err(()) => force_sig(SIGSEGV),
    }
}

/// `rt_sigsuspend(mask)`: wait with `mask` until a signal handler runs;
/// the old mask comes back with `rt_sigreturn` (or right away if no
/// handler runs and the call restarts).
pub fn sigsuspend(mask: SigSet) -> Errno {
    set_temp_mask(mask);
    while super::wait::sleep_until_interruptible(u64::MAX) != super::WakeReason::Interrupted {}
    Errno(ERESTARTNOHAND)
}

/// `pause()`: wait until a signal is delivered.
pub fn pause() -> Errno {
    while super::wait::sleep_until_interruptible(u64::MAX) != super::WakeReason::Interrupted {}
    Errno(ERESTARTNOHAND)
}

/// `rt_sigpending`: pending signals that are blocked.
pub fn pending_blocked() -> SigSet {
    let t = current();
    (t.sig_pending.load(Ordering::SeqCst) | SHARED_BITS.load(Ordering::SeqCst)) & t.blocked()
}
