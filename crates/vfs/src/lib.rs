//! In-memory filesystem core for patina (tmpfs semantics).
//!
//! [`Fs`] is a single inode table holding regular files, directories,
//! symlinks and *special* nodes, with Linux path resolution and the
//! namespace operations behind `openat`, `mkdirat`, `unlinkat`,
//! `renameat2`, `linkat`, `symlinkat`, `readlinkat`, `getdents64`,
//! `newfstatat`/`statx`, `ftruncate`, `utimensat`, `fchmod`/`fchown` and
//! `getcwd`. [`populate_from_cpio`] unpacks an initramfs into it.
//!
//! # Handles and lifetimes
//!
//! Inodes are named by [`Ino`]. Anything that keeps an inode in use outside
//! a single call (an open file description, a cwd, a dirfd) holds a
//! *reference*: [`Fs::get_ref`]/[`Fs::put_ref`], or the RAII [`Handle`].
//! An inode is freed when it has no links and no references, so an open
//! file keeps working after `unlink`, and a removed directory can still be
//! a cwd (lookups in it give `ENOENT`, `getcwd` gives `ENOENT`). Calls that
//! take a starting directory (`start`) expect the caller to hold a
//! reference on it.
//!
//! # Locking
//!
//! `Fs` is not internally synchronised: every method takes `&self` or
//! `&mut self`, and the kernel wraps the one instance in a single coarse
//! lock (its IRQ-safe `SpinLock<Fs>`). Each syscall does its whole
//! resolve-and-modify sequence inside one critical section, which is what
//! makes multi-step operations (`O_CREAT|O_EXCL`, `rename`, `O_APPEND`
//! size-then-write) atomic. [`FsRef`] abstracts "something that can lock the
//! `Fs`" so [`Handle`] can release its reference on drop; the kernel
//! implements it for its global. A `Handle` must not be dropped while the
//! lock is held (that would self-deadlock), and nothing in this crate drops
//! one. File data is read and written under the same lock; that is a
//! deliberate simplification for an in-memory filesystem whose operations
//! are memcpy-bound.
//!
//! # Special nodes
//!
//! Character/block devices, FIFOs, sockets and generated (procfs-style)
//! files are inodes whose behaviour the kernel supplies: they carry an
//! optional `Arc<dyn` [`Special`]`>` that the kernel downcasts (via
//! [`Special::as_any`]) when the node is opened. This crate only stores,
//! links, renames and stats them. Device nodes from the initrd are created
//! without a handle; the kernel can attach one later with
//! [`Fs::set_special`] (e.g. keyed by `rdev`).
//!
//! # Simplifications
//!
//! Single user: permission bits are stored, reported and changed, but not
//! enforced (as for root), except `X_OK` in [`Fs::access`]. The caller
//! applies the umask to creation modes.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

extern crate alloc;

mod data;
mod dir;
mod fs;
mod handle;
mod ops;
mod path;
mod populate;
mod stat;

#[cfg(test)]
mod tests;

pub use data::PAGE_SIZE;
pub use fs::{Clock, FileType, Fs, Special};
pub use handle::{FsRef, Handle};
pub use ops::{OpenOptions, Opened, SetTime};
pub use patina_abi::errno::Errno;
pub use patina_abi::time::Timespec;
pub use populate::{
    populate_from_cpio, populate_from_cpio_static, PopulateOptions, PopulateReport,
};
pub use stat::Meta;

/// Inode number (also `st_ino`).
pub type Ino = u64;

/// Inode number of the root directory.
pub const ROOT_INO: Ino = 1;

/// Longest path accepted, including the terminating NUL (`PATH_MAX`).
pub const PATH_MAX: usize = 4096;
/// Longest single component (`NAME_MAX`).
pub const NAME_MAX: usize = 255;
/// Symlinks followed during one resolution before `ELOOP` (`MAXSYMLINKS`).
pub const MAXSYMLINKS: u32 = 40;

/// Result type for filesystem operations.
pub type Result<T> = core::result::Result<T, Errno>;

pub(crate) mod e {
    use patina_abi::errno as n;
    use patina_abi::errno::Errno;
    pub const ENOENT: Errno = Errno(n::ENOENT);
    pub const ENOTDIR: Errno = Errno(n::ENOTDIR);
    pub const EISDIR: Errno = Errno(n::EISDIR);
    pub const EEXIST: Errno = Errno(n::EEXIST);
    pub const ENOTEMPTY: Errno = Errno(n::ENOTEMPTY);
    pub const EINVAL: Errno = Errno(n::EINVAL);
    pub const ELOOP: Errno = Errno(n::ELOOP);
    pub const ENAMETOOLONG: Errno = Errno(n::ENAMETOOLONG);
    pub const EPERM: Errno = Errno(n::EPERM);
    pub const EBUSY: Errno = Errno(n::EBUSY);
    pub const EFBIG: Errno = Errno(n::EFBIG);
    pub const ENOSPC: Errno = Errno(n::ENOSPC);
    pub const EACCES: Errno = Errno(n::EACCES);
    pub const EMLINK: Errno = Errno(n::EMLINK);
}
