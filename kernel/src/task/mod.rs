//! Process and threads (design §5).
//!
//! There is exactly one [`Process`] per VM (pid 1, `/app`), a global set
//! once by the loader. Threads are Go Ms. W2a has a single thread (thread 0,
//! tid 1) running on the boot stack; W2b adds `clone`, the scheduler and
//! futexes on top of these structures. The places W2b plugs in are marked
//! `W2b:`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::signal::{SS_DISABLE, SigAction, SigSet, StackT};

use crate::arch::{self, cpu, ctx, timer};
use crate::mm::vm::AddressSpace;
use crate::sync::{OnceCell, SpinLock};

/// Number of `SigAction` slots: index = signal number (0 unused), 1..=64.
pub const NSIG_SLOTS: usize = 65;

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

/// A kernel stack `[base, top)`.
#[derive(Clone, Copy, Debug)]
pub struct KernelStack {
    pub base: usize,
    pub top: usize,
}

/// Per-thread signal state.
#[derive(Clone, Copy, Debug)]
pub struct SigState {
    /// Blocked signals (never contains SIGKILL/SIGSTOP).
    pub mask: SigSet,
    pub altstack: StackT,
}

/// User state a thread starts from (before its first entry to user mode).
pub struct StartState {
    pub frame: Box<ctx::TrapFrame>,
    /// x86_64: XSAVE area (64-aligned: `FpState` is `repr(align(64))` and
    /// `Box` honours the alignment). aarch64: empty.
    pub fp: Box<ctx::FpState>,
}

/// A thread (Go M).
pub struct Thread {
    pub tid: u32,
    pub sig: SpinLock<SigState>,
    /// `set_tid_address` / `CLONE_CHILD_CLEARTID` pointer (0 = none).
    pub clear_child_tid: AtomicU64,
    /// `CLONE_CHILD_SETTID` pointer.
    pub set_child_tid: AtomicU64,
    /// Initial user state, consumed by the first entry.
    pub start: SpinLock<Option<StartState>>,
    /// W2b: allocate per-thread stacks (with a guard page); thread 0 uses
    /// the boot stack.
    pub kstack: KernelStack,
    /// `PR_SET_NAME` (comm), NUL-padded.
    pub name: SpinLock<[u8; 16]>,
}

impl Thread {
    pub fn new(tid: u32, kstack: KernelStack, start: StartState) -> Thread {
        Thread {
            tid,
            sig: SpinLock::new(SigState {
                mask: 0,
                altstack: StackT {
                    ss_flags: SS_DISABLE,
                    ..StackT::default()
                },
            }),
            clear_child_tid: AtomicU64::new(0),
            set_child_tid: AtomicU64::new(0),
            start: SpinLock::new(Some(start)),
            kstack,
            name: SpinLock::new(*b"app\0\0\0\0\0\0\0\0\0\0\0\0\0"),
        }
    }

    pub fn clear_child_tid(&self) -> u64 {
        self.clear_child_tid.load(Ordering::Relaxed)
    }
}

static THREAD0: OnceCell<Thread> = OnceCell::new();

/// The thread running on this CPU.
///
/// W2b: replace with the per-CPU current-thread pointer the scheduler sets
/// on every switch.
pub fn current() -> &'static Thread {
    THREAD0.get().expect("task: no current thread")
}

/// Create thread 0 (tid 1) with the loader's initial user state, on the
/// boot stack.
pub fn create_thread0(frame: ctx::TrapFrame) -> &'static Thread {
    let kstack = KernelStack {
        base: arch::layout::boot_stack_bottom(),
        top: arch::layout::boot_stack_top(),
    };
    let t = Thread::new(
        1,
        kstack,
        StartState {
            frame: Box::new(frame),
            fp: Box::new(ctx::FpState::default()),
        },
    );
    match THREAD0.set(t) {
        Ok(t) => t,
        Err(_) => panic!("task: thread 0 already exists"),
    }
}

/// Start thread 0: activate the address space, start the tick and enter
/// user mode. Never returns; the boot stack below this point is abandoned
/// and becomes thread 0's kernel stack for entries from user mode.
///
/// W2b: other threads start from a `new_kernel_context` trampoline that
/// does the same (minus `timer::init`) on their own kernel stack.
pub fn start_thread0() -> ! {
    let t = current();
    let start = t.start.lock().take().expect("thread 0 already started");
    let frame: ctx::TrapFrame = (*start.frame).clone();
    {
        let space = process().space.lock();
        // SAFETY: the tables live in the process (a 'static global) for
        // the life of the VM; their upper half is the kernel's.
        unsafe { space.tables.activate() };
    }
    // The tick must be running on the boot CPU before user code; it also
    // unmasks IRQs (enter_user masks them again until the eret/iretq).
    timer::init();
    let flags = arch::irq::save_disable();
    cpu::set_kernel_stack(t.kstack.top);
    ctx::fp_restore(&start.fp);
    drop(start);
    let _ = flags; // enter_user returns to user mode with IRQs enabled.
    // SAFETY: the user tables are live, `frame` is a fresh user frame, the
    // kernel stack for entries is set and the FP state loaded.
    unsafe { ctx::enter_user(&frame) }
}
