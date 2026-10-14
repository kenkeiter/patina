//! Regular-file contents: sparse 4 KiB pages over an optional read-only
//! `'static` backing slice (copy-on-write, used for initrd files so they are
//! not copied at boot).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;

/// Page size used for file storage and `st_blocks` accounting.
pub const PAGE_SIZE: usize = 4096;
const PAGE: u64 = PAGE_SIZE as u64;

#[derive(Default)]
pub(crate) struct FileData {
    size: u64,
    /// Materialised pages, by page index. Absent pages read from `backing`
    /// (while inside it) or as zeros (holes).
    pages: BTreeMap<u64, Box<[u8]>>,
    /// Read-only initial contents. Only bytes `< backing.len()` are valid;
    /// shrinking the file shortens it so later growth reads zeros.
    backing: &'static [u8],
}

impl core::fmt::Debug for FileData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FileData")
            .field("size", &self.size)
            .field("pages", &self.pages.len())
            .field("backing", &self.backing.len())
            .finish()
    }
}

impl FileData {
    pub fn from_static(data: &'static [u8]) -> Self {
        FileData {
            size: data.len() as u64,
            pages: BTreeMap::new(),
            backing: data,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// The whole contents as one borrowed slice, if they are still exactly
    /// the `'static` backing (nothing written since population).
    pub fn as_static(&self) -> Option<&'static [u8]> {
        (self.pages.is_empty() && self.backing.len() as u64 == self.size).then_some(self.backing)
    }

    /// Number of materialised (heap-owned) pages.
    pub fn owned_pages(&self) -> u64 {
        self.pages.len() as u64
    }

    /// Pages that hold data (materialised or backed), for `st_blocks`.
    pub fn resident_pages(&self) -> u64 {
        let backed = (self.backing.len() as u64).div_ceil(PAGE);
        let overlap = self.pages.range(..backed).count() as u64;
        self.pages.len() as u64 + backed - overlap
    }

    /// Copy up to `buf.len()` bytes from `off`; returns bytes read (0 at or
    /// past EOF).
    pub fn read(&self, off: u64, buf: &mut [u8]) -> usize {
        if off >= self.size {
            return 0;
        }
        let n = core::cmp::min(buf.len() as u64, self.size - off) as usize;
        let mut done = 0;
        while done < n {
            let pos = off + done as u64;
            let idx = pos / PAGE;
            let in_page = (pos % PAGE) as usize;
            let chunk = core::cmp::min(PAGE_SIZE - in_page, n - done);
            let dst = &mut buf[done..done + chunk];
            if let Some(p) = self.pages.get(&idx) {
                dst.copy_from_slice(&p[in_page..in_page + chunk]);
            } else {
                // Backed bytes first, zeros after.
                let b = self.backing.len() as u64;
                let avail = if pos < b {
                    core::cmp::min((b - pos) as usize, chunk)
                } else {
                    0
                };
                if avail > 0 {
                    dst[..avail].copy_from_slice(&self.backing[pos as usize..pos as usize + avail]);
                }
                dst[avail..].fill(0);
            }
            done += chunk;
        }
        n
    }

    /// Pages that a write of `len` bytes at `off` would newly materialise.
    pub fn pages_needed(&self, off: u64, len: usize) -> u64 {
        if len == 0 {
            return 0;
        }
        let first = off / PAGE;
        let last = (off + len as u64 - 1) / PAGE;
        let present = self.pages.range(first..=last).count() as u64;
        (last - first + 1) - present
    }

    fn page_mut(&mut self, idx: u64) -> &mut Box<[u8]> {
        let backing = self.backing;
        self.pages.entry(idx).or_insert_with(|| {
            let mut p = vec![0u8; PAGE_SIZE].into_boxed_slice();
            let start = idx * PAGE;
            let b = backing.len() as u64;
            if start < b {
                let n = core::cmp::min(PAGE, b - start) as usize;
                p[..n].copy_from_slice(&backing[start as usize..start as usize + n]);
            }
            p
        })
    }

    /// Write all of `data` at `off`, extending the file. The caller has
    /// checked `off + data.len()` for overflow and capacity.
    pub fn write(&mut self, off: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let pos = off + done as u64;
            let idx = pos / PAGE;
            let in_page = (pos % PAGE) as usize;
            let chunk = core::cmp::min(PAGE_SIZE - in_page, data.len() - done);
            let p = self.page_mut(idx);
            p[in_page..in_page + chunk].copy_from_slice(&data[done..done + chunk]);
            done += chunk;
        }
        let end = off + data.len() as u64;
        if end > self.size {
            self.size = end;
        }
    }

    /// Set the size. Shrinking drops whole pages past the end and zeroes the
    /// tail of the last partial page so a later extension reads zeros.
    /// Returns the number of materialised pages freed.
    pub fn truncate(&mut self, new: u64) -> u64 {
        let mut freed = 0;
        if new < self.size {
            let keep = new.div_ceil(PAGE);
            let gone = self.pages.split_off(&keep);
            freed = gone.len() as u64;
            if !new.is_multiple_of(PAGE) {
                if let Some(p) = self.pages.get_mut(&(new / PAGE)) {
                    p[(new % PAGE) as usize..].fill(0);
                }
            }
            if (self.backing.len() as u64) > new {
                self.backing = &self.backing[..new as usize];
            }
        }
        self.size = new;
        freed
    }
}
