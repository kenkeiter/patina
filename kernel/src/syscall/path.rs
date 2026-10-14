//! Path-based syscalls (design §7 Tier 2): `openat`, `*at` namespace and
//! metadata calls, cwd, `umask`, `statfs`, and the x86_64 legacy entry
//! points. Resolution goes through `fs::namei` (dirfd, cwd, procfs magic
//! links); the operations themselves are `patina_vfs::Fs` methods run
//! under the filesystem lock. User memory is never touched with that lock
//! held: paths are copied in first and results copied out after.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use patina_abi::errno::{EBADF, EINVAL, ENOTDIR, EOPNOTSUPP, EPERM, ERANGE, Errno};
use patina_abi::fs::{
    AT_EACCESS, AT_EMPTY_PATH, AT_FDCWD, AT_NO_AUTOMOUNT, AT_REMOVEDIR, AT_STATX_SYNC_TYPE,
    AT_SYMLINK_FOLLOW, AT_SYMLINK_NOFOLLOW, O_CLOEXEC, S_IFCHR, S_IFDIR, S_IFIFO, S_IFMT, S_IFREG,
    S_IFSOCK, ST_RELATIME, Statfs, Statx, StatxTimestamp, TMPFS_MAGIC,
};
use patina_abi::time::{Timespec, Timeval};
use patina_vfs::{FileType, SetTime};

use super::{SysResult, fs::install, with_space};
use crate::fs::file::OpenFile;
use crate::fs::namei::{self, Base, Node};
use crate::fs::vfs;
use crate::task;

const PATH_MAX: usize = 4096;

/// Copy a path in from user memory.
fn upath(ptr: usize) -> Result<Vec<u8>, Errno> {
    with_space(|s| s.read_cstr(ptr, PATH_MAX - 1))
}

fn umask() -> u32 {
    task::process().umask.load(Ordering::Relaxed)
}

// ---- open ----------------------------------------------------------------

pub fn openat(dirfd: i32, path: usize, flags: u32, mode: u32) -> SysResult {
    let p = upath(path)?;
    let of = namei::open(dirfd, &p, flags, mode)?;
    install(of, flags & O_CLOEXEC != 0)
}

// ---- namespace -----------------------------------------------------------

pub fn mkdirat(dirfd: i32, path: usize, mode: u32) -> SysResult {
    let p = upath(path)?;
    let base = Base::new(dirfd, &p)?;
    let mode = mode & 0o1777 & !umask();
    vfs::with(|fs| {
        let s = base.start(fs)?;
        fs.mkdir(s, &p, mode)
    })?;
    Ok(0)
}

/// `mknodat`: regular files and FIFOs; devices and sockets `EPERM`
/// (devices come from the initrd or the kernel's `/dev`).
pub fn mknodat(dirfd: i32, path: usize, mode: u32, _dev: u64) -> SysResult {
    let p = upath(path)?;
    let t = mode & S_IFMT;
    match t {
        0 | S_IFREG | S_IFIFO => {}
        S_IFCHR | patina_abi::fs::S_IFBLK | S_IFSOCK => return Err(Errno(EPERM)),
        S_IFDIR => return Err(Errno(EPERM)),
        _ => return Err(Errno(EINVAL)),
    }
    let base = Base::new(dirfd, &p)?;
    let mode = t | (mode & 0o7777 & !umask());
    vfs::with(|fs| {
        let s = base.start(fs)?;
        fs.mknod(s, &p, mode, 0, None)
    })?;
    Ok(0)
}

pub fn unlinkat(dirfd: i32, path: usize, flags: u32) -> SysResult {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    let base = Base::new(dirfd, &p)?;
    vfs::with(|fs| {
        let s = base.start(fs)?;
        if flags & AT_REMOVEDIR != 0 {
            fs.rmdir(s, &p)
        } else {
            fs.unlink(s, &p)
        }
    })?;
    Ok(0)
}

pub fn renameat2(od: i32, op: usize, nd: i32, np: usize, flags: u32) -> SysResult {
    let (op, np) = (upath(op)?, upath(np)?);
    let (ob, nb) = (Base::new(od, &op)?, Base::new(nd, &np)?);
    vfs::with(|fs| {
        let (os, ns) = (ob.start(fs)?, nb.start(fs)?);
        fs.rename(os, &op, ns, &np, flags)
    })?;
    Ok(0)
}

pub fn symlinkat(target: usize, dirfd: i32, path: usize) -> SysResult {
    let t = upath(target)?;
    let p = upath(path)?;
    let base = Base::new(dirfd, &p)?;
    vfs::with(|fs| {
        let s = base.start(fs)?;
        fs.symlink(&t, s, &p)
    })?;
    Ok(0)
}

pub fn linkat(od: i32, op: usize, nd: i32, np: usize, flags: u32) -> SysResult {
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno(EINVAL));
    }
    let (op, np) = (upath(op)?, upath(np)?);
    let nb = Base::new(nd, &np)?;
    namei::with_node(
        od,
        &op,
        flags & AT_SYMLINK_FOLLOW != 0,
        flags & AT_EMPTY_PATH != 0,
        |n| match n {
            Node::Vfs(fs, ino) => {
                let s = nb.start(fs)?;
                fs.link_ino(ino, s, &np)
            }
            // Pipes and the like cannot be linked into the tree.
            _ => Err(Errno(patina_abi::errno::EXDEV)),
        },
    )?;
    Ok(0)
}

pub fn readlinkat(dirfd: i32, path: usize, buf: usize, size: usize) -> SysResult {
    if (size as isize) <= 0 {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    // An empty path reads the dirfd itself (an O_PATH|O_NOFOLLOW symlink).
    let t = namei::with_node(dirfd, &p, false, dirfd != AT_FDCWD, |n| match n {
        Node::Vfs(fs, ino) => fs.readlink_ino(ino),
        Node::Link { target, .. } => Ok(target.to_vec()),
        Node::File(_) => Err(Errno(EINVAL)),
    })?;
    let n = t.len().min(size);
    with_space(|s| s.copy_to_user(buf, &t[..n]))?;
    Ok(n as isize)
}

// ---- metadata ------------------------------------------------------------

fn statx_from_stat(st: &patina_abi::current::Stat) -> Statx {
    let ts = |s: i64, ns: u64| StatxTimestamp {
        tv_sec: s,
        tv_nsec: ns as u32,
        _reserved: 0,
    };
    Statx {
        stx_mask: patina_abi::fs::STATX_BASIC_STATS,
        stx_blksize: st.st_blksize as u32,
        // u32 on aarch64, u64 on x86_64.
        #[allow(clippy::unnecessary_cast)]
        stx_nlink: st.st_nlink as u32,
        stx_uid: st.st_uid,
        stx_gid: st.st_gid,
        stx_mode: st.st_mode as u16,
        stx_ino: st.st_ino,
        stx_size: st.st_size as u64,
        stx_blocks: st.st_blocks as u64,
        stx_atime: ts(st.st_atime, st.st_atime_nsec),
        stx_ctime: ts(st.st_ctime, st.st_ctime_nsec),
        stx_mtime: ts(st.st_mtime, st.st_mtime_nsec),
        stx_rdev_major: patina_abi::fs::dev_major(st.st_rdev),
        stx_rdev_minor: patina_abi::fs::dev_minor(st.st_rdev),
        stx_dev_major: patina_abi::fs::dev_major(st.st_dev),
        stx_dev_minor: patina_abi::fs::dev_minor(st.st_dev),
        ..Default::default()
    }
}

pub fn newfstatat(dirfd: i32, path: usize, buf: usize, flags: u32) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT) != 0 {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    let st = namei::with_node(
        dirfd,
        &p,
        flags & AT_SYMLINK_NOFOLLOW == 0,
        flags & AT_EMPTY_PATH != 0,
        |n| match n {
            Node::Vfs(fs, ino) => Ok(fs.stat(ino)?.to_stat()),
            Node::File(of) => of.file.stat(),
            Node::Link { st, .. } => Ok(st),
        },
    )?;
    with_space(|s| s.write_val(buf, &st))?;
    Ok(0)
}

pub fn statx(dirfd: i32, path: usize, flags: u32, _mask: u32, buf: usize) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE) != 0
        || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE
    {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    let sx = namei::with_node(
        dirfd,
        &p,
        flags & AT_SYMLINK_NOFOLLOW == 0,
        flags & AT_EMPTY_PATH != 0,
        |n| match n {
            Node::Vfs(fs, ino) => Ok(fs.stat(ino)?.to_statx()),
            Node::File(of) => Ok(statx_from_stat(&of.file.stat()?)),
            Node::Link { st, .. } => Ok(statx_from_stat(&st)),
        },
    )?;
    with_space(|s| s.write_val(buf, &sx))?;
    Ok(0)
}

/// x86_64 `stat`/`lstat`.
pub fn stat(path: usize, buf: usize, follow: bool) -> SysResult {
    newfstatat(
        AT_FDCWD,
        path,
        buf,
        if follow { 0 } else { AT_SYMLINK_NOFOLLOW },
    )
}

pub fn faccessat2(dirfd: i32, path: usize, mode: u32, flags: u32) -> SysResult {
    if flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 || mode & !7 != 0 {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    namei::with_node(
        dirfd,
        &p,
        flags & AT_SYMLINK_NOFOLLOW == 0,
        flags & AT_EMPTY_PATH != 0,
        |n| match n {
            Node::Vfs(fs, ino) => fs.access(ino, mode),
            _ => Ok(()),
        },
    )?;
    Ok(0)
}

pub fn fchmodat2(dirfd: i32, path: usize, mode: u32, flags: u32) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    namei::with_node(
        dirfd,
        &p,
        flags & AT_SYMLINK_NOFOLLOW == 0,
        flags & AT_EMPTY_PATH != 0,
        |n| match n {
            Node::Vfs(fs, ino) => {
                if fs.file_type(ino)? == FileType::Symlink {
                    // Linux: symlink modes cannot be changed.
                    return Err(Errno(EOPNOTSUPP));
                }
                fs.chmod(ino, mode)
            }
            Node::File(_) => Ok(()),
            Node::Link { .. } => Err(Errno(EOPNOTSUPP)),
        },
    )?;
    Ok(0)
}

fn id(v: u32) -> Option<u32> {
    (v != u32::MAX).then_some(v)
}

pub fn fchownat(dirfd: i32, path: usize, uid: u32, gid: u32, flags: u32) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    namei::with_node(
        dirfd,
        &p,
        flags & AT_SYMLINK_NOFOLLOW == 0,
        flags & AT_EMPTY_PATH != 0,
        |n| match n {
            Node::Vfs(fs, ino) => fs.chown(ino, id(uid), id(gid)),
            _ => Ok(()),
        },
    )?;
    Ok(0)
}

/// Apply `utimensat` times to what `(dirfd, path)` names.
fn set_times(dirfd: i32, p: &[u8], times: [SetTime; 2], flags: u32) -> SysResult {
    namei::with_node(
        dirfd,
        p,
        flags & AT_SYMLINK_NOFOLLOW == 0,
        flags & AT_EMPTY_PATH != 0,
        |n| match n {
            Node::Vfs(fs, ino) => fs.utimens(ino, times[0], times[1]),
            _ => Ok(()),
        },
    )?;
    Ok(0)
}

pub fn utimensat(dirfd: i32, path: usize, times: usize, flags: u32) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno(EINVAL));
    }
    let t = if times == 0 {
        [SetTime::Now, SetTime::Now]
    } else {
        let ts: [Timespec; 2] = with_space(|s| s.read_val(times))?;
        [
            SetTime::from_timespec(Some(ts[0]))?,
            SetTime::from_timespec(Some(ts[1]))?,
        ]
    };
    // A NULL path is futimens(dirfd).
    if path == 0 {
        if dirfd == AT_FDCWD {
            return Err(Errno(patina_abi::errno::EFAULT));
        }
        return set_times(dirfd, &[], t, flags | AT_EMPTY_PATH);
    }
    let p = upath(path)?;
    set_times(dirfd, &p, t, flags)
}

/// x86_64 `utimes`/`futimesat`: microsecond `timeval`s.
pub fn futimesat(dirfd: i32, path: usize, times: usize) -> SysResult {
    let t = if times == 0 {
        [SetTime::Now, SetTime::Now]
    } else {
        let tv: [Timeval; 2] = with_space(|s| s.read_val(times))?;
        let mut out = [SetTime::Now; 2];
        for (o, v) in out.iter_mut().zip(tv) {
            if !(0..1_000_000).contains(&v.tv_usec) {
                return Err(Errno(EINVAL));
            }
            *o = SetTime::At(Timespec {
                tv_sec: v.tv_sec,
                tv_nsec: v.tv_usec * 1000,
            });
        }
        out
    };
    let p = upath(path)?;
    set_times(dirfd, &p, t, 0)
}

/// x86_64 `utime(path, const struct utimbuf *)`: whole seconds
/// (`actime`, `modtime`); NULL sets both to now.
pub fn utime(path: usize, times: usize) -> SysResult {
    let t = if times == 0 {
        [SetTime::Now, SetTime::Now]
    } else {
        let tb: [i64; 2] = with_space(|s| s.read_val(times))?;
        tb.map(|sec| {
            SetTime::At(Timespec {
                tv_sec: sec,
                tv_nsec: 0,
            })
        })
    };
    let p = upath(path)?;
    set_times(AT_FDCWD, &p, t, 0)
}

pub fn truncate(path: usize, len: i64) -> SysResult {
    if len < 0 {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    namei::with_node(AT_FDCWD, &p, true, false, |n| match n {
        Node::Vfs(fs, ino) => match fs.file_type(ino)? {
            FileType::Directory => Err(Errno(patina_abi::errno::EISDIR)),
            FileType::Regular if fs.special(ino).is_none() => fs.truncate(ino, len as u64),
            _ => Err(Errno(EINVAL)),
        },
        _ => Err(Errno(EINVAL)),
    })?;
    Ok(0)
}

// ---- cwd, umask ----------------------------------------------------------

pub fn chdir(path: usize) -> SysResult {
    let p = upath(path)?;
    namei::with_node(AT_FDCWD, &p, true, false, |n| match n {
        Node::Vfs(fs, ino) => namei::set_cwd(fs, ino),
        _ => Err(Errno(ENOTDIR)),
    })?;
    Ok(0)
}

pub fn fchdir(fd: i32) -> SysResult {
    let (of, v) = namei::fd_vnode(fd, true)?;
    let ino = v.ok_or(Errno(ENOTDIR))?;
    vfs::with(|fs| namei::set_cwd(fs, ino))?;
    drop(of);
    Ok(0)
}

pub fn getcwd(buf: usize, size: usize) -> SysResult {
    let mut p = vfs::with(|fs| fs.getcwd(namei::cwd()))?;
    p.push(0);
    if p.len() > size {
        return Err(Errno(ERANGE));
    }
    with_space(|s| s.copy_to_user(buf, &p))?;
    Ok(p.len() as isize)
}

pub fn umask_call(mask: u32) -> SysResult {
    Ok(task::process().umask.swap(mask & 0o777, Ordering::Relaxed) as isize)
}

// ---- statfs ----------------------------------------------------------------

fn statfs_val() -> Statfs {
    let (_, total) = crate::mm::frame::stats();
    let blocks = (total as u64 / 2).max(1);
    let (used, inodes) = vfs::with(|fs| (fs.pages_used(), fs.inode_count() as u64));
    let free = blocks.saturating_sub(used);
    let files = blocks.max(inodes * 2);
    Statfs {
        f_type: TMPFS_MAGIC,
        f_bsize: 4096,
        f_blocks: blocks,
        f_bfree: free,
        f_bavail: free,
        f_files: files,
        f_ffree: files - inodes,
        f_namelen: 255,
        f_frsize: 4096,
        f_flags: ST_RELATIME,
        ..Default::default()
    }
}

pub fn statfs(path: usize, buf: usize) -> SysResult {
    let p = upath(path)?;
    namei::with_node(AT_FDCWD, &p, true, false, |_| Ok(()))?;
    with_space(|s| s.write_val(buf, &statfs_val()))?;
    Ok(0)
}

/// `umount2(path, flags)`: there is one filesystem and nothing is
/// mounted on top of it, so an existing path is never a mount point
/// (`EINVAL`, as Linux for a non-mount-point; `ENOENT` if missing).
/// Go's syscall tests call it to clean up after a failed child.
pub fn umount2(path: usize, flags: u32) -> SysResult {
    // MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW.
    if flags & !0xf != 0 {
        return Err(Errno(EINVAL));
    }
    let p = upath(path)?;
    namei::with_node(AT_FDCWD, &p, flags & 8 == 0, false, |_| Ok(()))?;
    Err(Errno(EINVAL))
}

pub fn fstatfs(fd: i32, buf: usize) -> SysResult {
    namei::fdget(fd)?;
    with_space(|s| s.write_val(buf, &statfs_val()))?;
    Ok(0)
}

// ---- fd metadata -----------------------------------------------------------

/// Run `f` on `fd`'s inode (`EBADF` for `O_PATH`); non-VFS files: `Ok`.
fn on_fd_ino(
    fd: i32,
    f: impl FnOnce(&mut patina_vfs::Fs, patina_vfs::Ino) -> Result<(), Errno>,
) -> SysResult {
    let (of, v) = namei::fd_vnode(fd, false)?;
    if let Some(ino) = v {
        vfs::with(|fs| f(fs, ino))?;
    }
    drop::<alloc::sync::Arc<OpenFile>>(of);
    Ok(0)
}

pub fn fchmod(fd: i32, mode: u32) -> SysResult {
    on_fd_ino(fd, |fs, ino| fs.chmod(ino, mode))
}

pub fn fchown(fd: i32, uid: u32, gid: u32) -> SysResult {
    on_fd_ino(fd, |fs, ino| fs.chown(ino, id(uid), id(gid)))
}

/// x86_64 `lchown`.
pub fn lchown(path: usize, uid: u32, gid: u32) -> SysResult {
    fchownat(AT_FDCWD, path, uid, gid, AT_SYMLINK_NOFOLLOW)
}

/// Validate an fd for calls that are no-ops on tmpfs (`fsync`...).
pub fn fd_noop(fd: i32) -> SysResult {
    let of = namei::fdget(fd)?;
    if of.file.is_opath() {
        return Err(Errno(EBADF));
    }
    Ok(0)
}
