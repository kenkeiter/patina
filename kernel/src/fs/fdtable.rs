//! The per-process file-descriptor table (design §8), shared by all
//! threads: fd → ([`OpenFile`], `FD_CLOEXEC`). Allocation is
//! lowest-free-first (`patina_fsutil::FdSlots`), bounded by
//! `RLIMIT_NOFILE`'s soft limit.
//!
//! The table lives in a `SpinLock` in `task::Process`. Callers clone the
//! `Arc<OpenFile>` out and release the lock before operating on the file;
//! entries removed by `close`/`dup2` are returned so the caller drops them
//! (possibly running `File::release`) after unlocking.

use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::errno::{EBADF, EMFILE, Errno};
use patina_abi::fs::{O_RDONLY, O_RDWR};
use patina_fsutil::FdSlots;

use super::console::Console;
use super::file::OpenFile;

/// One fd.
#[derive(Clone)]
pub struct FdEntry {
    pub file: Arc<OpenFile>,
    pub cloexec: bool,
}

pub struct FdTable {
    slots: FdSlots<FdEntry>,
}

/// Fd numbers are `i32` in the ABI; negative ones are never valid.
fn idx(fd: i32) -> Result<usize, Errno> {
    usize::try_from(fd).map_err(|_| Errno(EBADF))
}

impl FdTable {
    pub const fn new() -> FdTable {
        FdTable {
            slots: FdSlots::new(),
        }
    }

    /// The initial table: 0 = console read-only (at EOF), 1 and 2 = one
    /// shared read-write console description (like a shell's tty dups).
    pub fn with_console() -> FdTable {
        let mut t = FdTable::new();
        let con: Arc<Console> = Arc::new(Console);
        let out = OpenFile::new(con.clone(), O_RDWR);
        t.slots.install(
            0,
            FdEntry {
                file: OpenFile::new(con, O_RDONLY),
                cloexec: false,
            },
        );
        t.slots.install(
            1,
            FdEntry {
                file: out.clone(),
                cloexec: false,
            },
        );
        t.slots.install(
            2,
            FdEntry {
                file: out,
                cloexec: false,
            },
        );
        t
    }

    /// The open file at `fd` (`EBADF`).
    pub fn get(&self, fd: i32) -> Result<Arc<OpenFile>, Errno> {
        Ok(self.entry(fd)?.file.clone())
    }

    pub fn entry(&self, fd: i32) -> Result<&FdEntry, Errno> {
        self.slots.get(idx(fd)?).ok_or(Errno(EBADF))
    }

    pub fn entry_mut(&mut self, fd: i32) -> Result<&mut FdEntry, Errno> {
        self.slots.get_mut(idx(fd)?).ok_or(Errno(EBADF))
    }

    /// Install `file` at the lowest free fd `>= min` (`EMFILE` if none
    /// below `limit`).
    pub fn alloc(
        &mut self,
        file: Arc<OpenFile>,
        cloexec: bool,
        min: usize,
        limit: usize,
    ) -> Result<i32, Errno> {
        self.slots
            .alloc(FdEntry { file, cloexec }, min, limit)
            .map(|fd| fd as i32)
            .map_err(|_| Errno(EMFILE))
    }

    /// Install at exactly `fd` (`dup2`/`dup3`; the caller checked `fd`
    /// against the limit). Returns the entry that was there.
    pub fn install(&mut self, fd: usize, file: Arc<OpenFile>, cloexec: bool) -> Option<FdEntry> {
        self.slots.install(fd, FdEntry { file, cloexec })
    }

    /// Remove `fd` (`EBADF` if not open). Drop the result after unlocking.
    pub fn close(&mut self, fd: i32) -> Result<FdEntry, Errno> {
        self.slots.take(idx(fd)?).ok_or(Errno(EBADF))
    }

    /// Remove every close-on-exec fd (for a future `execve`).
    pub fn take_cloexec(&mut self) -> Vec<FdEntry> {
        self.slots.take_matching(|e| e.cloexec)
    }

    /// Number of open fds.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}
