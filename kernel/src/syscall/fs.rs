//! File descriptors and paths, Phase 1 edition: fds 0/1/2 are the console
//! (stdin always at EOF), there is no filesystem yet (Phase 3), so every
//! path lookup fails with `ENOENT`.

use alloc::vec::Vec;

use patina_abi::errno::{EBADF, EINVAL, ENOENT, ENOTTY, Errno};
use patina_abi::fs::{
    AT_EMPTY_PATH, F_GETFD, F_GETFL, F_SETFD, F_SETFL, O_RDONLY, O_RDWR, S_IFCHR,
};

use super::{SysResult, with_space};
use crate::sync::SpinLock;

const PATH_MAX: usize = 4096;

fn is_stdio(fd: i32) -> bool {
    (0..=2).contains(&fd)
}

pub fn read(fd: i32, _buf: usize, _len: usize) -> SysResult {
    // stdin is always at EOF; stdout/stderr are write-only here.
    if fd == 0 { Ok(0) } else { Err(Errno(EBADF)) }
}

pub fn write(fd: i32, buf: usize, len: usize) -> SysResult {
    if fd != 1 && fd != 2 {
        return Err(Errno(EBADF));
    }
    // Copy in chunks so the address-space lock is not held while the
    // (slow) UART drains.
    let mut chunk = [0u8; 512];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(chunk.len());
        if let Err(e) = with_space(|s| s.copy_from_user(buf + done, &mut chunk[..n])) {
            return if done > 0 { Ok(done as isize) } else { Err(e) };
        }
        crate::console::write_bytes(&chunk[..n]);
        done += n;
    }
    Ok(len as isize)
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
    if is_stdio(fd) {
        Ok(0)
    } else {
        Err(Errno(EBADF))
    }
}

pub fn fcntl(fd: i32, cmd: i32, _arg: u64) -> SysResult {
    if !is_stdio(fd) {
        return Err(Errno(EBADF));
    }
    match cmd {
        F_GETFL => Ok(if fd == 0 { O_RDONLY } else { O_RDWR } as isize),
        F_SETFL | F_GETFD | F_SETFD => Ok(0),
        _ => Err(Errno(EINVAL)),
    }
}

pub fn ioctl(fd: i32) -> SysResult {
    if is_stdio(fd) {
        Err(Errno(ENOTTY))
    } else {
        Err(Errno(EBADF))
    }
}

/// `struct stat` of the console (a character device, like `/dev/console`).
fn console_stat() -> patina_abi::current::Stat {
    patina_abi::current::Stat {
        st_mode: S_IFCHR | 0o620,
        st_nlink: 1,
        st_rdev: (5 << 8) | 1, // makedev(5, 1)
        st_blksize: 1024,
        ..Default::default()
    }
}

pub fn fstat(fd: i32, buf: usize) -> SysResult {
    if !is_stdio(fd) {
        return Err(Errno(EBADF));
    }
    with_space(|s| s.write_val(buf, &console_stat()))?;
    Ok(0)
}

pub fn newfstatat(dirfd: i32, path: usize, buf: usize, flags: u32) -> SysResult {
    let p = with_space(|s| s.read_cstr(path, PATH_MAX))?;
    if p.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return fstat(dirfd, buf);
    }
    Err(Errno(ENOENT))
}
