//! Thread syscalls: `clone`, `exit`, `futex`, `sched_yield` (design §5).

use core::sync::atomic::Ordering;

use patina_abi::errno::{EINVAL, ENOSYS, EPERM, Errno};
use patina_abi::sched::{
    CLONE_THREAD_OPTIONAL, CloneFlags, FUTEX_BITSET_MATCH_ANY, FUTEX_CLOCK_REALTIME,
    FUTEX_CMD_MASK, FUTEX_LOCK_PI2, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAIT_REQUEUE_PI,
    FUTEX_WAKE, FUTEX_WAKE_BITSET, GO_CLONE_FLAGS,
};
use patina_abi::time::Timespec;

use super::{SysResult, with_space};
use crate::arch::ctx::{self, TrapFrame};
use crate::arch::timer;
use crate::task::signal::{ERESTART_RESTARTBLOCK, ERESTARTSYS, RestartBlock};
use crate::task::{self, futex, sched};

/// `clone(flags, stack, ptid, ctid/tls, tls/ctid)` (argument order per
/// arch via `clone_args`). Only thread creation is supported: the flags
/// must contain exactly Go's set (VM|FS|FILES|SIGHAND|SYSVSEM|THREAD) plus
/// optionally SETTLS / PARENT_SETTID / CHILD_SETTID / CHILD_CLEARTID /
/// DETACHED; the exit-signal byte is ignored as for any CLONE_THREAD.
pub fn clone(frame: &TrapFrame, a: &[u64; 6]) -> SysResult {
    let args = patina_abi::current::clone_args([a[0], a[1], a[2], a[3], a[4]]);
    let flags = args.clone_flags();
    if !flags.contains(GO_CLONE_FLAGS)
        || !flags
            .difference(GO_CLONE_FLAGS)
            .difference(CLONE_THREAD_OPTIONAL)
            .is_empty()
    {
        // A process-creating clone (Go's forkExec: CLONE_VFORK|CLONE_VM, or
        // no CLONE_THREAD at all) is fork/vfork, which is ENOSYS by design
        // (D57); any other unsupported flag combination is EINVAL.
        if !flags.contains(CloneFlags::THREAD) {
            return Err(Errno(ENOSYS));
        }
        return Err(Errno(EINVAL));
    }
    let settls = flags.contains(CloneFlags::SETTLS);
    if settls && args.tls >= crate::arch::uspace::USER_VA_END as u64 {
        // A non-canonical FS base would fault in the kernel's WRFSBASE.
        return Err(Errno(EPERM));
    }

    // The child resumes from the same syscall with 0 in the return register,
    // on the caller-provided stack.
    let mut cf = frame.clone();
    cf.set_syscall_ret(0);
    if args.stack != 0 {
        cf.set_sp(args.stack);
    }
    if settls {
        cf.set_tls(args.tls);
    }
    // x86_64: the live FP registers are still the caller's (soft-float
    // kernel). aarch64: FP state is in the frame; this is a no-op.
    let mut fp = ctx::FpState::default();
    ctx::fp_save(&mut fp);
    let mask = task::current().blocked();
    let cleartid = if flags.contains(CloneFlags::CHILD_CLEARTID) {
        args.child_tid
    } else {
        0
    };

    let child = task::spawn_user_thread(cf, fp, mask, cleartid)?;
    let tid = child.tid;
    // Same address space, so both writes can happen here, before the child
    // becomes runnable (it could otherwise run on another CPU and read its
    // tid word first, D41). A bad pointer is ignored rather than failing an
    // already created thread.
    if flags.contains(CloneFlags::PARENT_SETTID) {
        let _ = with_space(|s| s.write_val(args.parent_tid as usize, &tid));
    }
    if flags.contains(CloneFlags::CHILD_SETTID) {
        child.set_child_tid.store(args.child_tid, Ordering::Relaxed);
        let _ = with_space(|s| s.write_val(args.child_tid as usize, &tid));
    }
    sched::make_runnable(child);
    Ok(tid as isize)
}

/// `exit(code)`: end the calling thread. `clear_child_tid` is zeroed and
/// futex-woken (Go's `mexit` waits on it); the last thread to exit ends the
/// process with `code`.
pub fn exit(code: u64) -> ! {
    let t = task::current();
    let ctid = t.clear_child_tid() as usize;
    if ctid != 0 && with_space(|s| s.write_val(ctid, &0u32)).is_ok() {
        futex::wake(ctid, 1, FUTEX_BITSET_MATCH_ANY);
    }
    let last = task::unregister_current();
    crate::task::timers::account_dead(sched::current_cpu_time_ns());
    if last {
        super::proc::exit_group(code);
    }
    sched::exit_current()
}

/// `futex(uaddr, op, val, timeout, uaddr2, val3)`: WAIT / WAKE and their
/// BITSET forms (the whole of Go's usage). Other ops → `ENOSYS`.
pub fn futex(uaddr: usize, op: i32, val: u32, timeout: usize, val3: u32) -> SysResult {
    let cmd = op & FUTEX_CMD_MASK;
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    if realtime
        && !matches!(
            cmd,
            FUTEX_WAIT_BITSET | FUTEX_WAIT_REQUEUE_PI | FUTEX_LOCK_PI2
        )
    {
        return Err(Errno(ENOSYS));
    }
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bits = if cmd == FUTEX_WAIT {
                FUTEX_BITSET_MATCH_ANY
            } else {
                val3
            };
            if bits == 0 || !uaddr.is_multiple_of(4) {
                return Err(Errno(EINVAL));
            }
            let deadline = if timeout != 0 {
                let ts: Timespec = with_space(|s| s.read_val(timeout))?;
                let ns = ts.to_nanos().ok_or(Errno(EINVAL))?;
                Some(if cmd == FUTEX_WAIT {
                    // Relative, CLOCK_MONOTONIC.
                    timer::now_ns().saturating_add(ns)
                } else if realtime {
                    ns.saturating_sub(crate::wallclock::epoch_ns())
                } else {
                    ns
                })
            } else {
                None
            };
            futex_wait(uaddr, val, bits, deadline)
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bits = if cmd == FUTEX_WAKE {
                FUTEX_BITSET_MATCH_ANY
            } else {
                val3
            };
            if bits == 0 || !uaddr.is_multiple_of(4) {
                return Err(Errno(EINVAL));
            }
            // Linux wakes at least one waiter even for val <= 0.
            let n = (val as i32).max(1) as usize;
            Ok(futex::wake(uaddr, n, bits) as isize)
        }
        _ => Err(Errno(ENOSYS)),
    }
}

/// `FUTEX_WAIT` body (also `restart_syscall`'s continuation). Interrupted
/// by a signal: without a timeout `ERESTARTSYS` (restarted if the handler
/// has `SA_RESTART`, else `EINTR`); with one `ERESTART_RESTARTBLOCK` with
/// the absolute deadline in the restart block (`EINTR` when a handler
/// runs), as Linux's `futex_wait`.
pub fn futex_wait(uaddr: usize, val: u32, bits: u32, deadline: Option<u64>) -> SysResult {
    match futex::wait(uaddr, val, bits, deadline) {
        Ok(()) => Ok(0),
        Err(Errno(ERESTARTSYS)) => match deadline {
            None => Err(Errno(ERESTARTSYS)),
            Some(deadline) => {
                *task::current().restart_block.lock() = Some(RestartBlock::FutexWait {
                    uaddr,
                    val,
                    bits,
                    deadline,
                });
                Err(Errno(ERESTART_RESTARTBLOCK))
            }
        },
        Err(e) => Err(e),
    }
}

/// `sched_yield`.
pub fn sched_yield() -> SysResult {
    sched::yield_now();
    Ok(0)
}
