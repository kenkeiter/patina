//! Namespace and data operations.

use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::fs as abi;
use patina_abi::time::{Timespec, NSEC_PER_SEC};

use crate::data::FileData;
use crate::dir::Dir;
use crate::fs::{FileType, Kind, Special};
use crate::path::{check_path, Last, Parent};
use crate::{e, Fs, Ino, Result, MAXSYMLINKS, PATH_MAX};

/// `open`/`openat` options, decoded from the arch-specific `O_*` flags by
/// the caller (`O_DIRECTORY` and `O_NOFOLLOW` differ between x86_64 and
/// aarch64). Flags that only affect the open file description (`O_APPEND`,
/// `O_NONBLOCK`, `O_CLOEXEC`, ...) are the caller's business.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOptions {
    /// Access mode is `O_WRONLY` or `O_RDWR`.
    pub write: bool,
    pub create: bool,
    pub excl: bool,
    pub trunc: bool,
    pub directory: bool,
    pub nofollow: bool,
    /// `O_PATH`: only `directory` and `nofollow` matter; a final symlink is
    /// returned itself under `nofollow`.
    pub path: bool,
    /// Permission bits for a created file, umask already applied.
    pub mode: u32,
}

/// Result of [`Fs::open`]. The caller owns one reference on `ino` and
/// must release it with [`Fs::put_ref`] (or wrap it in a
/// [`Handle`](crate::Handle) with `Handle::adopt`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    pub ino: Ino,
    pub created: bool,
    pub ftype: FileType,
}

/// One timestamp argument of `utimensat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetTime {
    /// `UTIME_NOW`.
    Now,
    /// `UTIME_OMIT`.
    Omit,
    At(Timespec),
}

impl SetTime {
    /// Decode a `struct timespec` from `utimensat` (`UTIME_NOW`/`UTIME_OMIT`
    /// in `tv_nsec`). `None` (a NULL `times` pointer) means "now".
    pub fn from_timespec(ts: Option<Timespec>) -> Result<SetTime> {
        use patina_abi::time::{UTIME_NOW, UTIME_OMIT};
        match ts {
            None => Ok(SetTime::Now),
            Some(t) if t.tv_nsec == UTIME_NOW => Ok(SetTime::Now),
            Some(t) if t.tv_nsec == UTIME_OMIT => Ok(SetTime::Omit),
            Some(t) if t.tv_nsec >= 0 && t.tv_nsec < NSEC_PER_SEC => Ok(SetTime::At(t)),
            Some(_) => Err(e::EINVAL),
        }
    }
}

impl Fs {
    /// `filename_create`: resolve the parent of a node about to be created
    /// and check the name is free.
    fn prepare_create(&self, start: Ino, path: &[u8], is_dir: bool) -> Result<(Ino, Vec<u8>)> {
        check_path(path)?;
        let mut links = 0;
        let p = self.walk_parent(start, path, &mut links)?;
        let name = match p.last {
            Last::Normal(n) => n,
            _ => return Err(e::EEXIST),
        };
        match self.lookup_in(p.dir, &name) {
            Ok(_) => return Err(e::EEXIST),
            Err(x) if x == e::ENOENT => {}
            Err(x) => return Err(x),
        }
        if p.trailing && !is_dir {
            return Err(e::ENOENT);
        }
        if self.is_dead_dir(p.dir) {
            return Err(e::ENOENT);
        }
        Ok((p.dir, name))
    }

    fn new_file(&mut self, dir: Ino, name: &[u8], perm: u32) -> Result<Ino> {
        let ino = self.alloc(
            dir,
            abi::S_IFREG | (perm & abi::S_IALLUGO),
            Kind::File(FileData::default()),
        );
        self.link_new(dir, name, ino)?;
        Ok(ino)
    }

    /// `openat`. On success the returned inode carries one new reference
    /// for the caller.
    pub fn open(&mut self, start: Ino, path: &[u8], opts: &OpenOptions) -> Result<Opened> {
        check_path(path)?;
        let mut o = *opts;
        if o.path {
            o.create = false;
            o.excl = false;
            o.trunc = false;
            o.write = false;
        }
        if o.create && o.directory {
            return Err(e::EINVAL);
        }
        let mut links = 0;
        let mut p = self.walk_parent(start, path, &mut links)?;
        let mut created = false;
        let ino = if !o.create {
            self.finish_lookup(p, !o.nofollow, &mut links)?.0
        } else {
            loop {
                let name = match &p.last {
                    Last::Normal(n) => n.clone(),
                    _ => return Err(e::EISDIR),
                };
                if p.trailing {
                    return Err(e::EISDIR);
                }
                match self.lookup_in(p.dir, &name) {
                    Ok(ino) => {
                        if o.excl {
                            return Err(e::EEXIST);
                        }
                        if let Kind::Symlink(t) = &self.node(ino)?.kind {
                            if o.nofollow {
                                return Err(e::ELOOP);
                            }
                            links += 1;
                            if links > MAXSYMLINKS {
                                return Err(e::ELOOP);
                            }
                            if t.is_empty() {
                                return Err(e::ENOENT);
                            }
                            let t = t.clone();
                            p = self.walk_parent(p.dir, &t, &mut links)?;
                            continue;
                        }
                        break ino;
                    }
                    Err(x) if x == e::ENOENT => {
                        if self.is_dead_dir(p.dir) {
                            return Err(e::ENOENT);
                        }
                        created = true;
                        break self.new_file(p.dir, &name, o.mode)?;
                    }
                    Err(x) => return Err(x),
                }
            }
        };
        let n = self.node(ino)?;
        let ftype = n.ftype();
        if o.directory && ftype != FileType::Directory {
            return Err(e::ENOTDIR);
        }
        if ftype == FileType::Symlink && !o.path {
            return Err(e::ELOOP);
        }
        if ftype == FileType::Directory && (o.write || o.trunc || o.create) {
            return Err(e::EISDIR);
        }
        if o.trunc && !created && matches!(n.kind, Kind::File(_)) {
            self.truncate(ino, 0)?;
        }
        self.get_ref(ino)?;
        Ok(Opened {
            ino,
            created,
            ftype,
        })
    }

    /// `mkdirat`. `mode` has the umask applied; only `0o1777` is kept
    /// (plus set-group-ID inherited from the parent).
    pub fn mkdir(&mut self, start: Ino, path: &[u8], mode: u32) -> Result<Ino> {
        let (dir, name) = self.prepare_create(start, path, true)?;
        let ino = self.alloc(
            dir,
            abi::S_IFDIR | (mode & 0o1777),
            Kind::Dir(Dir::new(dir, &name)),
        );
        self.link_new(dir, &name, ino)?;
        Ok(ino)
    }

    /// `mknodat`: a regular file (`S_IFREG` or type 0), or a special node
    /// (`S_IFCHR`, `S_IFBLK`, `S_IFIFO`, `S_IFSOCK`) carrying the kernel's
    /// optional handle. A regular-typed node with a handle is a generated
    /// (procfs-style) file. `S_IFDIR` gives `EPERM` as on Linux.
    pub fn mknod(
        &mut self,
        start: Ino,
        path: &[u8],
        mode: u32,
        rdev: u64,
        special: Option<Arc<dyn Special>>,
    ) -> Result<Ino> {
        let ftype = match mode & abi::S_IFMT {
            0 | abi::S_IFREG => abi::S_IFREG,
            t @ (abi::S_IFCHR | abi::S_IFBLK | abi::S_IFIFO | abi::S_IFSOCK) => t,
            abi::S_IFDIR => return Err(e::EPERM),
            _ => return Err(e::EINVAL),
        };
        let (dir, name) = self.prepare_create(start, path, false)?;
        let kind = if ftype == abi::S_IFREG && special.is_none() {
            Kind::File(FileData::default())
        } else {
            Kind::Special(special)
        };
        let ino = self.alloc(dir, ftype | (mode & abi::S_IALLUGO), kind);
        if matches!(ftype, abi::S_IFCHR | abi::S_IFBLK) {
            self.node_mut(ino)?.rdev = rdev;
        }
        self.link_new(dir, &name, ino)?;
        Ok(ino)
    }

    /// `symlinkat(target, start, path)`.
    pub fn symlink(&mut self, target: &[u8], start: Ino, path: &[u8]) -> Result<Ino> {
        if target.is_empty() {
            return Err(e::ENOENT);
        }
        if target.len() >= PATH_MAX {
            return Err(e::ENAMETOOLONG);
        }
        let (dir, name) = self.prepare_create(start, path, false)?;
        let ino = self.alloc(dir, abi::S_IFLNK | 0o777, Kind::Symlink(target.to_vec()));
        self.link_new(dir, &name, ino)?;
        Ok(ino)
    }

    /// `linkat`: resolve `old_path` (following a final symlink only if
    /// `follow`, i.e. `AT_SYMLINK_FOLLOW`) and link it at `new_path`.
    pub fn link(
        &mut self,
        old_start: Ino,
        old_path: &[u8],
        follow: bool,
        new_start: Ino,
        new_path: &[u8],
    ) -> Result<()> {
        let old = self.resolve(old_start, old_path, follow)?;
        self.link_ino(old, new_start, new_path)
    }

    /// `linkat` with an already-resolved source (`AT_EMPTY_PATH`).
    pub fn link_ino(&mut self, old: Ino, new_start: Ino, new_path: &[u8]) -> Result<()> {
        let (dir, name) = self.prepare_create(new_start, new_path, false)?;
        let n = self.node(old)?;
        if n.is_dir() {
            return Err(e::EPERM);
        }
        if n.nlink == 0 {
            return Err(e::ENOENT);
        }
        self.add_entry(dir, &name, old)
    }

    /// `unlinkat(.., 0)`.
    pub fn unlink(&mut self, start: Ino, path: &[u8]) -> Result<()> {
        check_path(path)?;
        let mut links = 0;
        let p = self.walk_parent(start, path, &mut links)?;
        let name = p.normal().ok_or(e::EISDIR)?.to_vec();
        let ino = self.lookup_in(p.dir, &name)?;
        let is_dir = self.node(ino)?.is_dir();
        if is_dir {
            return Err(e::EISDIR);
        }
        if p.trailing {
            return Err(e::ENOTDIR);
        }
        self.remove_entry(p.dir, &name)?;
        Ok(())
    }

    /// `unlinkat(.., AT_REMOVEDIR)` / `rmdir`.
    pub fn rmdir(&mut self, start: Ino, path: &[u8]) -> Result<()> {
        check_path(path)?;
        let mut links = 0;
        let p = self.walk_parent(start, path, &mut links)?;
        let name = match &p.last {
            Last::Normal(n) => n.clone(),
            Last::Dot => return Err(e::EINVAL),
            Last::DotDot => return Err(e::ENOTEMPTY),
            Last::Root => return Err(e::EBUSY),
        };
        let ino = self.lookup_in(p.dir, &name)?;
        let d = self.dir(ino)?;
        if !d.is_empty() {
            return Err(e::ENOTEMPTY);
        }
        self.remove_entry(p.dir, &name)?;
        Ok(())
    }

    /// `renameat2`. `flags` are `RENAME_NOREPLACE` / `RENAME_EXCHANGE`
    /// (`RENAME_WHITEOUT` is not supported: `EINVAL`).
    pub fn rename(
        &mut self,
        old_start: Ino,
        old_path: &[u8],
        new_start: Ino,
        new_path: &[u8],
        flags: u32,
    ) -> Result<()> {
        use abi::{RENAME_EXCHANGE as EXCH, RENAME_NOREPLACE as NOREP, RENAME_WHITEOUT as WHITE};
        if flags & !(NOREP | EXCH | WHITE) != 0
            || (flags & NOREP != 0 && flags & EXCH != 0)
            || flags & WHITE != 0
        {
            return Err(e::EINVAL);
        }
        let exch = flags & EXCH != 0;
        check_path(old_path)?;
        check_path(new_path)?;
        let mut links = 0;
        let op: Parent = self.walk_parent(old_start, old_path, &mut links)?;
        let mut links = 0;
        let np: Parent = self.walk_parent(new_start, new_path, &mut links)?;
        let oname = op.normal().ok_or(e::EBUSY)?.to_vec();
        let nname = np
            .normal()
            .ok_or(if flags & NOREP != 0 {
                e::EEXIST
            } else {
                e::EBUSY
            })?
            .to_vec();
        let (od, nd) = (op.dir, np.dir);

        let old = self.lookup_in(od, &oname)?;
        let new = match self.lookup_in(nd, &nname) {
            Ok(i) => Some(i),
            Err(x) if x == e::ENOENT => None,
            Err(x) => return Err(x),
        };
        if flags & NOREP != 0 && new.is_some() {
            return Err(e::EEXIST);
        }
        let old_is_dir = self.node(old)?.is_dir();
        let new_is_dir = match new {
            Some(n) => self.node(n)?.is_dir(),
            None => false,
        };
        if exch {
            if new.is_none() {
                return Err(e::ENOENT);
            }
            if !new_is_dir && np.trailing {
                return Err(e::ENOTDIR);
            }
        }
        if !old_is_dir && (op.trailing || (!exch && np.trailing)) {
            return Err(e::ENOTDIR);
        }
        if od != nd {
            // Source must not be an ancestor of the target, nor the target
            // an ancestor of the source.
            if old_is_dir && self.is_ancestor_or_self(old, nd) {
                return Err(e::EINVAL);
            }
            if let Some(n) = new {
                if new_is_dir && self.is_ancestor_or_self(n, od) {
                    return Err(if exch { e::EINVAL } else { e::ENOTEMPTY });
                }
            }
        }
        if new == Some(old) {
            return Ok(());
        }
        match new {
            None => {
                if self.is_dead_dir(nd) {
                    return Err(e::ENOENT);
                }
            }
            Some(n) if !exch => {
                if old_is_dir && !new_is_dir {
                    return Err(e::ENOTDIR);
                }
                if !old_is_dir && new_is_dir {
                    return Err(e::EISDIR);
                }
                if new_is_dir && !self.dir(n)?.is_empty() {
                    return Err(e::ENOTEMPTY);
                }
            }
            Some(_) => {}
        }

        let now = self.now();
        if exch {
            let n = new.ok_or(e::ENOENT)?;
            self.dir_mut(od)?.replace(&oname, n);
            self.dir_mut(nd)?.replace(&nname, old);
            if od != nd && old_is_dir != new_is_dir {
                let (from, to) = if old_is_dir { (od, nd) } else { (nd, od) };
                self.node_mut(from)?.nlink -= 1;
                self.node_mut(to)?.nlink += 1;
            }
            if old_is_dir {
                self.set_dir_parent(old, nd, &nname)?;
            }
            if new_is_dir {
                self.set_dir_parent(n, od, &oname)?;
            }
            self.node_mut(n)?.ctime = now;
        } else {
            if new.is_some() {
                self.remove_entry(nd, &nname)?;
            }
            self.dir_mut(od)?.remove(&oname);
            self.dir_mut(nd)?.insert(&nname, old);
            if old_is_dir {
                if od != nd {
                    self.node_mut(od)?.nlink -= 1;
                    self.node_mut(nd)?.nlink += 1;
                }
                self.set_dir_parent(old, nd, &nname)?;
            }
        }
        self.node_mut(old)?.ctime = now;
        for d in [od, nd] {
            let n = self.node_mut(d)?;
            n.mtime = now;
            n.ctime = now;
        }
        Ok(())
    }

    fn set_dir_parent(&mut self, dir: Ino, parent: Ino, name: &[u8]) -> Result<()> {
        let d = self.dir_mut(dir)?;
        d.parent = parent;
        d.name = name.to_vec();
        Ok(())
    }

    /// `readlinkat`: the target of the symlink at `path` (not followed).
    pub fn readlink(&mut self, start: Ino, path: &[u8]) -> Result<Vec<u8>> {
        let ino = self.resolve(start, path, false)?;
        self.readlink_ino(ino)
    }

    /// Target of symlink `ino`; `EINVAL` if it is not a symlink.
    pub fn readlink_ino(&mut self, ino: Ino) -> Result<Vec<u8>> {
        let t = match &self.node(ino)?.kind {
            Kind::Symlink(t) => t.clone(),
            _ => return Err(e::EINVAL),
        };
        self.touch_atime(ino);
        Ok(t)
    }

    /// `ftruncate`/`truncate` on a regular file. `EISDIR` for a directory,
    /// `EINVAL` for other non-regular files or a size beyond `i64::MAX`.
    pub fn truncate(&mut self, ino: Ino, size: u64) -> Result<()> {
        if size > i64::MAX as u64 {
            return Err(e::EINVAL);
        }
        let now = self.now();
        let n = self.node_mut(ino)?;
        let freed = match &mut n.kind {
            Kind::File(d) => d.truncate(size),
            Kind::Dir(_) => return Err(e::EISDIR),
            _ => return Err(e::EINVAL),
        };
        n.mtime = now;
        n.ctime = now;
        self.release(freed);
        Ok(())
    }

    /// Read from a regular file at `off`. Returns bytes read (0 at EOF).
    pub fn read_at(&mut self, ino: Ino, off: u64, buf: &mut [u8]) -> Result<usize> {
        if off > i64::MAX as u64 {
            return Err(e::EINVAL);
        }
        let n = match &self.node(ino)?.kind {
            Kind::File(d) => d.read(off, buf),
            Kind::Dir(_) => return Err(e::EISDIR),
            _ => return Err(e::EINVAL),
        };
        if !buf.is_empty() {
            self.touch_atime(ino);
        }
        Ok(n)
    }

    /// Write all of `data` to a regular file at `off`, extending it
    /// (all-or-nothing: `ENOSPC` if the page limit would be exceeded).
    pub fn write_at(&mut self, ino: Ino, off: u64, data: &[u8]) -> Result<usize> {
        let pages = match &self.node(ino)?.kind {
            Kind::File(d) => {
                if data.is_empty() {
                    return Ok(0);
                }
                let end = off.checked_add(data.len() as u64).ok_or(e::EFBIG)?;
                if end > i64::MAX as u64 {
                    return Err(e::EFBIG);
                }
                d.pages_needed(off, data.len())
            }
            Kind::Dir(_) => return Err(e::EISDIR),
            _ => return Err(e::EINVAL),
        };
        self.charge(pages)?;
        let now = self.now();
        let n = self.node_mut(ino)?;
        if let Kind::File(d) = &mut n.kind {
            d.write(off, data);
        }
        n.mtime = now;
        n.ctime = now;
        Ok(data.len())
    }

    /// `O_APPEND` write: at the current size, atomically under the caller's
    /// lock. Returns bytes written and the new end-of-file offset.
    pub fn append(&mut self, ino: Ino, data: &[u8]) -> Result<(usize, u64)> {
        let off = self.size(ino)?;
        let n = self.write_at(ino, off, data)?;
        Ok((n, off + n as u64))
    }

    /// `getdents64`: serialise entries of directory `ino` starting at
    /// stream position `*pos` into `buf`, advancing `*pos`. Returns bytes
    /// written; 0 at end of directory. `EINVAL` if the next entry does not
    /// fit in an empty `buf`; `ENOENT` if the directory was removed.
    pub fn getdents64(&mut self, ino: Ino, pos: &mut u64, buf: &mut [u8]) -> Result<usize> {
        let n = self.node(ino)?;
        let d = n.as_dir().ok_or(e::ENOTDIR)?;
        if n.nlink == 0 {
            return Err(e::ENOENT);
        }
        let parent = if ino == crate::ROOT_INO {
            ino
        } else {
            d.parent
        };
        let mut used = 0usize;
        let mut p = *pos;
        let mut full = false;
        // Emit one record; on success advance the stream position to `next`.
        let mut emit = |p: &mut u64, eino: Ino, next: u64, dtype: u8, name: &[u8]| -> bool {
            match abi::write_dirent64(&mut buf[used..], eino, next as i64, dtype, name) {
                Some(len) => {
                    used += len;
                    *p = next;
                    true
                }
                None => false,
            }
        };
        if p == 0 && !emit(&mut p, ino, 1, abi::DT_DIR, b".") {
            full = true;
        }
        if !full && p == 1 && !emit(&mut p, parent, 2, abi::DT_DIR, b"..") {
            full = true;
        }
        if !full {
            for (cookie, ent) in d.starting_at(p) {
                let dtype = self
                    .inodes
                    .get(&ent.ino)
                    .map_or(abi::DT_UNKNOWN, |c| c.ftype().dtype());
                if !emit(&mut p, ent.ino, cookie + 1, dtype, &ent.name) {
                    full = true;
                    break;
                }
            }
        }
        if full && used == 0 {
            return Err(e::EINVAL);
        }
        *pos = p;
        self.touch_atime(ino);
        Ok(used)
    }

    /// Entries of directory `ino` (without `.` and `..`), in readdir order.
    pub fn read_dir(&self, ino: Ino) -> Result<Vec<(Vec<u8>, Ino, FileType)>> {
        let d = self.dir(ino)?;
        Ok(d.entries()
            .map(|en| {
                let t = self
                    .inodes
                    .get(&en.ino)
                    .map_or(FileType::Regular, |c| c.ftype());
                (en.name.clone(), en.ino, t)
            })
            .collect())
    }

    /// `utimensat` on a resolved inode. Both `Omit` is a no-op; otherwise
    /// ctime is set to now as well.
    pub fn utimens(&mut self, ino: Ino, atime: SetTime, mtime: SetTime) -> Result<()> {
        let now = self.now();
        let n = self.node_mut(ino)?;
        if atime == SetTime::Omit && mtime == SetTime::Omit {
            return Ok(());
        }
        for (slot, v) in [(&mut n.atime, atime), (&mut n.mtime, mtime)] {
            match v {
                SetTime::Now => *slot = now,
                SetTime::At(t) => {
                    if !(0..NSEC_PER_SEC).contains(&t.tv_nsec) {
                        return Err(e::EINVAL);
                    }
                    *slot = t;
                }
                SetTime::Omit => {}
            }
        }
        n.ctime = now;
        Ok(())
    }

    /// `fchmod`/`fchmodat`: replace the permission bits (`& 0o7777`).
    pub fn chmod(&mut self, ino: Ino, mode: u32) -> Result<()> {
        let now = self.now();
        let n = self.node_mut(ino)?;
        n.mode = (n.mode & abi::S_IFMT) | (mode & abi::S_IALLUGO);
        n.ctime = now;
        Ok(())
    }

    /// `fchown`/`fchownat` (`None` = -1, unchanged). Like Linux, clears
    /// set-user-ID and (if group-executable) set-group-ID on non-directories.
    pub fn chown(&mut self, ino: Ino, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        let now = self.now();
        let n = self.node_mut(ino)?;
        if let Some(u) = uid {
            n.uid = u;
        }
        if let Some(g) = gid {
            n.gid = g;
        }
        if !n.is_dir() && !matches!(n.kind, Kind::Symlink(_)) {
            n.mode &= !abi::S_ISUID;
            if n.mode & 0o010 != 0 {
                n.mode &= !abi::S_ISGID;
            }
        }
        n.ctime = now;
        Ok(())
    }

    /// `faccessat` for the single (root) user: existence, plus `X_OK`
    /// requires an execute bit on non-directories.
    pub fn access(&self, ino: Ino, mode: u32) -> Result<()> {
        let n = self.node(ino)?;
        if mode & !(abi::R_OK | abi::W_OK | abi::X_OK) != 0 {
            return Err(e::EINVAL);
        }
        if mode & abi::X_OK != 0 && !n.is_dir() && n.mode & 0o111 == 0 {
            return Err(e::EACCES);
        }
        Ok(())
    }

    /// File type of `ino`.
    pub fn file_type(&self, ino: Ino) -> Result<FileType> {
        Ok(self.node(ino)?.ftype())
    }

    /// Current size of a regular file (`st_size` for others).
    pub fn size(&self, ino: Ino) -> Result<u64> {
        Ok(self.stat(ino)?.size)
    }

    /// Contents of regular file `ino` as a `'static` slice without copying,
    /// if it was populated by [`populate_from_cpio_static`](crate::populate_from_cpio_static)
    /// and not modified since (`None` otherwise: read it with [`Fs::read_at`]).
    pub fn static_contents(&self, ino: Ino) -> Option<&'static [u8]> {
        match &self.inodes.get(&ino)?.kind {
            Kind::File(d) => d.as_static(),
            _ => None,
        }
    }

    /// The kernel handle of a special node, if any.
    pub fn special(&self, ino: Ino) -> Option<Arc<dyn Special>> {
        match &self.inodes.get(&ino)?.kind {
            Kind::Special(s) => s.clone(),
            _ => None,
        }
    }

    /// Attach (or detach) the kernel handle of a special node. `EINVAL` if
    /// `ino` is not a special node.
    pub fn set_special(&mut self, ino: Ino, special: Option<Arc<dyn Special>>) -> Result<()> {
        match &mut self.node_mut(ino)?.kind {
            Kind::Special(s) => {
                *s = special;
                Ok(())
            }
            _ => Err(e::EINVAL),
        }
    }

    /// `rdev` of a device node (0 otherwise).
    pub fn rdev(&self, ino: Ino) -> Result<u64> {
        Ok(self.node(ino)?.rdev)
    }

    /// Whether `ino` is a directory that has been removed (`nlink == 0`).
    pub fn is_removed_dir(&self, ino: Ino) -> bool {
        self.is_dead_dir(ino)
    }

    /// Create `path` and any missing parents as directories (`mkdir -p`),
    /// returning the final directory. Existing directories (or symlinks to
    /// them) are fine; anything else in the way gives `ENOTDIR`/`EEXIST`.
    pub fn mkdir_all(&mut self, start: Ino, path: &[u8], mode: u32) -> Result<Ino> {
        check_path(path)?;
        let mut cur = if path[0] == b'/' {
            crate::ROOT_INO
        } else {
            start
        };
        for comp in path.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
            cur = match self.resolve(cur, comp, true) {
                Ok(i) => {
                    if !self.node(i)?.is_dir() {
                        return Err(e::ENOTDIR);
                    }
                    i
                }
                Err(x) if x == e::ENOENT => self.mkdir(cur, comp, mode)?,
                Err(x) => return Err(x),
            };
        }
        Ok(cur)
    }
}
