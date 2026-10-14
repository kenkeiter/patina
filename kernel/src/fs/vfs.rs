//! The root filesystem (design §8): one `patina_vfs::Fs` behind a global
//! IRQ-safe `SpinLock`, populated from the initrd at boot (tmpfs
//! semantics: writable, in memory).
//!
//! # Locking and lifetimes
//!
//! Every filesystem operation runs inside [`with`] (one critical section
//! per syscall step, which is what makes `O_CREAT|O_EXCL`, `rename` and
//! `O_APPEND` atomic). Inode references outside a critical section are
//! [`VHandle`]s (`patina_vfs::Handle<KFs>`); dropping one takes the lock,
//! so **never drop a `VHandle`, an `Arc<OpenFile>` or an `Arc<dyn File>`
//! inside [`with`]**. Code that obtains a reference inside the lock
//! (`Fs::open`, `Fs::get_ref`) returns the raw inode number out of the
//! closure and wraps it with `VHandle::adopt` after unlocking. The process
//! cwd is a raw reference (`Process::cwd`) changed only under this lock.
//!
//! Lock order: fd table → (released); a file's own lock (e.g. a regular
//! file's offset) → FS lock → `Process::cwd` / heap. Nothing takes the fd
//! table or a file lock while holding the FS lock.
//!
//! # Special nodes
//!
//! Device nodes, FIFOs and generated (procfs-style) files carry a
//! [`KSpecial`] (the kernel's `patina_vfs::Special`), which wraps a
//! [`DevNode`]: opening the node calls [`DevNode::open`] to build a
//! free-standing `File`. Character devices without a handle (e.g. from
//! `mknod` entries in the initrd) are bound at open time by `rdev` from
//! the registry ([`register_chrdev`]); FIFOs get a fresh
//! [`pipe::Fifo`](super::pipe::Fifo) the first time they are opened.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::errno::Errno;
use patina_abi::time::Timespec;
use patina_vfs::{Fs, FsRef, Handle, Ino, Special};

use super::file::File;
use crate::sync::{OnceCell, SpinLock};

static FS: OnceCell<SpinLock<Fs>> = OnceCell::new();
/// The initial working directory (`patina.cwd`), if not the root. The
/// cell holds a reference on it for the life of the system.
static INITIAL_CWD: OnceCell<Ino> = OnceCell::new();

/// The global filesystem as a `patina_vfs::FsRef`.
#[derive(Clone, Copy, Debug)]
pub struct KFs;

impl FsRef for KFs {
    fn with<R>(&self, f: impl FnOnce(&mut Fs) -> R) -> R {
        with(f)
    }
}

/// A counted reference on an inode of the root filesystem.
pub type VHandle = Handle<KFs>;

/// Run `f` with the filesystem locked. See the module docs for what must
/// not happen inside.
pub fn with<R>(f: impl FnOnce(&mut Fs) -> R) -> R {
    let fs = FS.get().expect("vfs: not initialised");
    f(&mut fs.lock())
}

/// Whether [`init`] has run.
pub fn is_ready() -> bool {
    FS.get().is_some()
}

/// Create the filesystem (empty root). `epoch_ns` is `patina.epoch`, used
/// for timestamps until the process exists.
pub fn init(epoch_ns: u64) {
    let clock = move || {
        // Before the process exists `wallclock::epoch_ns()` is 0: use the
        // cmdline epoch until then; afterwards it follows `time` ops.
        let epoch = match crate::wallclock::epoch_ns() {
            0 => epoch_ns,
            e => e,
        };
        let ns = crate::arch::timer::now_ns().saturating_add(epoch);
        Timespec {
            tv_sec: (ns / 1_000_000_000) as i64,
            tv_nsec: (ns % 1_000_000_000) as i64,
        }
    };
    let mut fs = Fs::new(Box::new(clock));
    // tmpfs default: file data may use half of RAM.
    let (_, total) = crate::mm::frame::stats();
    fs.set_page_limit(Some(total as u64 / 2));
    if FS.set(SpinLock::new(fs)).is_err() {
        panic!("vfs: initialised twice");
    }
}

/// A reference on the initial cwd: the `patina.cwd` directory
/// ([`set_initial_cwd`]), else the root.
pub fn root_ref() -> Ino {
    with(|fs| {
        let r = INITIAL_CWD.get().copied().unwrap_or_else(|| fs.root());
        let _ = fs.get_ref(r);
        r
    })
}

/// Make the directory `ino` the initial cwd. FS locked; takes a
/// reference that is never dropped. Only the first call has an effect.
pub fn set_initial_cwd(fs: &mut Fs, ino: Ino) {
    if INITIAL_CWD.get().is_none() && fs.get_ref(ino).is_ok() && INITIAL_CWD.set(ino).is_err() {
        fs.put_ref(ino);
    }
}

/// Kernel behaviour of a special node (device, FIFO, generated file).
pub trait DevNode: Send + Sync {
    /// Build the `File` for one open of inode `ino` with open flags
    /// `flags` (access mode, `O_NONBLOCK`, ...). Called without any lock
    /// held; may block (FIFOs).
    fn open(&self, ino: Ino, flags: u32) -> Result<Arc<dyn File>, Errno>;

    /// `st_size` to report (0 like procfs by default).
    fn size(&self) -> u64 {
        0
    }
}

/// The kernel's `Special`: what `patina_vfs` stores in a special inode.
pub struct KSpecial(pub Arc<dyn DevNode>);

impl Special for KSpecial {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn size(&self) -> u64 {
        self.0.size()
    }
}

/// Wrap a `DevNode` for `Fs::mknod` / `Fs::set_special`.
pub fn special(node: Arc<dyn DevNode>) -> Arc<dyn Special> {
    Arc::new(KSpecial(node))
}

/// The `DevNode` behind a special inode's handle (`None` if it is not
/// the kernel's).
pub fn devnode(s: &Arc<dyn Special>) -> Option<Arc<dyn DevNode>> {
    s.as_any().downcast_ref::<KSpecial>().map(|k| k.0.clone())
}

static CHRDEVS: SpinLock<Vec<(u64, Arc<dyn DevNode>)>> = SpinLock::new(Vec::new());

/// Register the driver for character device `rdev` (`makedev(major,
/// minor)`): device nodes with that `rdev` and no handle of their own open
/// through it.
pub fn register_chrdev(rdev: u64, node: Arc<dyn DevNode>) {
    let mut d = CHRDEVS.lock();
    d.retain(|(r, _)| *r != rdev);
    d.push((rdev, node));
}

/// The registered driver for `rdev`.
pub fn chrdev(rdev: u64) -> Option<Arc<dyn DevNode>> {
    CHRDEVS
        .lock()
        .iter()
        .find(|(r, _)| *r == rdev)
        .map(|(_, n)| n.clone())
}
