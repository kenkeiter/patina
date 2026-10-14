//! The kernel path layer: `dirfd` + path → inode or open file, on top of
//! `patina_vfs` resolution, with the procfs magic links intercepted first.
//!
//! Entry points (used by `syscall::path` and available to other kernel
//! code):
//!
//! * [`open`]: `openat` semantics → `Arc<OpenFile>`;
//! * [`open_ino`]: open an inode already held (reopen through
//!   `/proc/self/fd/N`, and kernel users that resolved a node themselves);
//! * [`with_node`]: resolve for a metadata operation and run a closure on
//!   the result ([`Node`]) — under the FS lock for VFS inodes;
//! * [`Base`]: the starting directory of a relative lookup;
//! * [`cwd`] / [`set_cwd`].
//!
//! Magic links: `/proc/1/fd/N` and `/proc/1/cwd` (however reached:
//! `/proc/self/...`, `/dev/fd/N`, `/dev/stdin`, relative to a dirfd...)
//! are detected by [`classify`], which repeats the walk with
//! `Fs::lookup_parent` and follows final symlinks itself. Following such a
//! link reaches the open file (or cwd) directly, like Linux's
//! `proc_fd_link`; deeper paths through them (`/proc/self/fd/3/x`) are
//! not supported (`/proc/self/cwd/x` works through the placeholder
//! symlink).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use patina_abi::current::{O_DIRECT, O_DIRECTORY, O_NOFOLLOW, Stat};
use patina_abi::errno::{EBADF, EINVAL, EISDIR, ELOOP, ENOENT, ENOTDIR, ENXIO, EOPNOTSUPP, Errno};
use patina_abi::fs::{
    AT_FDCWD, O_ACCMODE, O_APPEND, O_CREAT, O_EXCL, O_NONBLOCK, O_PATH, O_RDONLY, O_RDWR,
    O_TMPFILE_BIT, O_TRUNC, O_WRONLY,
};
use patina_vfs::{FileType, Fs, Ino, MAXSYMLINKS, OpenOptions, Special};

use super::file::{File, OpenFile};
use super::pipe::Fifo;
use super::procfs::{self, FdDirFile};
use super::vfile::{DirFile, NodeFile, PathFile, RegFile};
use super::vfs::{self, DevNode, KFs, VHandle};
use crate::task;

/// The open file at `fd` (`EBADF`).
pub fn fdget(fd: i32) -> Result<Arc<OpenFile>, Errno> {
    task::process().files.lock().get(fd)
}

/// The current directory's inode. Call with the FS locked (the cwd only
/// changes under that lock, so the inode stays valid while it is held).
pub fn cwd() -> Ino {
    *task::process().cwd.lock()
}

/// Make `ino` (a directory, checked here) the current directory. FS
/// locked; `ino` must be alive.
pub fn set_cwd(fs: &mut Fs, ino: Ino) -> Result<(), Errno> {
    if fs.file_type(ino)? != FileType::Directory {
        return Err(Errno(ENOTDIR));
    }
    fs.get_ref(ino)?;
    let old = core::mem::replace(&mut *task::process().cwd.lock(), ino);
    fs.put_ref(old);
    if let Ok(p) = fs.getcwd(ino) {
        procfs::set_cwd_link(fs, &p);
    }
    Ok(())
}

/// The starting directory of a lookup: the cwd (`AT_FDCWD`, or any
/// absolute path) or a dirfd, which the `Base` keeps open.
pub struct Base(Option<Arc<OpenFile>>);

impl Base {
    pub fn new(dirfd: i32, path: &[u8]) -> Result<Base, Errno> {
        if path.first() == Some(&b'/') || dirfd == AT_FDCWD {
            Ok(Base(None))
        } else {
            Ok(Base(Some(fdget(dirfd)?)))
        }
    }

    /// The start inode (FS locked): `ENOTDIR` unless a directory.
    pub fn start(&self, fs: &Fs) -> Result<Ino, Errno> {
        match &self.0 {
            None => Ok(cwd()),
            Some(of) => {
                let ino = of.file.vnode().ok_or(Errno(ENOTDIR))?;
                if fs.file_type(ino)? != FileType::Directory {
                    return Err(Errno(ENOTDIR));
                }
                Ok(ino)
            }
        }
    }
}

/// A magic procfs link.
#[derive(Clone, Copy, Debug)]
pub enum Magic {
    /// `/proc/1/fd/N`.
    Fd(i32),
    /// `/proc/1/cwd`.
    Cwd,
}

fn parse_fd(name: &[u8]) -> Option<i32> {
    if name.is_empty() || name.len() > 9 || (name.len() > 1 && name[0] == b'0') {
        return None;
    }
    let mut v: i32 = 0;
    for &c in name {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i32;
    }
    Some(v)
}

/// Does resolving `path` from `start` end at a magic link? `follow`:
/// whether a final symlink is followed. Errors are left to the normal
/// resolution that runs when this says no.
pub fn classify(fs: &mut Fs, start: Ino, path: &[u8], follow: bool) -> Option<Magic> {
    let (fdd, pidd) = (procfs::fd_dir(), procfs::pid_dir());
    if fdd == 0 {
        return None;
    }
    let mut start = start;
    let mut p = path.to_vec();
    for _ in 0..=MAXSYMLINKS {
        let lc = fs.lookup_parent(start, &p).ok()?;
        let name = lc.name?;
        if lc.dir == fdd {
            return parse_fd(&name).map(Magic::Fd);
        }
        if lc.dir == pidd && name == b"cwd" {
            return Some(Magic::Cwd);
        }
        if !follow && !lc.trailing {
            return None;
        }
        let ino = fs.resolve(lc.dir, &name, false).ok()?;
        if fs.file_type(ino).ok()? != FileType::Symlink {
            return None;
        }
        p = fs.readlink_ino(ino).ok()?;
        start = lc.dir;
    }
    None
}

/// What a metadata operation acts on.
pub enum Node<'a> {
    /// A VFS inode (FS locked).
    Vfs(&'a mut Fs, Ino),
    /// An open file that has no inode in the tree (a pipe, eventfd...),
    /// reached through `/proc/self/fd/N` or `AT_EMPTY_PATH`.
    File(&'a Arc<OpenFile>),
    /// A magic link itself (not followed): its target and `lstat`.
    Link { target: &'a [u8], st: Stat },
}

fn node_of_file<R>(
    of: &Arc<OpenFile>,
    f: impl FnOnce(Node<'_>) -> Result<R, Errno>,
) -> Result<R, Errno> {
    match of.file.vnode() {
        // The description holds a reference: the inode is alive.
        Some(ino) => vfs::with(|fs| f(Node::Vfs(fs, ino))),
        None => f(Node::File(of)),
    }
}

/// Resolve `(dirfd, path)` and run `f` on what it names. `follow`: follow
/// a final symlink; `empty`: an empty path names `dirfd` itself
/// (`AT_EMPTY_PATH`), else it is `ENOENT`.
pub fn with_node<R>(
    dirfd: i32,
    path: &[u8],
    follow: bool,
    empty: bool,
    f: impl FnOnce(Node<'_>) -> Result<R, Errno>,
) -> Result<R, Errno> {
    if path.is_empty() {
        if !empty {
            return Err(Errno(ENOENT));
        }
        if dirfd == AT_FDCWD {
            return vfs::with(|fs| {
                let c = cwd();
                f(Node::Vfs(fs, c))
            });
        }
        let of = fdget(dirfd)?;
        return node_of_file(&of, f);
    }
    let base = Base::new(dirfd, path)?;
    let mut f = Some(f);
    let step = vfs::with(|fs| -> Result<Result<R, Magic>, Errno> {
        let start = base.start(fs)?;
        if let Some(m) = classify(fs, start, path, follow) {
            return Ok(Err(m));
        }
        let ino = fs.resolve(start, path, follow)?;
        match f.take() {
            Some(f) => f(Node::Vfs(fs, ino)).map(Ok),
            None => Err(Errno(EINVAL)),
        }
    })?;
    let m = match step {
        Ok(v) => return Ok(v),
        Err(m) => m,
    };
    let f = f.ok_or(Errno(EINVAL))?;
    match m {
        Magic::Fd(n) => {
            let of = fdget(n).map_err(|_| Errno(ENOENT))?;
            if follow {
                node_of_file(&of, f)
            } else {
                let t = of.file.proc_name();
                let st = procfs::link_stat(Some(n), t.len(), of.flags().writable());
                f(Node::Link { target: &t, st })
            }
        }
        Magic::Cwd => {
            if follow {
                vfs::with(|fs| {
                    let c = cwd();
                    f(Node::Vfs(fs, c))
                })
            } else {
                let t = vfs::with(|fs| fs.getcwd(cwd())).unwrap_or_else(|_| b"/".to_vec());
                let st = procfs::link_stat(None, t.len(), false);
                f(Node::Link { target: &t, st })
            }
        }
    }
}

/// Lexically normalise an absolute path (`.`, `..`, repeated `/`).
fn normalize(p: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for c in p.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = Vec::with_capacity(p.len() + 1);
    for c in parts {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// The absolute name an open file is shown with (`/proc/self/fd/N`).
fn abs_name(fs: &Fs, start: Ino, path: &[u8]) -> Vec<u8> {
    if path.first() == Some(&b'/') {
        return normalize(path);
    }
    let mut full = fs.getcwd(start).unwrap_or_default();
    full.push(b'/');
    full.extend_from_slice(path);
    normalize(&full)
}

/// A FIFO inode's kernel side.
struct FifoNode(Fifo);

impl DevNode for FifoNode {
    fn open(&self, _ino: Ino, flags: u32) -> Result<Arc<dyn File>, Errno> {
        let acc = flags & O_ACCMODE;
        self.0.open(
            acc == O_RDONLY || acc == O_RDWR,
            acc == O_WRONLY || acc == O_RDWR,
            flags & O_NONBLOCK != 0,
        )
    }
}

/// What `build` needs to know about an inode (gathered under the lock).
struct Info {
    ftype: FileType,
    special: Option<Arc<dyn Special>>,
    rdev: u64,
}

fn info(fs: &mut Fs, ino: Ino) -> Result<Info, Errno> {
    let ftype = fs.file_type(ino)?;
    let mut special = fs.special(ino);
    if ftype == FileType::Fifo && special.is_none() {
        let s = vfs::special(Arc::new(FifoNode(Fifo::new())));
        fs.set_special(ino, Some(s.clone()))?;
        special = Some(s);
    }
    Ok(Info {
        ftype,
        special,
        rdev: fs.rdev(ino)?,
    })
}

/// Build the open file description for inode `h` (outside every lock).
fn build(h: VHandle, inf: Info, flags: u32, name: Vec<u8>) -> Result<Arc<OpenFile>, Errno> {
    let path_only = flags & O_PATH != 0;
    let ino = h.ino();
    if ino == procfs::task_dir() && !path_only {
        vfs::with(procfs::sync_tasks);
    }
    let file: Arc<dyn File> = if path_only {
        Arc::new(PathFile::new(h, name))
    } else {
        let dev = || -> Option<Arc<dyn DevNode>> {
            match &inf.special {
                Some(s) => vfs::devnode(s),
                None if inf.ftype == FileType::CharDevice => vfs::chrdev(inf.rdev),
                None => None,
            }
        };
        match inf.ftype {
            FileType::Regular => match dev() {
                Some(d) => Arc::new(NodeFile::new(h, d.open(ino, flags)?, name)),
                None => Arc::new(RegFile::new(h, name)),
            },
            FileType::Directory if ino == procfs::fd_dir() => Arc::new(FdDirFile::new(h)),
            FileType::Directory => Arc::new(DirFile::new(h)),
            FileType::Symlink => return Err(Errno(ELOOP)),
            _ => {
                let d = dev().ok_or(Errno(ENXIO))?;
                Arc::new(NodeFile::new(h, d.open(ino, flags)?, name))
            }
        }
    };
    let status = if path_only {
        O_RDONLY
    } else {
        flags & (O_ACCMODE | O_NONBLOCK | O_APPEND)
    };
    Ok(OpenFile::new(file, status))
}

/// Decoded `openat` flags.
fn options(flags: u32, mode: u32) -> Result<OpenOptions, Errno> {
    if flags & O_TMPFILE_BIT != 0 {
        return Err(Errno(EOPNOTSUPP));
    }
    if flags & O_DIRECT != 0 {
        // tmpfs has no direct I/O.
        return Err(Errno(EINVAL));
    }
    let acc = flags & O_ACCMODE;
    if acc == O_ACCMODE {
        return Err(Errno(EINVAL));
    }
    let umask = task::process().umask.load(Ordering::Relaxed);
    Ok(OpenOptions {
        write: acc != O_RDONLY,
        create: flags & O_CREAT != 0,
        excl: flags & O_EXCL != 0,
        trunc: flags & O_TRUNC != 0,
        directory: flags & O_DIRECTORY != 0,
        nofollow: flags & O_NOFOLLOW != 0,
        path: flags & O_PATH != 0,
        mode: mode & 0o7777 & !umask,
    })
}

/// An inode `open` resolved (holding a reference), its info and name.
type Resolved = (Ino, Info, Vec<u8>);

/// `openat(dirfd, path, flags, mode)`.
pub fn open(dirfd: i32, path: &[u8], flags: u32, mode: u32) -> Result<Arc<OpenFile>, Errno> {
    let mut opts = options(flags, mode)?;
    if opts.path {
        // O_PATH ignores everything but O_DIRECTORY / O_NOFOLLOW / O_CLOEXEC.
        opts = OpenOptions {
            directory: opts.directory,
            nofollow: opts.nofollow,
            path: true,
            ..Default::default()
        };
    }
    if path.is_empty() {
        return Err(Errno(ENOENT));
    }
    let base = Base::new(dirfd, path)?;
    let follow = !opts.nofollow && !(opts.create && opts.excl);
    let step = vfs::with(|fs| -> Result<Result<Resolved, Magic>, Errno> {
        let start = base.start(fs)?;
        if let Some(m) = classify(fs, start, path, follow) {
            return Ok(Err(m));
        }
        let o = fs.open(start, path, &opts)?;
        // `o.ino` carries a reference for us from here on.
        match info(fs, o.ino) {
            Ok(inf) => Ok(Ok((o.ino, inf, abs_name(fs, start, path)))),
            Err(e) => {
                fs.put_ref(o.ino);
                Err(e)
            }
        }
    })?;
    match step {
        Ok((ino, inf, name)) => build(VHandle::adopt(KFs, ino), inf, flags, name),
        Err(Magic::Fd(n)) => {
            let of = fdget(n).map_err(|_| Errno(ENOENT))?;
            if opts.nofollow && !opts.path {
                return Err(Errno(ELOOP));
            }
            match of.file.vnode() {
                Some(ino) => open_ino(Some(ino), flags, &opts, of.file.proc_name()),
                None if opts.directory => Err(Errno(ENOTDIR)),
                // Not a VFS file (pipe, eventfd...): share the description.
                None => Ok(of),
            }
        }
        Err(Magic::Cwd) => {
            if opts.nofollow && !opts.path {
                return Err(Errno(ELOOP));
            }
            open_ino(None, flags, &opts, Vec::new())
        }
    }
}

/// Open inode `ino` (`None`: the cwd) as `open` would after resolving to
/// it: `O_DIRECTORY`, `EISDIR` for writable directories, `O_TRUNC`.
pub fn open_ino(
    ino: Option<Ino>,
    flags: u32,
    opts: &OpenOptions,
    name: Vec<u8>,
) -> Result<Arc<OpenFile>, Errno> {
    let (ino, inf, name) = vfs::with(|fs| -> Result<(Ino, Info, Vec<u8>), Errno> {
        let ino = ino.unwrap_or_else(cwd);
        let t = fs.file_type(ino)?;
        if opts.directory && t != FileType::Directory {
            return Err(Errno(ENOTDIR));
        }
        if !opts.path && t == FileType::Directory && (opts.write || opts.trunc) {
            return Err(Errno(EISDIR));
        }
        if !opts.path && opts.trunc && t == FileType::Regular && fs.special(ino).is_none() {
            fs.truncate(ino, 0)?;
        }
        let inf = info(fs, ino)?;
        let name = if name.is_empty() {
            fs.getcwd(ino).unwrap_or_default()
        } else {
            name
        };
        fs.get_ref(ino)?;
        Ok((ino, inf, name))
    })?;
    build(VHandle::adopt(KFs, ino), inf, flags, name)
}

/// The VFS inode of `fd` for an fd-based metadata call (`fchmod`...):
/// `EBADF` for `O_PATH` descriptors, `None` for non-VFS files.
pub fn fd_vnode(fd: i32, allow_opath: bool) -> Result<(Arc<OpenFile>, Option<Ino>), Errno> {
    let of = fdget(fd)?;
    if of.file.is_opath() && !allow_opath {
        return Err(Errno(EBADF));
    }
    let v = of.file.vnode();
    Ok((of, v))
}
