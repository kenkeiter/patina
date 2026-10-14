//! The `File` trait: the contract every fd-backed object implements
//! (design §8). Pipes, eventfds, epoll instances and the console do today;
//! Phase 3's VFS files, `/dev` nodes, sockets and the control channel plug
//! in the same way.
//!
//! # Objects
//!
//! * A [`File`] is the object behind an open file description. One is
//!   created per `open`/`pipe2`/`socket`/... call, so per-description state
//!   that Linux keeps in `struct file` (a regular file's offset, a socket's
//!   state) lives inside the `File` implementation.
//! * An [`OpenFile`] is the open file description: the `File` plus its
//!   status flags (`O_NONBLOCK`, `O_APPEND`, the access mode). `dup`,
//!   `F_DUPFD` and `fork`-less sharing hand out more `Arc<OpenFile>`s, so
//!   `F_SETFL` on one fd is seen through its duplicates, as on Linux.
//! * The fd table (`fdtable.rs`) maps fd → (`Arc<OpenFile>`, `FD_CLOEXEC`).
//!   When the last `Arc<OpenFile>` goes, `File::release` runs (Linux
//!   `->release`): a pipe end drops its reader/writer count there.
//!
//! # Non-blocking operations, blocking in the caller
//!
//! `read`/`write` never block: they return `Err(EAGAIN)` when they would.
//! Blocking is done by the syscall layer ([`wait_ready`]):
//!
//! ```text
//! loop {
//!     match file.read(buf, status) {
//!         Err(EAGAIN) if !nonblocking => wait_ready(file, IN, deadline),
//!         r => return r,
//!     }
//! }
//! ```
//!
//! `wait_ready` creates a [`PollWaiter`], registers it through
//! `file.poll(Some(hook))`, and blocks only if the returned readiness does
//! not already include the wanted events (or ERR/HUP). Because the
//! registration happens before the readiness check, under the file's lock,
//! a state change that happens after the check finds the waiter and
//! notifies it; the waiter records the notification under its own lock,
//! so the wake-up cannot be lost (see `task::wait`). Wake-ups may be
//! spurious: the caller always retries the operation.
//!
//! # Readiness and hooks (the implementor's side)
//!
//! A pollable file keeps a [`PollList`] (inside its own `SpinLock`) and
//!
//! 1. in `poll(hook)`: under its lock, adds `hook` to the list (if given)
//!    and returns its current readiness as `EpollFlags` (`IN`, `OUT`,
//!    `HUP`, `ERR`, `RDHUP`, plus `RDNORM`/`WRNORM` like Linux);
//! 2. on every state change that can make it more ready (data arrived,
//!    space freed, peer closed): under its lock, `list.collect(events)`;
//!    then, **after releasing its lock**, `.fire()` the result. Hooks take
//!    other locks (an epoll instance's, a waiter's); calling them under the
//!    file's lock would invert the lock order with epoll, which polls files
//!    while holding no lock of its own but must never be re-entered.
//!
//! Hooks stay registered until the hook object is dropped (the list holds
//! `Weak` references and prunes dead ones), so epoll registers once at
//! `EPOLL_CTL_ADD` and keeps getting notifications; a blocking syscall's
//! `PollWaiter` disappears when the syscall returns.
//!
//! `events` passed to `collect` says what changed (`IN` for new data, `OUT`
//! for new space, `HUP`/`ERR` for a closed peer); epoll ignores
//! notifications that do not intersect an item's interest mask, exactly
//! like Linux's `ep_poll_callback` key filtering. Pass an empty set when
//! unsure: it is treated as "anything may have changed".
//!
//! # Phase 3 (VFS) fit
//!
//! A regular file is a `File` holding a `patina_vfs` `Handle` plus its
//! offset (in a lock inside the impl): it overrides `read`/`write` (at the
//! offset), `pread`/`pwrite`/`lseek` (defaults: `ESPIPE`), `stat`, and
//! returns `false` from `pollable` (`EPERM` from `epoll_ctl`, always ready
//! for `ppoll`). `/dev` nodes, FIFOs and sockets found by path carry an
//! `Arc<dyn Special>`; `open()` builds a free-standing `File` from it (the
//! pipe and console impls here depend on nothing but their own state, so
//! a named FIFO reuses `pipe::new`'s ends). `OpenFile`s are dropped — and
//! `File::release` / the impl's `Drop` run — only after the fd-table lock
//! is released and never under the VFS lock: a `File` that drops a
//! `Handle` must therefore take the fs lock itself in `release`/`Drop`,
//! and kernel code must never drop an `Arc<OpenFile>` while holding the
//! fs lock.
//!
//! # Lock order
//!
//! fd table → (released before any file operation); file lock → leaf
//! (heap, address space only for `ioctl` copying); epoll lock → wait queue
//! / sleep list; `PollWaiter` lock → sleep list. Files never call hooks
//! while holding their own lock, and nothing holds an epoll lock while
//! calling into a file.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::{EINVAL, ENOTDIR, ENOTTY, ESPIPE, Errno};
use patina_abi::fs::{O_ACCMODE, O_APPEND, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY};

use crate::sync::SpinLock;
use crate::task::WakeReason;
use crate::task::wait::{self, WaitQueue};

/// Something to tell about a readiness change: a [`PollWaiter`] (blocking
/// syscalls, `ppoll`) or an epoll registration.
pub trait PollHook: Send + Sync {
    /// The file's readiness may have changed; `events` says how (empty:
    /// unknown). Called with no file lock held; may take other locks.
    fn notify(&self, events: EpollFlags);
}

/// The hooks registered on a pollable object. Embed it in the object's
/// `SpinLock`.
#[derive(Default)]
pub struct PollList {
    hooks: Vec<Weak<dyn PollHook>>,
}

impl PollList {
    pub const fn new() -> PollList {
        PollList { hooks: Vec::new() }
    }

    /// Register `hook` (once; dead registrations are pruned).
    pub fn add(&mut self, hook: &Arc<dyn PollHook>) {
        let w = Arc::downgrade(hook);
        self.hooks.retain(|h| h.strong_count() > 0);
        if !self.hooks.iter().any(|h| Weak::ptr_eq(h, &w)) {
            self.hooks.push(w);
        }
    }

    /// The live hooks to notify of `events`; call [`Notify::fire`] after
    /// dropping the lock that protects this list.
    #[must_use = "fire the notification after releasing the lock"]
    pub fn collect(&mut self, events: EpollFlags) -> Notify {
        let mut live = Vec::new();
        self.hooks.retain(|h| match h.upgrade() {
            Some(a) => {
                live.push(a);
                true
            }
            None => false,
        });
        Notify {
            hooks: live,
            events,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

/// A pending notification (see [`PollList::collect`]).
#[must_use = "fire the notification after releasing the lock"]
pub struct Notify {
    hooks: Vec<Arc<dyn PollHook>>,
    events: EpollFlags,
}

impl Notify {
    pub const NONE: Notify = Notify {
        hooks: Vec::new(),
        events: EpollFlags::empty(),
    };

    /// Merge another pending notification into this one.
    pub fn and(mut self, other: Notify) -> Notify {
        self.events |= other.events;
        self.hooks.extend(other.hooks);
        self
    }

    /// Call every hook. Must run with no file lock held.
    pub fn fire(self) {
        for h in self.hooks {
            h.notify(self.events);
        }
    }
}

/// A one-shot waiter for one blocking syscall: registered on one or more
/// files with `poll(Some(..))`, then [`PollWaiter::wait`]ed on.
pub struct PollWaiter {
    st: SpinLock<WaiterState>,
}

struct WaiterState {
    notified: bool,
    queue: WaitQueue,
}

fn waiter_queue(s: &mut WaiterState) -> &mut WaitQueue {
    &mut s.queue
}

impl PollWaiter {
    pub fn new() -> Arc<PollWaiter> {
        Arc::new(PollWaiter {
            st: SpinLock::new(WaiterState {
                notified: false,
                queue: WaitQueue::new(),
            }),
        })
    }

    /// Block until notified (returns at once if already notified), until
    /// `deadline` (monotonic ns), or until a signal is pending for the
    /// thread (`WakeReason::Interrupted`: the caller returns `EINTR` or an
    /// internal restart code, see `task::signal`).
    pub fn wait(&self, deadline: Option<u64>) -> WakeReason {
        let g = self.st.lock();
        if g.notified {
            return WakeReason::Woken;
        }
        if deadline.is_some_and(|d| crate::arch::timer::now_ns() >= d) {
            return WakeReason::TimedOut;
        }
        wait::block_interruptible(g, waiter_queue, 0, 0, deadline)
    }
}

impl PollHook for PollWaiter {
    fn notify(&self, _events: EpollFlags) {
        let mut g = self.st.lock();
        g.notified = true;
        g.queue.wake_all();
    }
}

/// Status flags of an open file description (`F_GETFL`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileFlags(pub u32);

impl FileFlags {
    pub fn nonblocking(self) -> bool {
        self.0 & O_NONBLOCK != 0
    }
    pub fn append(self) -> bool {
        self.0 & O_APPEND != 0
    }
    pub fn readable(self) -> bool {
        matches!(self.0 & O_ACCMODE, O_RDONLY | O_RDWR)
    }
    pub fn writable(self) -> bool {
        matches!(self.0 & O_ACCMODE, O_WRONLY | O_RDWR)
    }
}

/// An fd-backed kernel object. See the module docs for the protocol.
pub trait File: Send + Sync {
    /// Short type name for diagnostics (`"pipe"`, `"eventfd"`...).
    fn kind(&self) -> &'static str;

    /// Read into `buf` without blocking: `Ok(0)` at end of file,
    /// `Err(EAGAIN)` if nothing is available yet.
    fn read(&self, _buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EINVAL))
    }

    /// Write from `buf` without blocking: `Ok(n)` with `n` possibly short,
    /// `Err(EAGAIN)` if nothing could be written yet.
    fn write(&self, _buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EINVAL))
    }

    /// Current readiness; with `hook`, also register it for later changes
    /// (see the module docs). Non-pollable files (regular files) return
    /// `IN|OUT` (always ready) and say so in [`File::pollable`].
    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags;

    /// Can this file be added to an epoll set (`EPERM` otherwise)?
    fn pollable(&self) -> bool {
        true
    }

    /// File-specific `ioctl` (generic ones — `FIONBIO`, `FIOCLEX`,
    /// `FIONCLEX` — are handled by the fd layer). `arg` is the raw
    /// argument; implementations copy through `syscall::with_space`.
    fn ioctl(&self, _cmd: u32, _arg: u64) -> Result<isize, Errno> {
        Err(Errno(ENOTTY))
    }

    /// `fstat`.
    fn stat(&self) -> Result<Stat, Errno>;

    /// Positional read (`pread64`): `ESPIPE` on unseekable files.
    fn pread(&self, _buf: &mut [u8], _off: u64) -> Result<usize, Errno> {
        Err(Errno(ESPIPE))
    }

    /// Positional write (`pwrite64`).
    fn pwrite(&self, _buf: &[u8], _off: u64) -> Result<usize, Errno> {
        Err(Errno(ESPIPE))
    }

    /// `lseek(off, whence)` → new offset.
    fn lseek(&self, _off: i64, _whence: i32) -> Result<u64, Errno> {
        Err(Errno(ESPIPE))
    }

    /// `getdents64` into `buf` (directories): bytes written, 0 at the end.
    fn getdents64(&self, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(Errno(ENOTDIR))
    }

    /// `ftruncate`: `EINVAL` unless the file is a regular file.
    fn truncate(&self, _len: u64) -> Result<(), Errno> {
        Err(Errno(EINVAL))
    }

    /// The VFS inode behind this description, if it was opened by path
    /// (regular files, directories, `O_PATH` fds, device nodes and FIFOs):
    /// `fchmod`, `fchdir`, dirfd-relative lookups and `/proc/self/fd/N`
    /// act on it.
    fn vnode(&self) -> Option<patina_vfs::Ino> {
        None
    }

    /// An `O_PATH` description: only `fstat`, `fchdir`, `close`, `dup`,
    /// `fcntl` and use as a dirfd work; the rest give `EBADF`.
    fn is_opath(&self) -> bool {
        false
    }

    /// What `readlink("/proc/self/fd/N")` shows: the path it was opened
    /// by, or `pipe:[ino]`, `anon_inode:[eventfd]`, ...
    fn proc_name(&self) -> Vec<u8> {
        let mut v = Vec::from(&b"anon_inode:["[..]);
        v.extend_from_slice(self.kind().as_bytes());
        v.push(b']');
        v
    }

    /// The last reference to the open file description went away (every
    /// fd closed and no syscall in progress on it). Wake peers here.
    fn release(&self) {}

    /// Downcast for `epoll_ctl`/`epoll_wait` (an epoll fd is a `File`).
    fn as_epoll(&self) -> Option<&super::epoll::Epoll> {
        None
    }
}

/// An open file description: the file and its status flags.
pub struct OpenFile {
    pub file: Arc<dyn File>,
    flags: AtomicU32,
}

/// Status flags `F_SETFL` may change (Linux `SETFL_MASK` minus the ones
/// patina does not implement: `O_ASYNC`, `O_DIRECT`, `O_NOATIME` are
/// accepted and ignored).
const SETFL_MASK: u32 = O_NONBLOCK | O_APPEND;

impl OpenFile {
    /// `flags`: access mode plus status flags (`O_CLOEXEC` is an fd flag
    /// and is ignored here).
    pub fn new(file: Arc<dyn File>, flags: u32) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            file,
            flags: AtomicU32::new(flags & (O_ACCMODE | SETFL_MASK)),
        })
    }

    pub fn flags(&self) -> FileFlags {
        FileFlags(self.flags.load(Ordering::Relaxed))
    }

    /// `F_SETFL`: replace the changeable status flags.
    pub fn set_status(&self, new: u32) {
        let _ = self
            .flags
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |f| {
                Some((f & !SETFL_MASK) | (new & SETFL_MASK))
            });
    }

    /// `FIONBIO`.
    pub fn set_nonblocking(&self, on: bool) {
        if on {
            self.flags.fetch_or(O_NONBLOCK, Ordering::Relaxed);
        } else {
            self.flags.fetch_and(!O_NONBLOCK, Ordering::Relaxed);
        }
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        self.file.release();
    }
}

/// Block the calling thread until `file` reports any of `want` (or
/// ERR/HUP), or `deadline` passes. Returns at once if it already does.
/// Wake-ups may be spurious; callers retry their operation.
/// `WakeReason::Interrupted` when a signal is pending (callers return
/// `ERESTARTSYS`: restarted under `SA_RESTART`, else `EINTR`).
pub fn wait_ready(file: &dyn File, want: EpollFlags, deadline: Option<u64>) -> WakeReason {
    let w = PollWaiter::new();
    let hook: Arc<dyn PollHook> = w.clone();
    let ev = file.poll(Some(&hook));
    if ev.intersects(want | EpollFlags::ERR | EpollFlags::HUP) {
        return WakeReason::Woken;
    }
    w.wait(deadline)
}

/// Inode numbers for anonymous objects (pipes, eventfds, epoll).
static NEXT_INO: AtomicU64 = AtomicU64::new(1000);

pub fn alloc_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

/// A `struct stat` for an anonymous object with mode `mode`.
pub fn anon_stat(mode: u32, ino: u64) -> Stat {
    Stat {
        st_ino: ino,
        st_mode: mode,
        st_nlink: 1,
        st_blksize: 4096,
        ..Default::default()
    }
}
