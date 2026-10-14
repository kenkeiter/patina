//! File-descriptor syscalls. Fds go through the process's fd table
//! (`fs::FdTable`) to `fs::File` objects: the console (0/1/2), pipes,
//! eventfds, epoll instances and files opened by path (`syscall::path`).
//!
//! Blocking: file operations never block; the loops here retry after
//! `fs::file::wait_ready` unless the description is `O_NONBLOCK` (see
//! `fs/file.rs`).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use patina_abi::current::O_DIRECT;
use patina_abi::epoll::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE, EpollFlags};
use patina_abi::errno::{EAGAIN, EBADF, EFAULT, EINVAL, EPIPE, Errno};
use patina_abi::fs::{
    F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_GETLK, F_OFD_GETLK, F_OFD_SETLK, F_OFD_SETLKW,
    F_SETFD, F_SETFL, F_SETLK, F_SETLKW, FD_CLOEXEC, Iovec, O_CLOEXEC, O_NONBLOCK, O_RDONLY,
    O_RDWR, O_WRONLY, UIO_MAXIOV,
};
use patina_abi::misc::{FIOCLEX, FIONBIO, FIONCLEX};

use super::{SysResult, proc, with_space};
use crate::mm::vm::access_ok;
use crate::fs::file::{OpenFile, wait_ready};
use crate::fs::{eventfd, pipe};
use crate::task::signal::{self, ERESTARTSYS};
use crate::task::{self, WakeReason};
use patina_abi::signal::{SI_USER, SIGPIPE, SigInfo};

/// Kernel bounce buffer per read/write step.
const CHUNK: usize = 64 * 1024;
/// Linux `MAX_RW_COUNT` (`INT_MAX & PAGE_MASK`).
pub(super) const MAX_RW: usize = 0x7fff_f000;

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

/// Before a read consumes anything for a copy-out to `[buf, buf+len)`:
/// Linux `access_ok` (`EFAULT` if the range leaves the user half), then
/// the number of bytes the copy-out can take (the run of writable VMAs
/// from `buf`). A read is sized by this, so data that could not be
/// copied out stays in the pipe/socket/epoll/directory instead of being
/// consumed and lost; no writable byte at all is `EFAULT` (D86).
pub(super) fn out_room(buf: usize, len: usize) -> Result<usize, Errno> {
    if !access_ok(buf, len) {
        return Err(Errno(EFAULT));
    }
    if len == 0 {
        return Ok(0);
    }
    match with_space(|s| s.writable_prefix(buf, len)) {
        0 => Err(Errno(EFAULT)),
        n => Ok(n),
    }
}

/// [`out_room`] for an iovec array (already through [`read_iov`]): trims
/// it to the segments (and the part of the last one) a copy-out can fill.
/// `EFAULT` if not one byte of a non-empty request is writable.
pub(super) fn clamp_iov_out(iovs: &mut Vec<(usize, usize)>) -> Result<(), Errno> {
    let want: usize = iovs.iter().map(|v| v.1).sum();
    let mut keep = 0;
    let mut got = 0;
    for v in iovs.iter_mut() {
        if v.1 == 0 {
            keep += 1;
            continue;
        }
        let r = with_space(|s| s.writable_prefix(v.0, v.1));
        got += r;
        if r > 0 {
            keep += 1;
        }
        if r < v.1 {
            v.1 = r;
            break;
        }
    }
    iovs.truncate(keep);
    if want > 0 && got == 0 {
        return Err(Errno(EFAULT));
    }
    Ok(())
}

/// `n > 0` bytes done before error `e`: report the bytes, else the error.
fn partial(n: usize, e: Errno) -> SysResult {
    if n > 0 { Ok(n as isize) } else { Err(e) }
}

/// For a datagram socket, its largest message: `read`/`write` move a
/// whole message per call instead of 64 KiB chunks.
fn dgram_max(of: &OpenFile) -> Option<usize> {
    of.file
        .as_socket()
        .filter(|s| !s.is_stream())
        .map(|s| s.max_dgram())
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

/// Write all of `k`, blocking unless `O_NONBLOCK`: the bytes written and,
/// if it stopped early, why (`None`: a zero-length write).
fn write_kbuf(of: &OpenFile, k: &[u8]) -> (usize, Option<Errno>) {
    let mut off = 0;
    while off < k.len() {
        let fl = of.flags();
        match of.file.write(&k[off..], fl) {
            Ok(0) => return (off, None),
            Ok(w) => off += w,
            Err(Errno(EAGAIN)) if !fl.nonblocking() => {
                if wait_ready(&*of.file, EpollFlags::OUT | EpollFlags::WRNORM, None)
                    == WakeReason::Interrupted
                {
                    // Linux pipe_write: bytes written so far, else
                    // -ERESTARTSYS.
                    return (off, Some(Errno(ERESTARTSYS)));
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
                return (off, Some(e));
            }
        }
    }
    (off, None)
}

/// Write `len` user bytes at `ubuf`, blocking unless `O_NONBLOCK`.
fn write_from(of: &OpenFile, ubuf: usize, len: usize) -> SysResult {
    if len == 0 {
        return of.file.write(&[], of.flags()).map(|n| n as isize);
    }
    // A datagram socket takes one write as one message: no chunking.
    let dmax = dgram_max(of);
    if dmax.is_some_and(|m| len > m) {
        return Err(Errno(patina_abi::errno::EMSGSIZE));
    }
    let chunk = dmax.unwrap_or(CHUNK);
    let mut k = vec![0u8; len.min(chunk)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(chunk);
        if let Err(e) = with_space(|s| s.copy_from_user(ubuf + done, &mut k[..n])) {
            return partial(done, e);
        }
        match write_kbuf(of, &k[..n]) {
            (w, None) if w == n => {}
            (w, None) => return Ok((done + w) as isize),
            (w, Some(e)) => return partial(done + w, e),
        }
        done += n;
    }
    Ok(done as isize)
}

/// `sendfile(out_fd, in_fd, offp, count)`: copy from a seekable file
/// (`pread` at `*offp`, or at the file position, which advances) to any
/// writable fd through a kernel buffer, blocking on the output unless it
/// is `O_NONBLOCK`. Only what was written counts as consumed.
pub fn sendfile(out_fd: i32, in_fd: i32, offp: usize, count: usize) -> SysResult {
    let inf = get(in_fd)?;
    let outf = get(out_fd)?;
    if !inf.flags().readable() || !outf.flags().writable() {
        return Err(Errno(EBADF));
    }
    let start: u64 = if offp != 0 {
        let o: i64 = with_space(|s| s.read_val(offp))?;
        if o < 0 {
            return Err(Errno(EINVAL));
        }
        o as u64
    } else {
        inf.file
            .lseek(0, patina_abi::fs::SEEK_CUR)
            .map_err(|_| Errno(EINVAL))?
    };
    let count = count.min(MAX_RW);
    let mut k = vec![0u8; count.clamp(1, CHUNK)];
    let mut done = 0usize;
    let mut err = None;
    while done < count {
        let m = (count - done).min(CHUNK);
        let n = match inf.file.pread(&mut k[..m], start + done as u64) {
            Ok(0) => break,
            Ok(n) => n,
            Err(Errno(patina_abi::errno::ESPIPE)) => {
                err = Some(Errno(EINVAL));
                break;
            }
            Err(e) => {
                err = Some(e);
                break;
            }
        };
        let (w, e) = write_kbuf(&outf, &k[..n]);
        done += w;
        if e.is_some() || w < n {
            err = e;
            break;
        }
    }
    let end = start + done as u64;
    if offp != 0 {
        with_space(|s| s.write_val(offp, &(end as i64)))?;
    } else if done > 0 {
        inf.file.lseek(end as i64, patina_abi::fs::SEEK_SET)?;
    }
    match err {
        Some(e) => partial(done, e),
        None => Ok(done as isize),
    }
}

pub fn read(fd: i32, buf: usize, len: usize) -> SysResult {
    let of = get(fd)?;
    if !of.flags().readable() {
        return Err(Errno(EBADF));
    }
    // Linux vfs_read: access_ok on the whole count, then clamp.
    let room = out_room(buf, len)?;
    let chunk = dgram_max(&of).map_or(CHUNK, |m| m + 1);
    let mut k = vec![0u8; room.min(MAX_RW).min(chunk)];
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
pub(super) fn read_iov(iov: usize, cnt: usize) -> Result<Vec<(usize, usize)>, Errno> {
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
    let mut iovs = read_iov(iov, cnt)?;
    clamp_iov_out(&mut iovs)?;
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
    let room = out_room(buf, len)?;
    let mut k = vec![0u8; room.min(CHUNK)];
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
    let n = get(fd)?.file.lseek(off, whence)?;
    if n > i64::MAX as u64 {
        return Err(Errno(patina_abi::errno::EOVERFLOW));
    }
    Ok(n as isize)
}

/// `getdents64(fd, dirent, count)`: `count` is an `unsigned int`. The
/// directory position advances by what is read, so the read is sized to
/// the writable part of the buffer first (D86).
pub fn getdents64(fd: i32, buf: usize, len: u32) -> SysResult {
    let of = get(fd)?;
    let is_dir = of
        .file
        .stat()
        .is_ok_and(|st| st.st_mode & patina_abi::fs::S_IFMT == patina_abi::fs::S_IFDIR);
    // Not a directory: the file's own error (ENOTDIR, EBADF) comes first.
    let room = if is_dir {
        out_room(buf, len as usize)?
    } else {
        len as usize
    };
    let mut k = vec![0u8; room.min(CHUNK)];
    let n = of.file.getdents64(&mut k)?;
    with_space(|s| s.copy_to_user(buf, &k[..n]))?;
    Ok(n as isize)
}

pub fn ftruncate(fd: i32, len: i64) -> SysResult {
    let of = get(fd)?;
    if of.file.is_opath() {
        return Err(Errno(EBADF));
    }
    if len < 0 || !of.flags().writable() {
        return Err(Errno(EINVAL));
    }
    of.file.truncate(len as u64)?;
    Ok(0)
}

/// `fallocate`: mode 0 extends the file (tmpfs allocates nothing up
/// front); other modes `EOPNOTSUPP`.
pub fn fallocate(fd: i32, mode: i32, off: i64, len: i64) -> SysResult {
    let of = get(fd)?;
    if off < 0 || len <= 0 {
        return Err(Errno(EINVAL));
    }
    if !of.flags().writable() {
        return Err(Errno(EBADF));
    }
    if mode != 0 {
        return Err(Errno(patina_abi::errno::EOPNOTSUPP));
    }
    let end = off
        .checked_add(len)
        .ok_or(Errno(patina_abi::errno::EFBIG))? as u64;
    let st = of.file.stat()?;
    if st.st_mode & patina_abi::fs::S_IFMT != patina_abi::fs::S_IFREG {
        return Err(Errno(patina_abi::errno::ENODEV));
    }
    if end > st.st_size as u64 {
        of.file.truncate(end)?;
    }
    Ok(0)
}

/// `flock`: validated, always granted (one process).
pub fn flock(fd: i32, op: i32) -> SysResult {
    use patina_abi::fs::{LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN};
    get(fd)?;
    match op & !LOCK_NB {
        LOCK_SH | LOCK_EX | LOCK_UN => Ok(0),
        _ => Err(Errno(EINVAL)),
    }
}

/// `fadvise64`: advice is accepted and ignored (`ESPIPE` on pipes).
pub fn fadvise64(fd: i32, advice: i32) -> SysResult {
    let of = get(fd)?;
    if of.file.kind() == "pipe" {
        return Err(Errno(patina_abi::errno::ESPIPE));
    }
    if !(0..=5).contains(&advice) {
        return Err(Errno(EINVAL));
    }
    Ok(0)
}

/// `close_range(first, last, flags)`.
pub fn close_range(first: u32, last: u32, flags: u32) -> SysResult {
    use patina_abi::fs::{CLOSE_RANGE_CLOEXEC, CLOSE_RANGE_UNSHARE};
    if flags & !(CLOSE_RANGE_CLOEXEC | CLOSE_RANGE_UNSHARE) != 0 || first > last {
        return Err(Errno(EINVAL));
    }
    let in_range = |fd: i32| fd >= 0 && (fd as u32) >= first && (fd as u32) <= last;
    let closed = {
        let mut t = task::process().files.lock();
        let fds: Vec<i32> = t.fds().into_iter().filter(|&fd| in_range(fd)).collect();
        let mut closed = Vec::new();
        for fd in fds {
            if flags & CLOSE_RANGE_CLOEXEC != 0 {
                if let Ok(e) = t.entry_mut(fd) {
                    e.cloexec = true;
                }
            } else if let Ok(e) = t.close(fd) {
                closed.push(e);
            }
        }
        closed
    };
    // Released after the table lock.
    drop(closed);
    Ok(0)
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
        F_GETLK | F_SETLK | F_SETLKW | F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW => {
            record_lock(fd, cmd, arg as usize)
        }
        _ => {
            get(fd)?;
            Err(Errno(EINVAL))
        }
    }
}

/// POSIX (`F_SETLK`...) and open-file-description (`F_OFD_*`) record
/// locks. The `struct flock` is checked as Linux does (type, range,
/// access mode); then, with one process, a POSIX lock never conflicts
/// (`F_GETLK` reports `F_UNLCK`) and is granted at once. OFD locks are
/// treated the same way, like `flock` (D57: granted, not tracked), so two
/// descriptions in this process do not exclude each other.
fn record_lock(fd: i32, cmd: i32, p: usize) -> SysResult {
    use patina_abi::errno::EOVERFLOW;
    use patina_abi::fs::{F_RDLCK, F_UNLCK, F_WRLCK, Flock, SEEK_CUR};
    use patina_fsutil::{LockRangeError, lock_range};
    let of = get(fd)?;
    let mut fl: Flock = with_space(|s| s.read_val(p))?;
    let ofd = matches!(cmd, F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW);
    if ofd && fl.l_pid != 0 {
        return Err(Errno(EINVAL));
    }
    let get = matches!(cmd, F_GETLK | F_OFD_GETLK);
    match fl.l_type {
        F_RDLCK | F_WRLCK => {}
        F_UNLCK if !get => {}
        _ => return Err(Errno(EINVAL)),
    }
    let pos = of.file.lseek(0, SEEK_CUR).unwrap_or(0) as i64;
    let size = of.file.stat().map(|st| st.st_size).unwrap_or(0);
    lock_range(fl.l_whence, fl.l_start, fl.l_len, pos, size).map_err(|e| match e {
        LockRangeError::Invalid => Errno(EINVAL),
        LockRangeError::Overflow => Errno(EOVERFLOW),
    })?;
    if get {
        fl.l_type = F_UNLCK;
        with_space(|s| s.write_val(p, &fl))?;
        return Ok(0);
    }
    let fm = of.flags();
    match fl.l_type {
        F_RDLCK if !fm.readable() => Err(Errno(EBADF)),
        F_WRLCK if !fm.writable() => Err(Errno(EBADF)),
        _ => Ok(0),
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
