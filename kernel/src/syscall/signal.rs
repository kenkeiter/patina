//! Signal syscalls (design §6): dispositions, masks, alternate stacks,
//! sending (`kill`/`tgkill`/`tkill`), `rt_sigreturn`, `rt_sigsuspend`,
//! `rt_sigpending`, `pause` and `restart_syscall`. Queueing and delivery
//! live in `task::signal`.

use core::sync::atomic::Ordering;

use patina_abi::errno::{EINTR, EINVAL, ESRCH, Errno};
use patina_abi::signal::{
    NSIG, SI_TKILL, SI_USER, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK, SIGKILL, SIGSET_SIZE, SIGSTOP,
    SigAction, SigInfo, SigSet, StackT, UNBLOCKABLE,
};

use super::{SysResult, with_space};
use crate::arch::ctx::TrapFrame;
use crate::task::{self, signal};

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
    // Linux also discards pending instances of a signal that becomes
    // ignored; harmless to skip: delivery drops ignored signals.
    if oact != 0 {
        with_space(|s| s.write_val(oact, &old))?;
    }
    Ok(0)
}

/// `rt_sigprocmask`. Newly unblocked pending signals are delivered on the
/// way back to user mode (the delivery fast path compares the pending
/// bits with the new mask).
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
    let old = t.blocked();
    if let Some(n) = new {
        let m = match how {
            SIG_BLOCK => old | n,
            SIG_UNBLOCK => old & !n,
            SIG_SETMASK => n,
            _ => return Err(Errno(EINVAL)),
        } & !UNBLOCKABLE;
        t.blocked.store(m, Ordering::SeqCst);
    }
    if oset != 0 {
        with_space(|s| s.write_val(oset, &old))?;
    }
    Ok(0)
}

/// `sigaltstack(ss, oss)`; `sp` is the caller's user stack pointer
/// (`EPERM` while running on the alternate stack).
pub fn sigaltstack(ss: usize, oss: usize, sp: u64) -> SysResult {
    let new: Option<StackT> = if ss != 0 {
        Some(with_space(|s| s.read_val(ss))?)
    } else {
        None
    };
    let old = signal::do_sigaltstack(new, sp)?;
    if oss != 0 {
        with_space(|s| s.write_val(oss, &old))?;
    }
    Ok(0)
}

/// `rt_sigreturn`: restores `frame` from the signal frame on the user
/// stack (the return register included; dispatch does not overwrite it).
pub fn rt_sigreturn(frame: &mut TrapFrame) {
    signal::sigreturn(frame);
}

/// `rt_sigsuspend(mask, sigsetsize)`.
pub fn rt_sigsuspend(mask: usize, sigsetsize: usize) -> SysResult {
    if sigsetsize != SIGSET_SIZE {
        return Err(Errno(EINVAL));
    }
    let m: SigSet = with_space(|s| s.read_val(mask))?;
    Err(signal::sigsuspend(m))
}

/// x86_64 `pause()`.
pub fn pause() -> SysResult {
    Err(signal::pause())
}

/// `rt_sigpending(set, sigsetsize)`: blocked pending signals.
pub fn rt_sigpending(set: usize, sigsetsize: usize) -> SysResult {
    if sigsetsize > SIGSET_SIZE {
        return Err(Errno(EINVAL));
    }
    let p = signal::pending_blocked();
    with_space(|s| s.copy_to_user(set, &p.to_le_bytes()[..sigsetsize]))?;
    Ok(0)
}

/// `restart_syscall`: continue a call that returned
/// `ERESTART_RESTARTBLOCK` (`EINTR` if there is nothing to continue).
pub fn restart_syscall() -> SysResult {
    let blk = task::current().restart_block.lock().take();
    match blk {
        Some(signal::RestartBlock::Nanosleep { deadline, rem }) => {
            super::time::sleep_restartable(deadline, rem)
        }
        Some(signal::RestartBlock::FutexWait {
            uaddr,
            val,
            bits,
            deadline,
        }) => super::thread::futex_wait(uaddr, val, bits, Some(deadline)),
        None => Err(Errno(EINTR)),
    }
}

/// `kill(pid, sig)`: the process itself (pid, 0, -1 or -pid). Signal 0
/// only checks the target.
pub fn kill(pid: i32, sig: i32) -> SysResult {
    let me = task::process().pid as i32;
    if !(pid == me || pid == 0 || pid == -1 || pid == -me) {
        return Err(Errno(ESRCH));
    }
    if sig == 0 {
        return Ok(0);
    }
    if !valid_sig(sig) {
        return Err(Errno(EINVAL));
    }
    signal::send_to_process(SigInfo::kill(sig, SI_USER, me, 0));
    Ok(0)
}

/// `tgkill(tgid, tid, sig)`: the target must be a live thread of the
/// process.
pub fn tgkill(tgid: i32, tid: i32, sig: i32) -> SysResult {
    if tgid <= 0 || tid <= 0 {
        return Err(Errno(EINVAL));
    }
    if tgid != task::process().pid as i32 {
        return Err(Errno(ESRCH));
    }
    tkill(tid, sig)
}

/// `tkill(tid, sig)`.
pub fn tkill(tid: i32, sig: i32) -> SysResult {
    if tid <= 0 || !(0..=NSIG).contains(&sig) {
        return Err(Errno(EINVAL));
    }
    let Some(t) = task::thread_by_tid(tid as u32) else {
        return Err(Errno(ESRCH));
    };
    if sig != 0 {
        let me = task::process().pid as i32;
        signal::send_to_thread(&t, SigInfo::kill(sig, SI_TKILL, me, 0));
    }
    Ok(0)
}
