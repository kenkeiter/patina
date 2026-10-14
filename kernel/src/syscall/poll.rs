//! Readiness syscalls: epoll (`epoll_create1`, `epoll_ctl`, `epoll_pwait`,
//! `epoll_pwait2`, x86 `epoll_create`/`epoll_wait`) and `ppoll` / x86
//! `poll`, all over `File::poll` (see `fs/file.rs`, `fs/epoll.rs`).
//!
//! Signals: the waits are interruptible. `epoll_pwait` returns `EINTR`
//! (never restarted, as Linux); `ppoll`/`poll`/`select` return `ERESTARTNOHAND`
//! (`EINTR` when a handler runs, transparently re-executed with the
//! original timeout otherwise). The temporary `sigmask` of
//! `epoll_pwait`/`ppoll` stays installed when the wait was interrupted, so
//! the handler runs under it and the frame saves the original mask (Linux
//! `saved_sigmask` / `restore_saved_sigmask_unless`); otherwise the
//! original mask is restored on return.

use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::current::EpollEvent;
use patina_abi::epoll::{EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EpollFlags};
use patina_abi::errno::{EBADF, EFAULT, EINTR, EINVAL, ENOMEM, EPERM, Errno};
use patina_abi::fs::{O_RDWR, POLLNVAL, PollFd};
use patina_abi::signal::{SIGSET_SIZE, SigSet};
use patina_abi::time::{Timespec, Timeval};
use patina_abi::zerocopy::{FromZeros, IntoBytes};

use super::fs::{get, install};
use super::{SysResult, proc, with_space};
use crate::arch::timer;
use crate::fs::epoll::Epoll;
use crate::fs::file::{OpenFile, PollHook, PollWaiter};
use crate::mm::vm::access_ok;
use crate::task::{WakeReason, signal};

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
/// at `mask` (if non-NULL). If `f` was interrupted by a signal (`EINTR` /
/// `ERESTARTNOHAND`) the temporary mask stays until signal delivery
/// restores the saved one.
fn with_sigmask<R>(
    mask: usize,
    size: usize,
    f: impl FnOnce() -> Result<R, Errno>,
) -> Result<R, Errno> {
    if mask == 0 {
        return f();
    }
    if size != SIGSET_SIZE {
        return Err(Errno(EINVAL));
    }
    let m: SigSet = with_space(|s| s.read_val(mask))?;
    signal::set_temp_mask(m);
    let r = f();
    if !matches!(r, Err(Errno(EINTR)) | Err(Errno(signal::ERESTARTNOHAND))) {
        signal::restore_saved_mask();
    }
    r
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

/// `epoll_ctl`, in Linux's order (`fs/eventpoll.c`): the event is copied
/// in first unless `op` is `EPOLL_CTL_DEL` (EFAULT, also for an unknown
/// op), then `epfd` and `fd` (EBADF), a target that cannot be polled
/// (EPERM), `epfd` not an epoll or the same file (EINVAL), the
/// `EPOLLEXCLUSIVE` rules (EINVAL), and an unknown `op` last (EINVAL).
pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: usize) -> SysResult {
    let e: Option<EpollEvent> = if op != EPOLL_CTL_DEL {
        Some(with_space(|s| s.read_val(event))?)
    } else {
        None
    };
    let ep_of = get(epfd)?;
    let of = get(fd)?;
    if !of.file.pollable() {
        return Err(Errno(EPERM));
    }
    let ep = ep_of.file.as_epoll().ok_or(Errno(EINVAL))?;
    if Arc::ptr_eq(&ep_of, &of) {
        return Err(Errno(EINVAL));
    }
    if let Some(e) = &e {
        let ev = e.flags();
        if ev.contains(EpollFlags::EXCLUSIVE) {
            let ok = EpollFlags::IN
                | EpollFlags::OUT
                | EpollFlags::ERR
                | EpollFlags::HUP
                | EpollFlags::WAKEUP
                | EpollFlags::ET
                | EpollFlags::EXCLUSIVE;
            if op == EPOLL_CTL_MOD
                || (op == EPOLL_CTL_ADD && (of.file.as_epoll().is_some() || !(ev - ok).is_empty()))
            {
                return Err(Errno(EINVAL));
            }
        }
    }
    match (op, e) {
        (EPOLL_CTL_ADD, Some(e)) => ep.add(fd, &of, e.flags(), e.data())?,
        (EPOLL_CTL_MOD, Some(e)) => ep.modify(fd, &of, e.flags(), e.data())?,
        (EPOLL_CTL_DEL, _) => ep.delete(fd, &of)?,
        _ => return Err(Errno(EINVAL)),
    }
    Ok(0)
}

/// `epoll_pwait`/`epoll_pwait2` after the timeout was read, in Linux's
/// order (`fs/eventpoll.c do_epoll_pwait`, `do_epoll_wait`): the signal
/// mask (`sigsetsize` → EINVAL, copy → EFAULT), `maxevents` → EINVAL,
/// `access_ok(events)` → EFAULT, the fd → EBADF, not an epoll → EINVAL.
/// Events are collected only for the writable part of `events` (D86); one
/// that still cannot be copied out is put back on the ready list.
fn epoll_wait_common(
    epfd: i32,
    events: usize,
    maxevents: i32,
    tmo: Timeout,
    sigmask: usize,
    sigsetsize: usize,
) -> SysResult {
    with_sigmask(sigmask, sigsetsize, || {
        const EV: usize = size_of::<EpollEvent>();
        if maxevents <= 0 || maxevents as usize > i32::MAX as usize / EV {
            return Err(Errno(EINVAL));
        }
        let bytes = maxevents as usize * EV;
        if !access_ok(events, bytes) {
            return Err(Errno(EFAULT));
        }
        let of = get(epfd)?;
        let ep = of.file.as_epoll().ok_or(Errno(EINVAL))?;
        let room = with_space(|s| s.writable_prefix(events, bytes)) / EV;
        if room == 0 {
            return Err(Errno(EFAULT));
        }
        let evs = ep.wait(room, tmo.block, tmo.deadline)?;
        // Copy event by event: a fault keeps the rest on the ready list.
        let mut done = 0;
        let r = with_space(|s| {
            for r in &evs {
                s.write_val(events + done * EV, &r.ev)?;
                done += 1;
            }
            Ok::<(), Errno>(())
        });
        if let Err(e) = r {
            ep.unreport(&evs[done..]);
            if done == 0 {
                return Err(if e == Errno(ENOMEM) { e } else { Errno(EFAULT) });
            }
        }
        Ok(done as isize)
    })
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

/// `ppoll`/`poll` after the timeout was read, in Linux's order
/// (`fs/select.c`): the signal mask first (`set_user_sigmask`:
/// `sigsetsize` → EINVAL, copy → EFAULT), then `do_sys_poll` (`nfds` over
/// `RLIMIT_NOFILE` → EINVAL, the array → EFAULT).
fn do_poll(fds: usize, nfds: usize, tmo: Timeout, sigmask: usize, sigsetsize: usize) -> SysResult {
    with_sigmask(sigmask, sigsetsize, || {
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
        let mut block = tmo.block;
        let n = loop {
            if !block {
                break poll_once(&mut pfds, &files, None);
            }
            let w = PollWaiter::new();
            let hook: Arc<dyn PollHook> = w.clone();
            let n = poll_once(&mut pfds, &files, Some(&hook));
            if n > 0 {
                break n;
            }
            match w.wait(tmo.deadline) {
                WakeReason::TimedOut => block = false,
                WakeReason::Interrupted => return Err(Errno(signal::ERESTARTNOHAND)),
                WakeReason::Woken => {}
            }
        };
        if nfds > 0 {
            with_space(|s| s.copy_to_user(fds, pfds.as_slice().as_bytes()))?;
        }
        Ok(n as isize)
    })
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

/// `select` readiness classes (Linux `POLLIN_SET`, `POLLOUT_SET`,
/// `POLLEX_SET`), indexed like the read / write / except `fd_set`s.
const SELECT_SETS: [EpollFlags; 3] = [
    EpollFlags::IN
        .union(EpollFlags::RDNORM)
        .union(EpollFlags::RDBAND)
        .union(EpollFlags::HUP)
        .union(EpollFlags::ERR),
    EpollFlags::OUT
        .union(EpollFlags::WRNORM)
        .union(EpollFlags::WRBAND)
        .union(EpollFlags::ERR),
    EpollFlags::PRI,
];

/// The common part of `select` and `pselect6`: `sets` are the user
/// read / write / except `fd_set` pointers (0 = none), each
/// `nfds.div_ceil(64)` 64-bit words as on Linux (`unsigned long` bitmaps).
/// Returns the number of set bits written back, as Linux.
fn do_select(
    nfds: i32,
    sets: [usize; 3],
    tmo: Timeout,
    sigmask: usize,
    sigsetsize: usize,
) -> SysResult {
    // Linux do_pselect: set_user_sigmask before core_sys_select.
    with_sigmask(sigmask, sigsetsize, || select_inner(nfds, sets, tmo))
}

fn select_inner(nfds: i32, sets: [usize; 3], tmo: Timeout) -> SysResult {
    if nfds < 0 {
        return Err(Errno(EINVAL));
    }
    // Linux clamps to the descriptor table size.
    let nfds = (nfds as usize).min(proc::nofile_limit());
    let nwords = nfds.div_ceil(64);
    let mut inp: [Vec<u64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for (k, &p) in sets.iter().enumerate() {
        inp[k].resize(nwords, 0);
        if p != 0 && nwords > 0 {
            with_space(|s| s.copy_from_user(p, inp[k].as_mut_slice().as_mut_bytes()))?;
        }
        // Bits at or above nfds are ignored.
        if !nfds.is_multiple_of(64) {
            inp[k][nwords - 1] &= (1u64 << (nfds % 64)) - 1;
        }
    }
    // The descriptors named in any set; one that is not open is EBADF.
    let mut fds: Vec<(usize, Arc<OpenFile>)> = Vec::new();
    for fd in 0..nfds {
        let (w, b) = (fd / 64, 1u64 << (fd % 64));
        if inp.iter().any(|v| v[w] & b != 0) {
            fds.push((fd, get(fd as i32).map_err(|_| Errno(EBADF))?));
        }
    }
    let mut out: [Vec<u64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for o in &mut out {
        o.resize(nwords, 0);
    }
    let scan = |out: &mut [Vec<u64>; 3], hook: Option<&Arc<dyn PollHook>>| -> usize {
        let mut n = 0;
        for (fd, of) in &fds {
            let (w, b) = (fd / 64, 1u64 << (fd % 64));
            let ev = of.file.poll(hook);
            for k in 0..3 {
                if inp[k][w] & b != 0 && ev.intersects(SELECT_SETS[k]) {
                    out[k][w] |= b;
                    n += 1;
                }
            }
        }
        n
    };
    let mut block = tmo.block;
    let n = loop {
        if !block {
            break scan(&mut out, None);
        }
        let w = PollWaiter::new();
        let hook: Arc<dyn PollHook> = w.clone();
        let n = scan(&mut out, Some(&hook));
        if n > 0 {
            break n;
        }
        match w.wait(tmo.deadline) {
            WakeReason::TimedOut => block = false,
            WakeReason::Interrupted => return Err(Errno(signal::ERESTARTNOHAND)),
            WakeReason::Woken => {}
        }
    };
    for (k, &p) in sets.iter().enumerate() {
        if p != 0 && nwords > 0 {
            with_space(|s| s.copy_to_user(p, out[k].as_slice().as_bytes()))?;
        }
    }
    Ok(n as isize)
}

/// Nanoseconds left until `tmo`'s deadline (0 once it has passed).
fn remaining_ns(tmo: &Timeout) -> u64 {
    tmo.deadline
        .map_or(0, |d| d.saturating_sub(timer::now_ns()))
}

/// `pselect6(nfds, readfds, writefds, exceptfds, tmo_p, sig)`, where
/// `sig` points at `{ const sigset_t *ss; size_t ss_len; }`. As Linux, the
/// time left is written back to a non-zero `*tmo_p`, so a restarted call
/// waits only for the remainder.
pub fn pselect6(nfds: i32, r: usize, w: usize, e: usize, tmo_p: usize, sig: usize) -> SysResult {
    // Linux: the sigset argpack (EFAULT), then the timeout, then the mask.
    let (mask, size) = if sig == 0 {
        (0, 0)
    } else {
        let v: [u64; 2] = with_space(|s| s.read_val(sig))?;
        (v[0] as usize, v[1] as usize)
    };
    let tmo = Timeout::from_timespec(tmo_p)?;
    let res = do_select(nfds, [r, w, e], tmo, mask, size);
    if tmo_p != 0 && tmo.block && tmo.deadline.is_some() {
        let left = Timespec::from_nanos(remaining_ns(&tmo));
        with_space(|s| s.write_val(tmo_p, &left))?;
    }
    res
}

/// x86_64 `select(nfds, readfds, writefds, exceptfds, struct timeval *)`.
/// `tv_usec` of a million or more carries into seconds, as Linux; the
/// time left is written back to a non-zero `*tv`.
pub fn select(nfds: i32, r: usize, w: usize, e: usize, tv_p: usize) -> SysResult {
    let tmo = if tv_p == 0 {
        Timeout {
            block: true,
            deadline: None,
        }
    } else {
        let tv: Timeval = with_space(|s| s.read_val(tv_p))?;
        if tv.tv_sec < 0 || tv.tv_usec < 0 {
            return Err(Errno(EINVAL));
        }
        let ns = (tv.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((tv.tv_usec as u64).saturating_mul(1000));
        Timeout {
            block: ns != 0,
            deadline: Some(timer::now_ns().saturating_add(ns)),
        }
    };
    let res = do_select(nfds, [r, w, e], tmo, 0, 0);
    if tv_p != 0 && tmo.block {
        let left = Timeval::from_nanos(remaining_ns(&tmo));
        with_space(|s| s.write_val(tv_p, &left))?;
    }
    res
}
