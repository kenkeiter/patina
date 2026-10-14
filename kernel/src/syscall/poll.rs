//! Readiness syscalls: epoll (`epoll_create1`, `epoll_ctl`, `epoll_pwait`,
//! `epoll_pwait2`, x86 `epoll_create`/`epoll_wait`) and `ppoll` / x86
//! `poll`, all over `File::poll` (see `fs/file.rs`, `fs/epoll.rs`).
//!
//! Signals: the temporary `sigmask` of `epoll_pwait`/`ppoll` is installed
//! for the wait and restored afterwards. The waits are not interruptible
//! yet. TODO(signals): use the interruptible block (→ `EINTR`) in
//! `Epoll::wait` and `PollWaiter::wait`, and keep the temporary mask
//! installed until a handler run for a signal that interrupted the wait
//! returns (Linux `saved_sigmask` / `restore_saved_sigmask_unless`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::current::EpollEvent;
use patina_abi::epoll::{EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EpollFlags};
use patina_abi::errno::{EINVAL, Errno};
use patina_abi::fs::{O_RDWR, POLLNVAL, PollFd};
use patina_abi::signal::{SIGSET_SIZE, SigSet, UNBLOCKABLE};
use patina_abi::time::Timespec;
use patina_abi::zerocopy::{FromZeros, IntoBytes};

use super::fs::{get, install};
use super::{SysResult, proc, with_space};
use crate::arch::timer;
use crate::fs::epoll::Epoll;
use crate::fs::file::{OpenFile, PollHook, PollWaiter};
use crate::task::{self, WakeReason};

/// How long to wait: `block == false` → return at once.
#[derive(Clone, Copy)]
struct Timeout {
    block: bool,
    deadline: Option<u64>,
}

impl Timeout {
    /// From milliseconds (`-1`/negative: forever, `0`: poll).
    fn from_ms(ms: i32) -> Timeout {
        match ms {
            ms if ms < 0 => Timeout {
                block: true,
                deadline: None,
            },
            0 => Timeout {
                block: false,
                deadline: None,
            },
            ms => Timeout {
                block: true,
                deadline: Some(timer::now_ns().saturating_add(ms as u64 * 1_000_000)),
            },
        }
    }

    /// From a user `struct timespec *` (NULL: forever).
    fn from_timespec(p: usize) -> Result<Timeout, Errno> {
        if p == 0 {
            return Ok(Timeout {
                block: true,
                deadline: None,
            });
        }
        let ts: Timespec = with_space(|s| s.read_val(p))?;
        let ns = ts.to_nanos().ok_or(Errno(EINVAL))?;
        Ok(Timeout {
            block: ns != 0,
            deadline: Some(timer::now_ns().saturating_add(ns)),
        })
    }
}

/// Run `f` with the thread's signal mask temporarily set to the user mask
/// at `mask` (if non-NULL).
fn with_sigmask<R>(mask: usize, size: usize, f: impl FnOnce() -> R) -> Result<R, Errno> {
    if mask == 0 {
        return Ok(f());
    }
    if size != SIGSET_SIZE {
        return Err(Errno(EINVAL));
    }
    let m: SigSet = with_space(|s| s.read_val(mask))?;
    let t = task::current();
    let old = {
        let mut st = t.sig.lock();
        let old = st.mask;
        st.mask = m & !UNBLOCKABLE;
        old
    };
    let r = f();
    t.sig.lock().mask = old;
    Ok(r)
}

pub fn epoll_create1(flags: u32) -> SysResult {
    if flags & !EPOLL_CLOEXEC != 0 {
        return Err(Errno(EINVAL));
    }
    install(OpenFile::new(Epoll::new(), O_RDWR), flags != 0)
}

/// x86_64 `epoll_create(size)`: `size` must be positive, otherwise unused.
pub fn epoll_create(size: i32) -> SysResult {
    if size <= 0 {
        return Err(Errno(EINVAL));
    }
    epoll_create1(0)
}

pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: usize) -> SysResult {
    let ep_of = get(epfd)?;
    let of = get(fd)?;
    let ep = ep_of.file.as_epoll().ok_or(Errno(EINVAL))?;
    if Arc::ptr_eq(&ep_of, &of) {
        return Err(Errno(EINVAL));
    }
    match op {
        EPOLL_CTL_ADD | EPOLL_CTL_MOD => {
            let e: EpollEvent = with_space(|s| s.read_val(event))?;
            let ev = e.flags();
            if ev.contains(EpollFlags::EXCLUSIVE) {
                let ok = EpollFlags::IN
                    | EpollFlags::OUT
                    | EpollFlags::ERR
                    | EpollFlags::HUP
                    | EpollFlags::WAKEUP
                    | EpollFlags::ET
                    | EpollFlags::EXCLUSIVE;
                if op == EPOLL_CTL_MOD || !(ev - ok).is_empty() {
                    return Err(Errno(EINVAL));
                }
            }
            if op == EPOLL_CTL_ADD {
                ep.add(fd, &of, ev, e.data())?;
            } else {
                ep.modify(fd, &of, ev, e.data())?;
            }
        }
        EPOLL_CTL_DEL => ep.delete(fd, &of)?,
        _ => return Err(Errno(EINVAL)),
    }
    Ok(0)
}

fn epoll_wait_common(
    epfd: i32,
    events: usize,
    maxevents: i32,
    tmo: Timeout,
    sigmask: usize,
    sigsetsize: usize,
) -> SysResult {
    if maxevents <= 0 || maxevents as usize > i32::MAX as usize / size_of::<EpollEvent>() {
        return Err(Errno(EINVAL));
    }
    let of = get(epfd)?;
    let ep = of.file.as_epoll().ok_or(Errno(EINVAL))?;
    let evs = with_sigmask(sigmask, sigsetsize, || {
        ep.wait(maxevents as usize, tmo.block, tmo.deadline)
    })?;
    if !evs.is_empty() {
        with_space(|s| s.copy_to_user(events, evs.as_slice().as_bytes()))?;
    }
    Ok(evs.len() as isize)
}

pub fn epoll_pwait(
    epfd: i32,
    events: usize,
    maxevents: i32,
    timeout_ms: i32,
    sigmask: usize,
    sigsetsize: usize,
) -> SysResult {
    let tmo = Timeout::from_ms(timeout_ms);
    epoll_wait_common(epfd, events, maxevents, tmo, sigmask, sigsetsize)
}

pub fn epoll_pwait2(
    epfd: i32,
    events: usize,
    maxevents: i32,
    timeout: usize,
    sigmask: usize,
    sigsetsize: usize,
) -> SysResult {
    let tmo = Timeout::from_timespec(timeout)?;
    epoll_wait_common(epfd, events, maxevents, tmo, sigmask, sigsetsize)
}

/// Poll `pfds` once (registering `hook` on every file when given);
/// returns the number with non-zero `revents`.
fn poll_once(
    pfds: &mut [PollFd],
    files: &[Option<Arc<OpenFile>>],
    hook: Option<&Arc<dyn PollHook>>,
) -> usize {
    let mut n = 0;
    for (p, f) in pfds.iter_mut().zip(files) {
        p.revents = 0;
        if p.fd < 0 {
            continue;
        }
        p.revents = match f {
            None => POLLNVAL,
            Some(of) => {
                let ev = of.file.poll(hook);
                let mask = p.events as u16 as u32 | (EpollFlags::ERR | EpollFlags::HUP).bits();
                (ev.bits() & mask) as u16 as i16
            }
        };
        if p.revents != 0 {
            n += 1;
        }
    }
    n
}

fn do_poll(fds: usize, nfds: usize, tmo: Timeout, sigmask: usize, sigsetsize: usize) -> SysResult {
    if nfds > proc::nofile_limit() {
        return Err(Errno(EINVAL));
    }
    let mut pfds: Vec<PollFd> = Vec::new();
    pfds.resize(nfds, PollFd::new_zeroed());
    if nfds > 0 {
        with_space(|s| s.copy_from_user(fds, pfds.as_mut_slice().as_mut_bytes()))?;
    }
    let files: Vec<Option<Arc<OpenFile>>> = pfds
        .iter()
        .map(|p| if p.fd < 0 { None } else { get(p.fd).ok() })
        .collect();
    let n = with_sigmask(sigmask, sigsetsize, || {
        let mut block = tmo.block;
        loop {
            if !block {
                return poll_once(&mut pfds, &files, None);
            }
            let w = PollWaiter::new();
            let hook: Arc<dyn PollHook> = w.clone();
            let n = poll_once(&mut pfds, &files, Some(&hook));
            if n > 0 {
                return n;
            }
            if w.wait(tmo.deadline) == WakeReason::TimedOut {
                block = false;
            }
        }
    })?;
    if nfds > 0 {
        with_space(|s| s.copy_to_user(fds, pfds.as_slice().as_bytes()))?;
    }
    Ok(n as isize)
}

/// `ppoll(fds, nfds, tmo_p, sigmask, sigsetsize)`. The remaining time is
/// not written back to `tmo_p`.
pub fn ppoll(fds: usize, nfds: usize, tmo: usize, sigmask: usize, sigsetsize: usize) -> SysResult {
    do_poll(fds, nfds, Timeout::from_timespec(tmo)?, sigmask, sigsetsize)
}

/// x86_64 `poll(fds, nfds, timeout_ms)`.
pub fn poll(fds: usize, nfds: usize, timeout_ms: i32) -> SysResult {
    do_poll(fds, nfds, Timeout::from_ms(timeout_ms), 0, 0)
}
