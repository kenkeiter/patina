//! Process and threads (design §5).
//!
//! There is exactly one [`Process`] per VM (pid 1, `/app`), a global set
//! once by the loader. Threads are Go Ms: thread 0 (tid 1) is created by the
//! loader, the others by `clone` (tids from 2 upwards). Every thread has its
//! own guarded kernel stack ([`kstack`]) and is scheduled by [`sched`].
//!
//! Modules:
//! * [`sched`]: per-CPU state (current thread, runqueue, idle context),
//!   `schedule`, the tick, thread start trampolines and thread exit;
//! * [`wait`]: wait queues and the lost-wakeup-free `block` primitive;
//! * [`sleep`]: the deadline-ordered sleep list expired from the tick;
//! * [`futex`]: the futex hash table;
//! * [`kstack`]: kernel stacks with guard pages;
//! * [`signal`]: pending sets, sending, delivery and `rt_sigreturn`.

pub mod futex;
pub mod kstack;
pub mod sched;
pub mod selftest;
pub mod signal;
pub mod sleep;
pub mod timers;
pub mod wait;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use patina_abi::cred::Cred;
use patina_abi::errno::{EAGAIN, ENOMEM, Errno};
use patina_abi::signal::{SS_DISABLE, SigAction, SigSet, StackT};

pub use sched::current;

use crate::arch::ctx;
use crate::mm::vm::AddressSpace;
use crate::sync::{OnceCell, SpinLock};

/// Number of `SigAction` slots: index = signal number (0 unused), 1..=64.
pub const NSIG_SLOTS: usize = 65;

/// Upper bound on live threads (Linux's default `threads-max` is far
/// higher; Go stops at 10000 Ms by default).
pub const MAX_THREADS: usize = 10_000;

/// The (only) user process.
pub struct Process {
    pub pid: u32,
    pub space: SpinLock<AddressSpace>,
    /// Handler table, shared by all threads (`CLONE_SIGHAND`).
    pub sigactions: SpinLock<[SigAction; NSIG_SLOTS]>,
    pub argv: Vec<Vec<u8>>,
    /// `K=V` strings.
    pub envp: Vec<Vec<u8>>,
    pub exe_path: &'static [u8],
    /// `patina.strace`: trace every syscall to the console.
    pub strace: bool,
    /// `patina.epoch`: wall-clock time at boot (`CLOCK_REALTIME` offset).
    pub epoch_ns: u64,
    /// File descriptors, shared by all threads (design §8).
    pub files: SpinLock<crate::fs::FdTable>,
    /// Current directory: an inode holding a VFS reference, changed only
    /// with the VFS lock held (`fs::namei::set_cwd`).
    pub cwd: SpinLock<patina_vfs::Ino>,
    /// File-creation mask (`umask`).
    pub umask: AtomicU32,
}

static PROCESS: OnceCell<Process> = OnceCell::new();

/// Install the process (loader, once).
pub fn set_process(p: Process) -> &'static Process {
    match PROCESS.set(p) {
        Ok(p) => p,
        Err(_) => panic!("task: process already set"),
    }
}

/// The process; panics before the loader ran.
pub fn process() -> &'static Process {
    PROCESS.get().expect("task: no process yet")
}

/// The process, if the loader has run (the arch self-tests run without).
pub fn try_process() -> Option<&'static Process> {
    PROCESS.get()
}

/// A kernel stack `[base, top)` (see [`kstack`]).
#[derive(Clone, Copy, Debug)]
pub struct KernelStack {
    pub base: usize,
    pub top: usize,
}

/// Per-thread signal state that only the thread itself touches (the
/// blocked mask is [`Thread::blocked`], readable by senders).
#[derive(Clone, Copy, Debug)]
pub struct SigState {
    /// `sigaltstack` as Linux stores it: `ss_flags` is the raw value the
    /// program passed (`SS_DISABLE` with sp = size = 0 when disabled).
    pub altstack: StackT,
}

/// User state a thread starts from (before its first entry to user mode).
pub struct StartState {
    pub frame: Box<ctx::TrapFrame>,
    /// x86_64: XSAVE area (64-aligned: `FpState` is `repr(align(64))` and
    /// `Box` honours the alignment). aarch64: empty.
    pub fp: Box<ctx::FpState>,
}

/// Scheduling state of a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    /// On a runqueue, waiting for a CPU.
    Runnable,
    /// Current on some CPU.
    Running,
    /// Parked on a wait queue and/or the sleep list.
    Blocked,
    /// Exited; its kernel stack is freed by the next thread to run.
    Dead,
}

/// Why a blocked thread was made runnable again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeReason {
    /// Explicit wake-up (futex wake, wait queue wake).
    Woken,
    /// The deadline passed (sleep list).
    TimedOut,
    /// A signal became pending for a thread in an interruptible block
    /// (`wait::block_interruptible`); the syscall returns `EINTR` or is
    /// restarted (see `task::signal`).
    Interrupted,
}

// Packed scheduling word: bits 0..4 state, bits 4..6 wake reason
// (0 Woken, 1 TimedOut, 2 Interrupted), bit 6 "blocked interruptibly",
// bits 8.. the block sequence number. Every block increments the
// sequence, so a wake-up carrying a stale sequence (a sleep-list entry or
// wait-queue entry from an earlier block) fails its compare-exchange. One
// atomic word keeps this correct when Phase 2 wakes threads from other
// CPUs.
const ST_RUNNABLE: u64 = 0;
const ST_RUNNING: u64 = 1;
const ST_BLOCKED: u64 = 2;
const ST_DEAD: u64 = 3;
const ST_MASK: u64 = 0xf;
const ST_REASON_SHIFT: u32 = 4;
const ST_REASON_MASK: u64 = 0b11 << ST_REASON_SHIFT;
const ST_INTR: u64 = 1 << 6;
const SEQ_SHIFT: u32 = 8;

const fn reason_bits(r: WakeReason) -> u64 {
    (match r {
        WakeReason::Woken => 0,
        WakeReason::TimedOut => 1,
        WakeReason::Interrupted => 2,
    }) << ST_REASON_SHIFT
}

/// What a thread runs.
#[derive(Clone, Copy, Debug)]
pub enum ThreadKind {
    /// A user thread: enters user mode from its [`StartState`].
    User,
    /// A kernel-only thread (self-tests): runs `f` and exits.
    Kernel(fn()),
}

/// Context-switch state, touched only by the scheduler on the CPU the
/// thread is running on / being switched on, with IRQs masked.
pub struct SwitchState {
    pub kctx: ctx::KernelContext,
    /// User FP state while the thread is switched out (x86_64; empty on
    /// aarch64, where the trap frame carries it).
    pub fp: Box<ctx::FpState>,
    /// Has run at least once (fresh threads restore their FP state from
    /// the `StartState` in the start trampoline instead).
    pub started: bool,
}

/// A thread (Go M).
pub struct Thread {
    pub tid: u32,
    pub kind: ThreadKind,
    pub sig: SpinLock<SigState>,
    /// `set_tid_address` / `CLONE_CHILD_CLEARTID` pointer (0 = none).
    pub clear_child_tid: AtomicU64,
    /// `CLONE_CHILD_SETTID` pointer.
    pub set_child_tid: AtomicU64,
    /// Initial user state, consumed by the first entry.
    pub start: SpinLock<Option<StartState>>,
    /// Kernel stack (guarded, from [`kstack`]).
    pub kstack: KernelStack,
    /// `PR_SET_NAME` (comm), NUL-padded.
    pub name: SpinLock<[u8; 16]>,
    /// Credentials (per thread, as on Linux; `clone` copies the parent's).
    pub cred: SpinLock<Cred>,
    /// Supplementary groups (`setgroups`), shared until changed.
    pub groups: SpinLock<Arc<Vec<u32>>>,
    /// Packed state / wake reason / block sequence (see `ST_*`).
    sched_word: AtomicU64,
    /// SMP: the CPU this thread runs on / last ran on (`sched::kick_remote`,
    /// wake placement).
    pub(crate) cpu: AtomicUsize,
    /// SMP: true from the moment a CPU switches this thread in until the
    /// CPU that switched it out has saved its context (`sched` docs).
    pub(crate) on_cpu: AtomicBool,
    /// CPU time consumed (user + system), ns; see `sched::current_cpu_time_ns`.
    pub(crate) cpu_time_ns: AtomicU64,
    /// Blocked signals (never SIGKILL/SIGSTOP). Written only by the thread
    /// itself; read by senders choosing a target.
    pub blocked: AtomicU64,
    /// Thread-directed pending signals (`tgkill`, faults, thread timers).
    pub sigq: SpinLock<signal::PendingQueue>,
    /// Bits of `sigq` (updated under its lock, read lock-free by the
    /// return-to-user fast path).
    pub sig_pending: AtomicU64,
    /// Linux `saved_sigmask`: the mask to restore (and to save in the
    /// signal frame) after `rt_sigsuspend` / `epoll_pwait` / `ppoll` with
    /// a temporary mask; `signal::NO_SAVED_MASK` when none. Only the
    /// thread itself touches it.
    pub saved_mask: AtomicU64,
    /// Internal `ERESTART*` code of the syscall this thread is returning
    /// from (set by `syscall::dispatch`, consumed by signal delivery); 0
    /// when none.
    pub restart: AtomicI32,
    /// `restart_syscall` state for `ERESTART_RESTARTBLOCK`.
    pub restart_block: SpinLock<Option<signal::RestartBlock>>,
    switch: UnsafeCell<SwitchState>,
}

// SAFETY: every field but `switch` is Sync. `switch` is accessed only by
// the scheduler (sched.rs) with IRQs masked, on the CPU that is switching
// the thread out or in; a thread is never switched on two CPUs at once
// (it is on at most one runqueue, or current on one CPU).
unsafe impl Sync for Thread {}
// SAFETY: as above; nothing in a Thread is tied to the creating CPU.
unsafe impl Send for Thread {}

impl Thread {
    #[allow(clippy::too_many_arguments)] // one per inherited piece of state
    fn new(
        tid: u32,
        kind: ThreadKind,
        kstack: KernelStack,
        start: Option<StartState>,
        fp: Box<ctx::FpState>,
        sig: SigState,
        blocked: SigSet,
        cred: (Cred, Arc<Vec<u32>>),
    ) -> Thread {
        let kctx = match kind {
            ThreadKind::User => {
                ctx::new_kernel_context(kstack.top, sched::user_thread_entry, tid as usize)
            }
            ThreadKind::Kernel(f) => {
                ctx::new_kernel_context(kstack.top, sched::kernel_thread_entry, f as usize)
            }
        };
        Thread {
            tid,
            kind,
            sig: SpinLock::new(sig),
            clear_child_tid: AtomicU64::new(0),
            set_child_tid: AtomicU64::new(0),
            start: SpinLock::new(start),
            kstack,
            name: SpinLock::new(*b"app\0\0\0\0\0\0\0\0\0\0\0\0\0"),
            cred: SpinLock::new(cred.0),
            groups: SpinLock::new(cred.1),
            sched_word: AtomicU64::new(ST_RUNNABLE),
            cpu: AtomicUsize::new(0),
            on_cpu: AtomicBool::new(false),
            cpu_time_ns: AtomicU64::new(0),
            blocked: AtomicU64::new(blocked),
            sigq: SpinLock::new(signal::PendingQueue::new()),
            sig_pending: AtomicU64::new(0),
            saved_mask: AtomicU64::new(signal::NO_SAVED_MASK),
            restart: AtomicI32::new(0),
            restart_block: SpinLock::new(None),
            switch: UnsafeCell::new(SwitchState {
                kctx,
                fp,
                started: false,
            }),
        }
    }

    pub fn clear_child_tid(&self) -> u64 {
        self.clear_child_tid.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> ThreadState {
        match self.sched_word.load(Ordering::Acquire) & ST_MASK {
            ST_RUNNABLE => ThreadState::Runnable,
            ST_RUNNING => ThreadState::Running,
            ST_BLOCKED => ThreadState::Blocked,
            _ => ThreadState::Dead,
        }
    }

    /// The blocked signal mask.
    #[inline]
    pub fn blocked(&self) -> SigSet {
        self.blocked.load(Ordering::SeqCst)
    }

    /// Set the state, keeping the sequence (and clearing the wake reason
    /// unless going to Running, which keeps it for the woken thread).
    pub(crate) fn set_state(&self, st: ThreadState) {
        let v = match st {
            ThreadState::Runnable => ST_RUNNABLE,
            ThreadState::Running => ST_RUNNING,
            ThreadState::Blocked => ST_BLOCKED,
            ThreadState::Dead => ST_DEAD,
        };
        let keep = if v == ST_RUNNING { ST_REASON_MASK } else { 0 };
        let mut w = self.sched_word.load(Ordering::Acquire);
        loop {
            let new = (w & !ST_MASK & !ST_INTR & (!ST_REASON_MASK | keep)) | v;
            match self
                .sched_word
                .compare_exchange_weak(w, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(cur) => w = cur,
            }
        }
    }

    /// The calling (current, Running) thread starts blocking: bump the
    /// sequence, state Blocked. Returns the new sequence, which wake-ups
    /// must quote.
    pub(crate) fn begin_block(&self) -> u64 {
        self.begin_block_with(false)
    }

    /// As [`begin_block`](Self::begin_block), but a signal may end the
    /// block (`sched::kick` → `WakeReason::Interrupted`).
    pub(crate) fn begin_block_interruptible(&self) -> u64 {
        self.begin_block_with(true)
    }

    fn begin_block_with(&self, intr: bool) -> u64 {
        let w = self.sched_word.load(Ordering::Acquire);
        let seq = (w >> SEQ_SHIFT) + 1;
        let flag = if intr { ST_INTR } else { 0 };
        // SeqCst: pairs with the sender's SeqCst pending-bit update (the
        // blocker re-checks pending signals after this store, the sender
        // reads this word after its store; one of them sees the other).
        self.sched_word
            .store((seq << SEQ_SHIFT) | ST_BLOCKED | flag, Ordering::SeqCst);
        seq
    }

    /// Blocked (in block `seq`) → Runnable with `reason`. False if the
    /// thread was already woken or has blocked again since, or (for
    /// `Interrupted`) if the block is not interruptible.
    pub(crate) fn try_wake(&self, seq: u64, reason: WakeReason) -> bool {
        let mut w = self.sched_word.load(Ordering::Acquire);
        loop {
            if w >> SEQ_SHIFT != seq
                || w & ST_MASK != ST_BLOCKED
                || (reason == WakeReason::Interrupted && w & ST_INTR == 0)
            {
                return false;
            }
            let to = (seq << SEQ_SHIFT) | ST_RUNNABLE | reason_bits(reason);
            match self
                .sched_word
                .compare_exchange(w, to, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(cur) => w = cur,
            }
        }
    }

    /// The calling thread, Blocked in block `seq` but not yet switched
    /// out, found a pending signal: back to Running with reason
    /// `Interrupted`. False if a waker got there first (it has then also
    /// queued the thread; the caller must go through `schedule`).
    pub(crate) fn cancel_block(&self, seq: u64) -> bool {
        let from = self.sched_word.load(Ordering::Acquire);
        if from >> SEQ_SHIFT != seq || from & ST_MASK != ST_BLOCKED {
            return false;
        }
        let to = (seq << SEQ_SHIFT) | ST_RUNNING | reason_bits(WakeReason::Interrupted);
        self.sched_word
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The block sequence if the thread is blocked interruptibly.
    pub(crate) fn interruptible_seq(&self) -> Option<u64> {
        let w = self.sched_word.load(Ordering::SeqCst);
        (w & ST_MASK == ST_BLOCKED && w & ST_INTR != 0).then_some(w >> SEQ_SHIFT)
    }

    /// Why the last block ended.
    pub(crate) fn wake_reason(&self) -> WakeReason {
        match (self.sched_word.load(Ordering::Acquire) & ST_REASON_MASK) >> ST_REASON_SHIFT {
            1 => WakeReason::TimedOut,
            2 => WakeReason::Interrupted,
            _ => WakeReason::Woken,
        }
    }

    /// The switch state.
    ///
    /// # Safety
    /// Only the scheduler may call this, with IRQs masked, for a thread that
    /// is current on this CPU or that this CPU is switching in or out; no
    /// other reference to the switch state may be live.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn switch_state(&self) -> &mut SwitchState {
        // SAFETY: per the caller contract.
        unsafe { &mut *self.switch.get() }
    }

    pub(crate) fn kctx_ptr(&self) -> *mut ctx::KernelContext {
        // SAFETY: only forms a raw pointer.
        unsafe { &raw mut (*self.switch.get()).kctx }
    }
}

// ---------------------------------------------------------------------------
// Thread table.

/// Next tid (thread 0 is tid 1 = pid).
static NEXT_TID: AtomicU32 = AtomicU32::new(2);
/// Live (not yet exited) user threads.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// All live threads by tid (Phase 2: tgkill target lookup, signal delivery
/// to "any thread not masking it", /proc/self/task).
static THREADS: SpinLock<BTreeMap<u32, Arc<Thread>>> = SpinLock::new(BTreeMap::new());

/// Snapshot of all live threads (signal target selection).
pub fn threads() -> Vec<Arc<Thread>> {
    THREADS.lock().values().cloned().collect()
}

/// Look a thread up by tid.
pub fn thread_by_tid(tid: u32) -> Option<Arc<Thread>> {
    THREADS.lock().get(&tid).cloned()
}

/// Number of live user threads.
pub fn live_threads() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn default_sig() -> SigState {
    SigState {
        altstack: StackT {
            ss_flags: SS_DISABLE,
            ..StackT::default()
        },
    }
}

fn root_cred() -> (Cred, Arc<Vec<u32>>) {
    (Cred::root(), Arc::new(Vec::new()))
}

/// The calling thread's credentials and groups (what `clone` copies).
pub fn current_cred() -> (Cred, Arc<Vec<u32>>) {
    let t = current();
    let c = *t.cred.lock();
    let g = t.groups.lock().clone();
    (c, g)
}

/// The process's credentials as `/proc/<pid>/status` shows them: the
/// main thread's (tid 1), or the caller's once it has exited.
pub fn process_cred() -> (Cred, Arc<Vec<u32>>) {
    match thread_by_tid(1) {
        Some(t) => {
            let c = *t.cred.lock();
            let g = t.groups.lock().clone();
            (c, g)
        }
        None => current_cred(),
    }
}

fn register(t: Thread) -> Arc<Thread> {
    let t = Arc::new(t);
    if let ThreadKind::User = t.kind {
        LIVE.fetch_add(1, Ordering::Relaxed);
    }
    THREADS.lock().insert(t.tid, t.clone());
    t
}

/// Create thread 0 (tid 1) with the loader's initial user state.
pub fn create_thread0(frame: ctx::TrapFrame) -> Arc<Thread> {
    let kstack = kstack::alloc().expect("task: no memory for thread 0's kernel stack");
    let t = Thread::new(
        1,
        ThreadKind::User,
        kstack,
        Some(StartState {
            frame: Box::new(frame),
            fp: Box::default(),
        }),
        Box::default(),
        default_sig(),
        0,
        root_cred(),
    );
    register(t)
}

/// Create a user thread for `clone` from the parent's user state: `frame`
/// is the child's register state (return value, stack and TLS already
/// adjusted), `fp` the parent's FP state at the syscall, `mask` the
/// parent's signal mask (the alternate stack is not inherited for
/// `CLONE_VM` threads). The thread is NOT runnable yet: the caller does
/// the `*_SETTID` writes and then calls `sched::make_runnable` (D41).
pub fn spawn_user_thread(
    frame: ctx::TrapFrame,
    fp: ctx::FpState,
    mask: SigSet,
    clear_child_tid: u64,
) -> Result<Arc<Thread>, Errno> {
    if LIVE.load(Ordering::Relaxed) >= MAX_THREADS {
        return Err(Errno(EAGAIN));
    }
    // A kernel stack (frames, and page tables to map it) and the thread's
    // objects: only while the heap's low watermark holds (D87).
    crate::mm::heap::reserve((kstack::KSTACK_PAGES + 2) * crate::mm::PAGE_SIZE)?;
    let kstack = kstack::alloc().ok_or(Errno(ENOMEM))?;
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    let fp = Box::new(fp);
    let t = Thread::new(
        tid,
        ThreadKind::User,
        kstack,
        Some(StartState {
            frame: Box::new(frame),
            fp: fp.clone(),
        }),
        fp,
        default_sig(),
        mask,
        current_cred(),
    );
    t.clear_child_tid.store(clear_child_tid, Ordering::Relaxed);
    Ok(register(t))
}

/// Create a kernel-only thread running `f` (self-tests) and make it
/// runnable. It exits when `f` returns.
pub fn spawn_kernel_thread(f: fn()) -> Arc<Thread> {
    let kstack = kstack::alloc().expect("task: no memory for a kernel stack");
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    let t = register(Thread::new(
        tid,
        ThreadKind::Kernel(f),
        kstack,
        None,
        Box::default(),
        default_sig(),
        0,
        root_cred(),
    ));
    sched::make_runnable(t.clone());
    t
}

/// Remove the calling thread from the table. Returns true if it was the
/// last live user thread.
pub(crate) fn unregister_current() -> bool {
    let t = current();
    THREADS.lock().remove(&t.tid);
    match t.kind {
        ThreadKind::User => LIVE.fetch_sub(1, Ordering::Relaxed) == 1,
        ThreadKind::Kernel(_) => false,
    }
}

/// Activate the process's user page tables on this CPU (each CPU, before
/// it first enters user mode).
pub(crate) fn activate_user_tables() {
    let space = process().space.lock();
    // SAFETY: the tables live in the process (a 'static global) for the
    // life of the VM; their upper half is the kernel's.
    unsafe { space.tables.activate() };
}

/// Start thread 0: activate the address space, start the tick and the
/// secondary CPUs (`sched::run_smp`), and turn the boot context into this
/// CPU's idle loop, which switches to thread 0 on its own kernel stack.
/// Never returns.
pub fn start_thread0(t0: Arc<Thread>) -> ! {
    activate_user_tables();
    sched::run_smp(t0)
}
