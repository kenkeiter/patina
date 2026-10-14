//! `File` implementations for nodes opened by path: regular files,
//! directories, `O_PATH` descriptors, special nodes (wrapped so `fstat`
//! reports the inode) and generated read-only contents (procfs).
//!
//! Each holds a [`VHandle`]: the inode stays alive (and readable) after
//! `unlink` until the last description goes. Handles are dropped when the
//! `OpenFile` goes, outside every lock (see `vfs.rs`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::{EBADF, EINVAL, EISDIR, ENXIO, Errno};
use patina_abi::fs::{SEEK_CUR, SEEK_DATA, SEEK_END, SEEK_HOLE, SEEK_SET};
use patina_vfs::Ino;

use super::file::{File, FileFlags, PollHook};
use super::vfs::{self, VHandle};
use crate::sync::SpinLock;

const ALWAYS_READY: EpollFlags = EpollFlags::IN
    .union(EpollFlags::RDNORM)
    .union(EpollFlags::OUT)
    .union(EpollFlags::WRNORM);

/// `struct stat` of inode `ino`.
pub fn stat_ino(ino: Ino) -> Result<Stat, Errno> {
    vfs::with(|fs| fs.stat(ino)).map(|m| m.to_stat())
}

/// New offset for `lseek(off, whence)` on a file of `size` bytes at `pos`
/// (Linux `generic_file_llseek` + tmpfs `SEEK_DATA`/`SEEK_HOLE`: a file
/// is one data extent, holes are not reported).
pub fn seek_pos(pos: u64, size: u64, off: i64, whence: i32) -> Result<u64, Errno> {
    let base: i64 = match whence {
        SEEK_SET => 0,
        SEEK_CUR => pos as i64,
        SEEK_END => size as i64,
        SEEK_DATA | SEEK_HOLE => {
            if off < 0 {
                return Err(Errno(ENXIO));
            }
            if off as u64 >= size {
                return Err(Errno(ENXIO));
            }
            return Ok(if whence == SEEK_DATA {
                off as u64
            } else {
                size
            });
        }
        _ => return Err(Errno(EINVAL)),
    };
    match base.checked_add(off) {
        Some(n) if n >= 0 => Ok(n as u64),
        _ => Err(Errno(EINVAL)),
    }
}

/// A regular file: inode + offset.
pub struct RegFile {
    h: VHandle,
    pos: SpinLock<u64>,
    name: Vec<u8>,
}

impl RegFile {
    pub fn new(h: VHandle, name: Vec<u8>) -> RegFile {
        RegFile {
            h,
            pos: SpinLock::new(0),
            name,
        }
    }
}

impl File for RegFile {
    fn kind(&self) -> &'static str {
        "file"
    }

    fn read(&self, buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        let mut pos = self.pos.lock();
        let n = self.h.with(|fs, ino| fs.read_at(ino, *pos, buf))?;
        *pos += n as u64;
        Ok(n)
    }

    fn write(&self, buf: &[u8], flags: FileFlags) -> Result<usize, Errno> {
        let mut pos = self.pos.lock();
        if flags.append() {
            let (n, end) = self.h.with(|fs, ino| fs.append(ino, buf))?;
            *pos = end;
            Ok(n)
        } else {
            let n = self.h.with(|fs, ino| fs.write_at(ino, *pos, buf))?;
            *pos += n as u64;
            Ok(n)
        }
    }

    fn poll(&self, _hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        ALWAYS_READY
    }

    fn pollable(&self) -> bool {
        false
    }

    fn stat(&self) -> Result<Stat, Errno> {
        stat_ino(self.h.ino())
    }

    fn pread(&self, buf: &mut [u8], off: u64) -> Result<usize, Errno> {
        self.h.with(|fs, ino| fs.read_at(ino, off, buf))
    }

    fn pwrite(&self, buf: &[u8], off: u64) -> Result<usize, Errno> {
        // Deviation: Linux appends when the description is O_APPEND (a
        // documented BUGS entry of pwrite(2)); patina writes at `off`.
        self.h.with(|fs, ino| fs.write_at(ino, off, buf))
    }

    fn lseek(&self, off: i64, whence: i32) -> Result<u64, Errno> {
        let mut pos = self.pos.lock();
        let size = self.h.with(|fs, ino| fs.size(ino))?;
        let n = seek_pos(*pos, size, off, whence)?;
        *pos = n;
        Ok(n)
    }

    fn truncate(&self, len: u64) -> Result<(), Errno> {
        self.h.with(|fs, ino| fs.truncate(ino, len))
    }

    fn vnode(&self) -> Option<Ino> {
        Some(self.h.ino())
    }

    fn proc_name(&self) -> Vec<u8> {
        self.name.clone()
    }
}

/// A directory: inode + `getdents64` stream position.
pub struct DirFile {
    h: VHandle,
    pos: SpinLock<u64>,
}

impl DirFile {
    pub fn new(h: VHandle) -> DirFile {
        DirFile {
            h,
            pos: SpinLock::new(0),
        }
    }
}

impl File for DirFile {
    fn kind(&self) -> &'static str {
        "dir"
    }

    fn read(&self, _buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EISDIR))
    }

    fn write(&self, _buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EBADF))
    }

    fn poll(&self, _hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        ALWAYS_READY
    }

    fn pollable(&self) -> bool {
        false
    }

    fn stat(&self) -> Result<Stat, Errno> {
        stat_ino(self.h.ino())
    }

    fn pread(&self, _buf: &mut [u8], _off: u64) -> Result<usize, Errno> {
        Err(Errno(EISDIR))
    }

    fn lseek(&self, off: i64, whence: i32) -> Result<u64, Errno> {
        // dcache_dir_lseek: SEEK_SET / SEEK_CUR on the stream position.
        let mut pos = self.pos.lock();
        let n = match whence {
            SEEK_SET => off,
            SEEK_CUR => (*pos as i64).checked_add(off).ok_or(Errno(EINVAL))?,
            _ => return Err(Errno(EINVAL)),
        };
        if n < 0 {
            return Err(Errno(EINVAL));
        }
        *pos = n as u64;
        Ok(n as u64)
    }

    fn getdents64(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut pos = self.pos.lock();
        self.h.with(|fs, ino| fs.getdents64(ino, &mut pos, buf))
    }

    fn truncate(&self, _len: u64) -> Result<(), Errno> {
        Err(Errno(EISDIR))
    }

    fn vnode(&self) -> Option<Ino> {
        Some(self.h.ino())
    }

    fn proc_name(&self) -> Vec<u8> {
        vfs::with(|fs| fs.getcwd(self.h.ino())).unwrap_or_else(|_| b"/".to_vec())
    }
}

/// An `O_PATH` descriptor: names an inode, nothing else.
pub struct PathFile {
    h: VHandle,
    name: Vec<u8>,
}

impl PathFile {
    pub fn new(h: VHandle, name: Vec<u8>) -> PathFile {
        PathFile { h, name }
    }
}

impl File for PathFile {
    fn kind(&self) -> &'static str {
        "path"
    }
    fn read(&self, _buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EBADF))
    }
    fn write(&self, _buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EBADF))
    }
    fn poll(&self, _hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        EpollFlags::empty()
    }
    fn pollable(&self) -> bool {
        false
    }
    fn ioctl(&self, _cmd: u32, _arg: u64) -> Result<isize, Errno> {
        Err(Errno(EBADF))
    }
    fn stat(&self) -> Result<Stat, Errno> {
        stat_ino(self.h.ino())
    }
    fn pread(&self, _buf: &mut [u8], _off: u64) -> Result<usize, Errno> {
        Err(Errno(EBADF))
    }
    fn pwrite(&self, _buf: &[u8], _off: u64) -> Result<usize, Errno> {
        Err(Errno(EBADF))
    }
    fn lseek(&self, _off: i64, _whence: i32) -> Result<u64, Errno> {
        Err(Errno(EBADF))
    }
    fn getdents64(&self, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(Errno(EBADF))
    }
    fn truncate(&self, _len: u64) -> Result<(), Errno> {
        Err(Errno(EBADF))
    }
    fn vnode(&self) -> Option<Ino> {
        Some(self.h.ino())
    }
    fn is_opath(&self) -> bool {
        true
    }
    fn proc_name(&self) -> Vec<u8> {
        self.name.clone()
    }
}

/// A special node opened by path: the `DevNode`'s file, with `fstat`
/// reporting the node's inode (mode, `rdev`, times).
pub struct NodeFile {
    h: VHandle,
    inner: Arc<dyn File>,
    name: Vec<u8>,
}

impl NodeFile {
    pub fn new(h: VHandle, inner: Arc<dyn File>, name: Vec<u8>) -> NodeFile {
        NodeFile { h, inner, name }
    }
}

impl File for NodeFile {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn read(&self, buf: &mut [u8], flags: FileFlags) -> Result<usize, Errno> {
        self.inner.read(buf, flags)
    }
    fn write(&self, buf: &[u8], flags: FileFlags) -> Result<usize, Errno> {
        self.inner.write(buf, flags)
    }
    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        self.inner.poll(hook)
    }
    fn pollable(&self) -> bool {
        self.inner.pollable()
    }
    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        self.inner.ioctl(cmd, arg)
    }
    fn stat(&self) -> Result<Stat, Errno> {
        stat_ino(self.h.ino())
    }
    fn pread(&self, buf: &mut [u8], off: u64) -> Result<usize, Errno> {
        self.inner.pread(buf, off)
    }
    fn pwrite(&self, buf: &[u8], off: u64) -> Result<usize, Errno> {
        self.inner.pwrite(buf, off)
    }
    fn lseek(&self, off: i64, whence: i32) -> Result<u64, Errno> {
        self.inner.lseek(off, whence)
    }
    fn getdents64(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.inner.getdents64(buf)
    }
    fn truncate(&self, len: u64) -> Result<(), Errno> {
        self.inner.truncate(len)
    }
    fn vnode(&self) -> Option<Ino> {
        Some(self.h.ino())
    }
    fn proc_name(&self) -> Vec<u8> {
        self.name.clone()
    }
    fn release(&self) {
        self.inner.release();
    }
}

/// Read-only contents generated at open time (procfs files).
pub struct MemFile {
    data: Vec<u8>,
    pos: SpinLock<u64>,
}

impl MemFile {
    pub fn new(data: Vec<u8>) -> MemFile {
        MemFile {
            data,
            pos: SpinLock::new(0),
        }
    }

    fn read_at(&self, buf: &mut [u8], off: u64) -> usize {
        let len = self.data.len() as u64;
        if off >= len {
            return 0;
        }
        let n = buf.len().min((len - off) as usize);
        buf[..n].copy_from_slice(&self.data[off as usize..off as usize + n]);
        n
    }
}

impl File for MemFile {
    fn kind(&self) -> &'static str {
        "proc"
    }
    fn read(&self, buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        let mut pos = self.pos.lock();
        let n = self.read_at(buf, *pos);
        *pos += n as u64;
        Ok(n)
    }
    fn write(&self, _buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EINVAL))
    }
    fn poll(&self, _hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        ALWAYS_READY
    }
    fn pollable(&self) -> bool {
        false
    }
    fn stat(&self) -> Result<Stat, Errno> {
        Ok(super::file::anon_stat(
            patina_abi::fs::S_IFREG | 0o444,
            super::file::alloc_ino(),
        ))
    }
    fn pread(&self, buf: &mut [u8], off: u64) -> Result<usize, Errno> {
        Ok(self.read_at(buf, off))
    }
    fn lseek(&self, off: i64, whence: i32) -> Result<u64, Errno> {
        let mut pos = self.pos.lock();
        let n = seek_pos(*pos, self.data.len() as u64, off, whence)?;
        *pos = n;
        Ok(n)
    }
}
