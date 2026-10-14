//! Signal bookkeeping (design §6). Phase 1 stores dispositions, masks and
//! alternate stacks with Linux validation but never delivers anything;
//! delivery (and `rt_sigreturn`) is Phase 2.

use core::sync::atomic::AtomicU64;

use patina_abi::current::MINSIGSTKSZ;
use patina_abi::errno::{EINVAL, ENOMEM, ESRCH, Errno};
use patina_abi::signal::{
    NSIG, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK, SIGKILL, SIGSET_SIZE, SIGSTOP, SS_AUTODISARM,
    SS_DISABLE, SS_ONSTACK, SigAction, SigSet, StackT, UNBLOCKABLE,
};

use super::{SysResult, first_time, with_space};
use crate::arch::ctx::TrapFrame;
use crate::task;

fn valid_sig(sig: i32) -> bool {
    (1..=NSIG).contains(&sig)
}

pub fn rt_sigaction(sig: i32, act: usize, oact: usize, sigsetsize: usize) -> SysResult {
    if sigsetsize != SIGSET_SIZE
        || !valid_sig(sig)
        || (act != 0 && (sig == SIGKILL || sig == SIGSTOP))
    {
        return Err(Errno(EINVAL));
    }
    let new = if act != 0 {
        let mut a: SigAction = with_space(|s| s.read_val(act))?;
        a.sa_mask &= !UNBLOCKABLE;
        Some(a)
    } else {
        None
    };
    let p = task::process();
    let old = {
        let mut tab = p.sigactions.lock();
        let old = tab[sig as usize];
        if let Some(a) = new {
            tab[sig as usize] = a;
        }
        old
    };
    if oact != 0 {
        with_space(|s| s.write_val(oact, &old))?;
    }
    Ok(0)
}

pub fn rt_sigprocmask(how: i32, set: usize, oset: usize, sigsetsize: usize) -> SysResult {
    if sigsetsize != SIGSET_SIZE {
        return Err(Errno(EINVAL));
    }
    let new: Option<SigSet> = if set != 0 {
        Some(with_space(|s| s.read_val(set))?)
    } else {
        None
    };
    let t = task::current();
    let old = {
        let mut st = t.sig.lock();
        let old = st.mask;
        if let Some(n) = new {
            st.mask = match how {
                SIG_BLOCK => old | n,
                SIG_UNBLOCK => old & !n,
                SIG_SETMASK => n,
                _ => return Err(Errno(EINVAL)),
            } & !UNBLOCKABLE;
        }
        old
    };
    if oset != 0 {
        with_space(|s| s.write_val(oset, &old))?;
    }
    Ok(0)
}

pub fn sigaltstack(ss: usize, oss: usize) -> SysResult {
    let new: Option<StackT> = if ss != 0 {
        Some(with_space(|s| s.read_val(ss))?)
    } else {
        None
    };
    let t = task::current();
    let old = {
        let mut st = t.sig.lock();
        let old = st.altstack;
        if let Some(mut n) = new {
            // W2b/Phase 2: EPERM while executing on the alternate stack.
            let mode = n.ss_flags & !SS_AUTODISARM;
            if mode != 0 && mode != SS_DISABLE && mode != SS_ONSTACK {
                return Err(Errno(EINVAL));
            }
            if mode == SS_DISABLE {
                n.ss_sp = 0;
                n.ss_size = 0;
                n.ss_flags = SS_DISABLE;
            } else {
                if (n.ss_size as usize) < MINSIGSTKSZ {
                    return Err(Errno(ENOMEM));
                }
                n.ss_flags &= SS_AUTODISARM;
            }
            n._pad = 0;
            st.altstack = n;
        }
        old
    };
    if oss != 0 {
        with_space(|s| s.write_val(oss, &old))?;
    }
    Ok(0)
}

/// Never valid before Phase 2 delivers signals: fatal.
pub fn rt_sigreturn(frame: &mut TrapFrame) -> SysResult {
    crate::kprintln!(
        "[patina] fatal: rt_sigreturn without a delivered signal (delivery is Phase 2)"
    );
    frame.dump();
    crate::exit(128 + 11)
}

static QUEUED_SEEN: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

fn queue(sig: i32, tid: i32) -> SysResult {
    if sig == 0 {
        return Ok(0);
    }
    if !valid_sig(sig) {
        return Err(Errno(EINVAL));
    }
    if first_time(&QUEUED_SEEN, sig as usize) {
        crate::kprintln!(
            "[patina] signal {} to tid {} queued (delivery is Phase 2)",
            sig,
            tid
        );
    }
    Ok(0)
}

pub fn kill(pid: i32, sig: i32) -> SysResult {
    let me = task::process().pid as i32;
    if pid == me || pid == 0 || pid == -1 || pid == -me {
        queue(sig, me)
    } else {
        Err(Errno(ESRCH))
    }
}

/// `tgkill(tgid, tid, sig)` (and `tkill` with tgid = pid).
/// The target must be a live thread of the process. Nothing is delivered
/// before Phase 2 (Go's async preemption SIGURG is simply dropped; Go
/// falls back to cooperative preemption).
pub fn tgkill(tgid: i32, tid: i32, sig: i32) -> SysResult {
    if tgid <= 0 || tid <= 0 {
        return Err(Errno(EINVAL));
    }
    if tgid != task::process().pid as i32 || task::thread_by_tid(tid as u32).is_none() {
        return Err(Errno(ESRCH));
    }
    queue(sig, tid)
}
