//! The inode table and the primitives the operations are built from.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::fs as abi;
use patina_abi::time::Timespec;

use crate::data::FileData;
use crate::dir::Dir;
use crate::{e, Ino, Result, ROOT_INO};

/// Source of the current wall-clock time for timestamps.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timespec;
}

impl<F: Fn() -> Timespec + Send + Sync> Clock for F {
    fn now(&self) -> Timespec {
        self()
    }
}

/// Kernel-supplied behaviour of a special node (device, FIFO, socket,
/// generated file). Opaque to this crate.
pub trait Special: Send + Sync {
    /// For downcasting to the kernel's concrete type.
    fn as_any(&self) -> &dyn core::any::Any;
    /// `st_size` to report (generated files report 0 like procfs).
    fn size(&self) -> u64 {
        0
    }
}

/// Inode type, from the `S_IFMT` bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

impl FileType {
    pub fn from_mode(mode: u32) -> Option<FileType> {
        Some(match mode & abi::S_IFMT {
            abi::S_IFREG => FileType::Regular,
            abi::S_IFDIR => FileType::Directory,
            abi::S_IFLNK => FileType::Symlink,
            abi::S_IFCHR => FileType::CharDevice,
            abi::S_IFBLK => FileType::BlockDevice,
            abi::S_IFIFO => FileType::Fifo,
            abi::S_IFSOCK => FileType::Socket,
            _ => return None,
        })
    }

    pub fn mode_bits(self) -> u32 {
        match self {
            FileType::Regular => abi::S_IFREG,
            FileType::Directory => abi::S_IFDIR,
            FileType::Symlink => abi::S_IFLNK,
            FileType::CharDevice => abi::S_IFCHR,
            FileType::BlockDevice => abi::S_IFBLK,
            FileType::Fifo => abi::S_IFIFO,
            FileType::Socket => abi::S_IFSOCK,
        }
    }

    /// `d_type` for `getdents64`.
    pub fn dtype(self) -> u8 {
        abi::mode_to_dtype(self.mode_bits())
    }
}

pub(crate) enum Kind {
    File(FileData),
    Dir(Dir),
    Symlink(Vec<u8>),
    Special(Option<Arc<dyn Special>>),
}

impl core::fmt::Debug for Kind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Kind::File(d) => d.fmt(f),
            Kind::Dir(d) => f.debug_struct("Dir").field("entries", &d.len()).finish(),
            Kind::Symlink(t) => f.debug_tuple("Symlink").field(&t.len()).finish(),
            Kind::Special(s) => f.debug_tuple("Special").field(&s.is_some()).finish(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Inode {
    /// Type and permission bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub rdev: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub btime: Timespec,
    /// Outstanding references (open files, cwds, ...).
    pub refs: u32,
    pub kind: Kind,
}

impl Inode {
    pub fn ftype(&self) -> FileType {
        // Every inode is created with a valid type.
        FileType::from_mode(self.mode).unwrap_or(FileType::Regular)
    }
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir(_))
    }
    pub fn as_dir(&self) -> Option<&Dir> {
        match &self.kind {
            Kind::Dir(d) => Some(d),
            _ => None,
        }
    }
    pub fn as_dir_mut(&mut self) -> Option<&mut Dir> {
        match &mut self.kind {
            Kind::Dir(d) => Some(d),
            _ => None,
        }
    }
}

/// The filesystem: an inode table rooted at [`ROOT_INO`]. See the crate
/// docs for the locking and reference model.
pub struct Fs {
    pub(crate) inodes: BTreeMap<Ino, Inode>,
    next_ino: Ino,
    clock: Box<dyn Clock>,
    pub(crate) dev: u64,
    pub(crate) pages_used: u64,
    page_limit: Option<u64>,
    /// Owner given to new inodes.
    pub(crate) cred: (u32, u32),
}

impl core::fmt::Debug for Fs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fs")
            .field("inodes", &self.inodes.len())
            .field("pages_used", &self.pages_used)
            .finish()
    }
}

/// Maximum link count (tmpfs uses `~0U`; keep well clear of overflow).
const LINK_MAX: u32 = u32::MAX - 1;

impl Fs {
    /// An empty filesystem: just a root directory (`0755`, owner 0:0).
    pub fn new(clock: Box<dyn Clock>) -> Fs {
        let mut fs = Fs {
            inodes: BTreeMap::new(),
            next_ino: ROOT_INO + 1,
            clock,
            dev: abi::makedev(0, 1),
            pages_used: 0,
            page_limit: None,
            cred: (0, 0),
        };
        let now = fs.now();
        fs.inodes.insert(
            ROOT_INO,
            Inode {
                mode: abi::S_IFDIR | 0o755,
                uid: 0,
                gid: 0,
                nlink: 2,
                rdev: 0,
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                // The root is never freed.
                refs: 1,
                kind: Kind::Dir(Dir::new(ROOT_INO, b"")),
            },
        );
        fs
    }

    /// `st_dev` reported for every inode (default `makedev(0, 1)`).
    pub fn set_dev(&mut self, dev: u64) {
        self.dev = dev;
    }

    /// Limit the heap pages file data may use (`ENOSPC` beyond it). Pages
    /// borrowed from a `'static` backing (initrd) do not count until written.
    pub fn set_page_limit(&mut self, pages: Option<u64>) {
        self.page_limit = pages;
    }

    /// Heap pages currently used by file data.
    pub fn pages_used(&self) -> u64 {
        self.pages_used
    }

    /// Owner (`uid`, `gid`) given to newly created inodes (default 0:0).
    pub fn set_owner(&mut self, uid: u32, gid: u32) {
        self.cred = (uid, gid);
    }

    /// Number of live inodes (including unlinked-but-open ones).
    pub fn inode_count(&self) -> usize {
        self.inodes.len()
    }

    pub fn root(&self) -> Ino {
        ROOT_INO
    }

    pub(crate) fn now(&self) -> Timespec {
        self.clock.now()
    }

    pub(crate) fn node(&self, ino: Ino) -> Result<&Inode> {
        self.inodes.get(&ino).ok_or(e::ENOENT)
    }

    pub(crate) fn node_mut(&mut self, ino: Ino) -> Result<&mut Inode> {
        self.inodes.get_mut(&ino).ok_or(e::ENOENT)
    }

    pub(crate) fn dir(&self, ino: Ino) -> Result<&Dir> {
        self.node(ino)?.as_dir().ok_or(e::ENOTDIR)
    }

    pub(crate) fn dir_mut(&mut self, ino: Ino) -> Result<&mut Dir> {
        self.node_mut(ino)?.as_dir_mut().ok_or(e::ENOTDIR)
    }

    /// Whether `ino` is a directory that has been removed.
    pub(crate) fn is_dead_dir(&self, ino: Ino) -> bool {
        self.inodes
            .get(&ino)
            .is_some_and(|n| n.is_dir() && n.nlink == 0)
    }

    /// Take a reference on `ino` (keeps it alive after its last unlink).
    pub fn get_ref(&mut self, ino: Ino) -> Result<()> {
        let n = self.node_mut(ino)?;
        n.refs = n.refs.checked_add(1).ok_or(e::EINVAL)?;
        Ok(())
    }

    /// Drop a reference taken by [`Fs::get_ref`] (or returned by
    /// [`Fs::open`]); frees the inode if it was the last and the inode has
    /// no links.
    pub fn put_ref(&mut self, ino: Ino) {
        if let Some(n) = self.inodes.get_mut(&ino) {
            debug_assert!(n.refs > 0, "put_ref underflow on {ino}");
            n.refs = n.refs.saturating_sub(1);
            self.maybe_free(ino);
        }
    }

    /// References currently held on `ino` (0 if it does not exist).
    pub fn ref_count(&self, ino: Ino) -> u32 {
        self.inodes.get(&ino).map_or(0, |n| n.refs)
    }

    pub(crate) fn maybe_free(&mut self, ino: Ino) {
        let free = match self.inodes.get(&ino) {
            Some(n) => n.nlink == 0 && n.refs == 0 && ino != ROOT_INO,
            None => false,
        };
        if free {
            if let Some(n) = self.inodes.remove(&ino) {
                if let Kind::File(d) = &n.kind {
                    self.pages_used -= d.owned_pages();
                }
            }
        }
    }

    /// Allocate an inode with `mode` and `kind`, owned per `cred` and the
    /// parent's set-group-ID bit. Not linked anywhere yet (`nlink` 0).
    pub(crate) fn alloc(&mut self, parent: Ino, mut mode: u32, kind: Kind) -> Ino {
        let ino = self.next_ino;
        self.next_ino += 1;
        let now = self.now();
        let (uid, mut gid) = self.cred;
        if let Ok(p) = self.node(parent) {
            if p.mode & abi::S_ISGID != 0 {
                gid = p.gid;
                if mode & abi::S_IFMT == abi::S_IFDIR {
                    mode |= abi::S_ISGID;
                }
            }
        }
        self.inodes.insert(
            ino,
            Inode {
                mode,
                uid,
                gid,
                nlink: 0,
                rdev: 0,
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                refs: 0,
                kind,
            },
        );
        ino
    }

    /// Add `name -> ino` to `dir` and count the link (and the child's `..`
    /// on the parent for directories). Updates the directory's times.
    pub(crate) fn add_entry(&mut self, dir: Ino, name: &[u8], ino: Ino) -> Result<()> {
        let now = self.now();
        let child_is_dir = self.node(ino)?.is_dir();
        {
            let c = self.node_mut(ino)?;
            if c.nlink >= LINK_MAX {
                return Err(e::EMLINK);
            }
            c.nlink += if child_is_dir { 2 } else { 1 };
            if !child_is_dir {
                c.ctime = now;
            }
        }
        if child_is_dir {
            if let Some(d) = self.node_mut(ino)?.as_dir_mut() {
                d.parent = dir;
                d.name = name.to_vec();
            }
        }
        let p = self.node_mut(dir)?;
        if child_is_dir {
            p.nlink += 1;
        }
        p.mtime = now;
        p.ctime = now;
        p.as_dir_mut().ok_or(e::ENOTDIR)?.insert(name, ino);
        Ok(())
    }

    /// `add_entry` for a freshly allocated inode: frees it again on failure.
    pub(crate) fn link_new(&mut self, dir: Ino, name: &[u8], ino: Ino) -> Result<()> {
        let r = self.add_entry(dir, name, ino);
        if r.is_err() {
            self.maybe_free(ino);
        }
        r
    }

    /// Remove `name` from `dir` and drop the link it held (a directory
    /// child loses both its links and its parent loses one). Frees the
    /// inode if nothing else holds it.
    pub(crate) fn remove_entry(&mut self, dir: Ino, name: &[u8]) -> Result<Ino> {
        let now = self.now();
        let ino = self.dir_mut(dir)?.remove(name).ok_or(e::ENOENT)?;
        let child_is_dir = self.node(ino)?.is_dir();
        let p = self.node_mut(dir)?;
        p.mtime = now;
        p.ctime = now;
        if child_is_dir {
            p.nlink -= 1;
        }
        let c = self.node_mut(ino)?;
        c.ctime = now;
        if child_is_dir {
            c.nlink = 0;
            if let Some(d) = c.as_dir_mut() {
                d.clear();
            }
        } else {
            c.nlink -= 1;
        }
        self.maybe_free(ino);
        Ok(ino)
    }

    /// Whether `anc` is `ino` or one of its ancestors (by `..` links).
    pub(crate) fn is_ancestor_or_self(&self, anc: Ino, mut ino: Ino) -> bool {
        loop {
            if ino == anc {
                return true;
            }
            if ino == ROOT_INO {
                return false;
            }
            match self.inodes.get(&ino).and_then(|n| n.as_dir()) {
                Some(d) if d.parent != ino => ino = d.parent,
                _ => return false,
            }
        }
    }

    /// Charge `pages` new heap pages against the limit.
    pub(crate) fn charge(&mut self, pages: u64) -> Result<()> {
        let total = self.pages_used.checked_add(pages).ok_or(e::ENOSPC)?;
        if let Some(lim) = self.page_limit {
            if total > lim {
                return Err(e::ENOSPC);
            }
        }
        self.pages_used = total;
        Ok(())
    }

    /// Update atime with relatime rules: only if it is not newer than
    /// mtime/ctime or is more than a day old.
    pub(crate) fn touch_atime(&mut self, ino: Ino) {
        let now = self.now();
        if let Some(n) = self.inodes.get_mut(&ino) {
            let older = |a: Timespec, b: Timespec| (a.tv_sec, a.tv_nsec) <= (b.tv_sec, b.tv_nsec);
            if older(n.atime, n.mtime)
                || older(n.atime, n.ctime)
                || now.tv_sec - n.atime.tv_sec >= 86400
            {
                n.atime = now;
            }
        }
    }
}
