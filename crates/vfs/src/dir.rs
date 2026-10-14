//! Directory entries with stable readdir cookies.
//!
//! Every entry gets a cookie from a per-directory counter that only grows,
//! and readdir walks entries in cookie order. A directory stream position is
//! "the next cookie to emit", so entries removed or added while a reader is
//! part-way through never cause another entry to be skipped or returned
//! twice: removals just vanish, additions get a higher cookie and show up
//! later in the same pass (POSIX leaves it unspecified whether they appear).
//!
//! Positions 0 and 1 are `.` and `..`; real entries use cookies from
//! [`FIRST_COOKIE`]. The `d_off` reported for an entry is its cookie + 1.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::Ino;

pub(crate) const FIRST_COOKIE: u64 = 2;

#[derive(Debug)]
pub(crate) struct DirEntry {
    pub name: Vec<u8>,
    pub ino: Ino,
}

#[derive(Debug)]
pub(crate) struct Dir {
    /// Parent directory (itself for the root). Kept for a removed directory
    /// so `..` from an unlinked cwd still resolves the way Linux does.
    pub parent: Ino,
    /// Name in `parent` (empty for the root). Directories have exactly one
    /// link from a parent, so this is unambiguous; used by `getcwd`.
    pub name: Vec<u8>,
    by_name: BTreeMap<Vec<u8>, u64>,
    by_cookie: BTreeMap<u64, DirEntry>,
    next_cookie: u64,
}

impl Dir {
    pub fn new(parent: Ino, name: &[u8]) -> Self {
        Dir {
            parent,
            name: name.to_vec(),
            by_name: BTreeMap::new(),
            by_cookie: BTreeMap::new(),
            next_cookie: FIRST_COOKIE,
        }
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn get(&self, name: &[u8]) -> Option<Ino> {
        let c = self.by_name.get(name)?;
        Some(self.by_cookie[c].ino)
    }

    /// Insert a new name (caller checked it is absent).
    pub fn insert(&mut self, name: &[u8], ino: Ino) {
        let c = self.next_cookie;
        self.next_cookie += 1;
        let prev = self.by_name.insert(name.to_vec(), c);
        debug_assert!(prev.is_none());
        self.by_cookie.insert(
            c,
            DirEntry {
                name: name.to_vec(),
                ino,
            },
        );
    }

    pub fn remove(&mut self, name: &[u8]) -> Option<Ino> {
        let c = self.by_name.remove(name)?;
        self.by_cookie.remove(&c).map(|e| e.ino)
    }

    /// Point an existing name at a different inode, keeping its cookie.
    pub fn replace(&mut self, name: &[u8], ino: Ino) -> Option<Ino> {
        let c = self.by_name.get(name)?;
        let e = self.by_cookie.get_mut(c)?;
        Some(core::mem::replace(&mut e.ino, ino))
    }

    /// Entries with cookie `>= pos`, in cookie order.
    pub fn starting_at(&self, pos: u64) -> impl Iterator<Item = (u64, &DirEntry)> {
        self.by_cookie
            .range(core::cmp::max(pos, FIRST_COOKIE)..)
            .map(|(c, e)| (*c, e))
    }

    pub fn entries(&self) -> impl Iterator<Item = &DirEntry> {
        self.by_cookie.values()
    }

    pub fn clear(&mut self) {
        self.by_name.clear();
        self.by_cookie.clear();
    }
}
