//! File descriptors and paths. Fds go through the process's fd table
//! (`fs::FdTable`) to `fs::File` objects: the console (0/1/2), pipes,
//! eventfds and epoll instances. There is no filesystem yet (Phase 3), so
//! every path lookup still fails with `ENOENT`.
//!
//! Blocking: file operations never block; the loops here retry after
//! `fs::file::wait_ready` unless the description is `O_NONBLOCK` (see
//! `fs/file.rs`).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use patina_abi::current::O_DIRECT;
use patina_abi::epoll::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE, EpollFlags};
use patina_abi::errno::{EAGAIN, EBADF, EFAULT, EINVAL, ENOENT, EPIPE, Errno};
use patina_abi::fs::{
    AT_EMPTY_PATH, F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC, Iovec,
    O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, UIO_MAXIOV,
};
use patina_abi::misc::{FIOCLEX, FIONBIO, FIONCLEX};

use super::{SysResult, proc, with_space};
use crate::fs::file::{OpenFile, wait_ready};
use crate::fs::{eventfd, pipe};
use crate::sync::SpinLock;
use crate::task::signal::{self, ERESTARTSYS};
use crate::task::{self, WakeReason};
use patina_abi::signal::{SI_USER, SIGPIPE, SigInfo};

const PATH_MAX: usize = 4096;
/// Kernel bounce buffer per read/write step.
const CHUNK: usize = 64 * 1024;
/// Linux `MAX_RW_COUNT` (`INT_MAX & PAGE_MASK`).
const MAX_RW: usize = 0x7fff_f000;

/// The open file at `fd`.
pub fn get(fd: i32) -> Result<Arc<OpenFile>, Errno> {
    task::process().files.lock().get(fd)
}

/// Install `of` at the lowest free fd.
pub fn install(of: Arc<OpenFile>, cloexec: bool) -> SysResult {
    let lim = proc::nofile_limit();
    let fd = task::process().files.lock().alloc(of, cloexec, 0, lim)?;
    Ok(fd as isize)
}

/// `n > 0` bytes done before error `e`: report the bytes, else the error.
fn partial(n: usize, e: Errno) -> SysResult {
    if n > 0 { Ok(n as isize) } else { Err(e) }
}

/// One read into `kbuf`, blocking unless `O_NONBLOCK`.
fn read_into(of: &OpenFile, kbuf: &mut [u8]) -> Result<usize, Errno> {
    loop {
        let fl = of.flags();
        match of.file.read(kbuf, fl) {
            Err(Errno(EAGAIN)) if !fl.nonblocking() => {
                if wait_ready(&*of.file, EpollFlags::IN | EpollFlags::RDNORM, None)
                    == WakeReason::Interrupted
                {
                    return Err(Errno(ERESTARTSYS));
                }
            }
            r => return r,
        }
    }
}

/// Write `len` user bytes at `ubuf`, blocking unless `O_NONBLOCK`.
fn write_from(of: &OpenFile, ubuf: usize, len: usize) -> SysResult {
    if len == 0 {
        return of.file.write(&[], of.flags()).map(|n| n as isize);
    }
    let mut k = vec![0u8; len.min(CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(CHUNK);
        if let Err(e) = with_space(|s| s.copy_from_user(ubuf + done, &mut k[..n])) {
            return partial(done, e);
        }
        let mut off = 0;
        while off < n {
            let fl = of.flags();
            match of.file.write(&k[off..n], fl) {
                Ok(0) => return Ok((done + off) as isize),
                Ok(w) => off += w,
                Err(Errno(EAGAIN)) if !fl.nonblocking() => {
                    if wait_ready(&*of.file, EpollFlags::OUT | EpollFlags::WRNORM, None)
                        == WakeReason::Interrupted
                    {
                        // Linux pipe_write: bytes written so far, else
                        // -ERESTARTSYS.
                        return partial(done + off, Errno(ERESTARTSYS));
                    }
                }
                Err(e) => {
                    if e == Errno(EPIPE) {
                        // Linux `send_sig(SIGPIPE, current, 0)`.
                        let pid = task::process().pid as i32;
                        signal::send_to_thread(
                            &task::sched::current_arc(),
                            SigInfo::kill(SIGPIPE, SI_USER, pid, 0),
                        );
                    }
                    return partial(done + off, e);
                }
            }
        }
        done += n;
    }
    Ok(done as isize)
}

pub fn read(fd: i32, buf: usize, len: usize) -> SysResult {
    let of = get(fd)?;
    if !of.flags().readable() {
        return Err(Errno(EBADF));
    }
    let mut k = vec![0u8; len.min(MAX_RW).min(CHUNK)];
    let n = read_into(&of, &mut k)?;
    with_space(|s| s.copy_to_user(buf, &k[..n]))?;
    Ok(n as isize)
}

pub fn write(fd: i32, buf: usize, len: usize) -> SysResult {
    let of = get(fd)?;
    if !of.flags().writable() {
        return Err(Errno(EBADF));
    }
    write_from(&of, buf, len.min(MAX_RW))
}

/// Read an iovec array: (base, len) pairs.
fn read_iov(iov: usize, cnt: usize) -> Result<Vec<(usize, usize)>, Errno> {
    if cnt > UIO_MAXIOV {
        return Err(Errno(EINVAL));
    }
    let mut v = Vec::with_capacity(cnt);
    let mut total = 0usize;
    for i in 0..cnt {
        let e: Iovec = with_space(|s| s.read_val(iov + i * size_of::<Iovec>()))?;
        if e.iov_len > isize::MAX as u64 {
            return Err(Errno(EINVAL));
        }
        let len = (e.iov_len as usize).min(MAX_RW - total);
        total += len;
        v.push((e.iov_base as usize, len));
    }
    Ok(v)
}

pub fn readv(fd: i32, iov: usize, cnt: usize) -> SysResult {
    let of = get(fd)?;
    if !of.flags().readable() {
        return Err(Errno(EBADF));
    }
    let iovs = read_iov(iov, cnt)?;
    let total: usize = iovs.iter().map(|v| v.1).sum();
    let mut k = vec![0u8; total.min(CHUNK)];
    let n = read_into(&of, &mut k)?;
    let mut off = 0;
    for (base, len) in iovs {
        if off >= n {
            break;
        }
        let m = len.min(n - off);
        with_space(|s| s.copy_to_user(base, &k[off..off + m]))?;
        off += m;
    }
    Ok(n as isize)
}

pub fn writev(fd: i32, iov: usize, cnt: usize) -> SysResult {
    let of = get(fd)?;
    if !of.flags().writable() {
        return Err(Errno(EBADF));
    }
    let iovs = read_iov(iov, cnt)?;
    let mut done = 0;
    for (base, len) in iovs {
        if len == 0 {
            continue;
        }
        match write_from(&of, base, len) {
            Ok(n) => {
                done += n as usize;
                if (n as usize) < len {
                    break;
                }
            }
            Err(e) => return partial(done, e),
        }
    }
    Ok(done as isize)
}

pub fn pread64(fd: i32, buf: usize, len: usize, off: i64) -> SysResult {
    let of = get(fd)?;
    if !of.flags().readable() {
        return Err(Errno(EBADF));
    }
    if off < 0 {
        return Err(Errno(EINVAL));
    }
    let mut k = vec![0u8; len.min(CHUNK)];
    let n = of.file.pread(&mut k, off as u64)?;
    with_space(|s| s.copy_to_user(buf, &k[..n]))?;
    Ok(n as isize)
}

pub fn pwrite64(fd: i32, buf: usize, len: usize, off: i64) -> SysResult {
    let of = get(fd)?;
    if !of.flags().writable() {
        return Err(Errno(EBADF));
    }
    if off < 0 {
        return Err(Errno(EINVAL));
    }
    let mut k = vec![0u8; len.min(CHUNK)];
    with_space(|s| s.copy_from_user(buf, &mut k))?;
    Ok(of.file.pwrite(&k, off as u64)? as isize)
}

pub fn lseek(fd: i32, off: i64, whence: i32) -> SysResult {
    Ok(get(fd)?.file.lseek(off, whence)? as isize)
}

static OPENED: SpinLock<Vec<Vec<u8>>> = SpinLock::new(Vec::new());

/// `openat`: no filesystem yet; logs each distinct path once.
pub fn openat(_dirfd: i32, path: usize, _flags: u32) -> SysResult {
    let p = with_space(|s| s.read_cstr(path, PATH_MAX))?;
    let mut seen = OPENED.lock();
    if !seen.contains(&p) {
        crate::kprintln!(
            "[patina] openat {:?}: ENOENT (no filesystem before Phase 3)",
            core::str::from_utf8(&p).unwrap_or("<non-utf8>")
        );
        seen.push(p);
    }
    Err(Errno(ENOENT))
}

/// Path-based lookups (`readlinkat`, `faccessat`, x86 `stat`...): validate
/// the pointer, then `ENOENT`.
pub fn path_enoent(path: usize) -> SysResult {
    with_space(|s| s.read_cstr(path, PATH_MAX))?;
    Err(Errno(ENOENT))
}

pub fn close(fd: i32) -> SysResult {
    // The entry (maybe the last reference: File::release) is dropped
    // after the table lock.
    let e = task::process().files.lock().close(fd)?;
    drop(e);
    Ok(0)
}

pub fn dup(fd: i32) -> SysResult {
    install(get(fd)?, false)
}

pub fn dup3(old: i32, new: i32, flags: u32) -> SysResult {
    if flags & !O_CLOEXEC != 0 || old == new {
        return Err(Errno(EINVAL));
    }
    let lim = proc::nofile_limit();
    if new < 0 || new as usize >= lim {
        return Err(Errno(EBADF));
    }
    let replaced = {
        let mut t = task::process().files.lock();
        let of = t.get(old)?;
        t.install(new as usize, of, flags & O_CLOEXEC != 0)
    };
    drop(replaced);
    Ok(new as isize)
}

/// x86_64 `dup2`: like `dup3` but `old == new` returns `new` if valid.
pub fn dup2(old: i32, new: i32) -> SysResult {
    if old == new {
        get(old)?;
        return Ok(new as isize);
    }
    dup3(old, new, 0)
}

pub fn fcntl(fd: i32, cmd: i32, arg: u64) -> SysResult {
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let lim = proc::nofile_limit();
            let min = arg as i32;
            if min < 0 || min as usize >= lim {
                return Err(Errno(EINVAL));
            }
            let mut t = task::process().files.lock();
            let of = t.get(fd)?;
            Ok(t.alloc(of, cmd == F_DUPFD_CLOEXEC, min as usize, lim)? as isize)
        }
        F_GETFD => {
            let t = task::process().files.lock();
            Ok(if t.entry(fd)?.cloexec { FD_CLOEXEC } else { 0 } as isize)
        }
        F_SETFD => {
            let mut t = task::process().files.lock();
            t.entry_mut(fd)?.cloexec = arg as i32 & FD_CLOEXEC != 0;
            Ok(0)
        }
        F_GETFL => Ok(get(fd)?.flags().0 as isize),
        F_SETFL => {
            get(fd)?.set_status(arg as u32);
            Ok(0)
        }
        _ => {
            get(fd)?;
            Err(Errno(EINVAL))
        }
    }
}

pub fn ioctl(fd: i32, cmd: u32, arg: u64) -> SysResult {
    match cmd {
        FIONBIO => {
            let of = get(fd)?;
            let on: i32 = with_space(|s| s.read_val(arg as usize))?;
            of.set_nonblocking(on != 0);
            Ok(0)
        }
        FIOCLEX | FIONCLEX => {
            let mut t = task::process().files.lock();
            t.entry_mut(fd)?.cloexec = cmd == FIOCLEX;
            Ok(0)
        }
        _ => get(fd)?.file.ioctl(cmd, arg),
    }
}

pub fn fstat(fd: i32, buf: usize) -> SysResult {
    let st = get(fd)?.file.stat()?;
    with_space(|s| s.write_val(buf, &st))?;
    Ok(0)
}

pub fn newfstatat(dirfd: i32, path: usize, buf: usize, flags: u32) -> SysResult {
    let p = with_space(|s| s.read_cstr(path, PATH_MAX))?;
    if p.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd >= 0 {
        return fstat(dirfd, buf);
    }
    Err(Errno(ENOENT))
}

pub fn pipe2(fds: usize, flags: u32) -> SysResult {
    if flags & O_DIRECT != 0 || flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        // O_DIRECT (packet mode) is not implemented.
        return Err(Errno(EINVAL));
    }
    let cloexec = flags & O_CLOEXEC != 0;
    let st = flags & O_NONBLOCK;
    let (r, w) = pipe::new();
    let rf = OpenFile::new(r, O_RDONLY | st);
    let wf = OpenFile::new(w, O_WRONLY | st);
    let lim = proc::nofile_limit();
    let (a, b) = {
        let mut t = task::process().files.lock();
        let a = t.alloc(rf, cloexec, 0, lim)?;
        match t.alloc(wf, cloexec, 0, lim) {
            Ok(b) => (a, b),
            Err(e) => {
                let undo = t.close(a);
                drop(t);
                drop(undo);
                return Err(e);
            }
        }
    };
    if with_space(|s| s.write_val(fds, &[a, b])).is_err() {
        let (x, y) = {
            let mut t = task::process().files.lock();
            (t.close(a), t.close(b))
        };
        drop((x, y));
        return Err(Errno(EFAULT));
    }
    Ok(0)
}

pub fn eventfd2(init: u32, flags: u32) -> SysResult {
    if flags & !(EFD_SEMAPHORE | EFD_CLOEXEC | EFD_NONBLOCK) != 0 {
        return Err(Errno(EINVAL));
    }
    let ev = eventfd::EventFd::new(init, flags & EFD_SEMAPHORE != 0);
    install(
        OpenFile::new(ev, O_RDWR | (flags & EFD_NONBLOCK)),
        flags & EFD_CLOEXEC != 0,
    )
}
