//! Reader for SVR4 cpio archives — `newc` (`070701`) and `crc`
//! (`070702`) — over a byte slice, as used for Linux initramfs.
//!
//! ```text
//! header (110 bytes ASCII): magic[6] ino mode uid gid nlink mtime filesize
//!                           devmajor devminor rdevmajor rdevminor namesize check
//!                           (13 fields x 8 hex digits)
//! name (namesize bytes, NUL-terminated), padded so header+name is 4-aligned
//! data (filesize bytes), padded to 4
//! ... last entry is named "TRAILER!!!"
//! ```
//!
//! Notes for consumers:
//!
//! - **Hard links:** in newc, all links share `(dev, ino)` and the file
//!   data is stored only with the *last* link; earlier links have
//!   `filesize == 0`. Group by [`Entry::dev`]/[`Entry::ino`] when
//!   `nlink > 1`.
//! - **Names** are as stored (typically `./app` or `app`); see
//!   [`Entry::path`] for a normalised form.
//! - [`Reader::initramfs`] accepts several concatenated archives separated
//!   by NUL padding, like the Linux initramfs unpacker; [`Reader::new`]
//!   stops at the first trailer.
//!
//! No allocation, no `unsafe`, and no panics on malformed input: the
//! iterator yields one `Err` and then stops.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

/// Header size in bytes.
pub const HEADER_LEN: usize = 110;
pub const MAGIC_NEWC: &[u8; 6] = b"070701";
pub const MAGIC_CRC: &[u8; 6] = b"070702";
pub const TRAILER: &[u8] = b"TRAILER!!!";

/// File type and permission bits in `mode`.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFIFO: u32 = 0o010000;

/// Archive format of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `070701`: no checksum.
    Newc,
    /// `070702`: `check` is the 32-bit sum of the data bytes.
    Crc,
}

/// Entry type decoded from `mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
    /// Unrecognised `S_IFMT` bits.
    Unknown(u32),
}

impl FileType {
    pub fn from_mode(mode: u32) -> Self {
        match mode & S_IFMT {
            S_IFREG => FileType::Regular,
            S_IFDIR => FileType::Directory,
            S_IFLNK => FileType::Symlink,
            S_IFCHR => FileType::CharDevice,
            S_IFBLK => FileType::BlockDevice,
            S_IFIFO => FileType::Fifo,
            S_IFSOCK => FileType::Socket,
            other => FileType::Unknown(other),
        }
    }
}

/// One archive member, borrowing name and data from the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    pub format: Format,
    /// Name as stored, without the terminating NUL.
    pub name: &'a [u8],
    pub ino: u32,
    /// Type and permission bits (`st_mode`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    /// Seconds since the Unix epoch.
    pub mtime: u32,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub rdev_major: u32,
    pub rdev_minor: u32,
    /// `check` field (data checksum for [`Format::Crc`], else usually 0).
    pub check: u32,
    /// File contents (or symlink target for symlinks).
    pub data: &'a [u8],
    /// Byte offset of this entry's header in the input.
    pub offset: usize,
}

impl<'a> Entry<'a> {
    pub fn file_type(&self) -> FileType {
        FileType::from_mode(self.mode)
    }

    /// Permission bits including setuid/setgid/sticky (`mode & 0o7777`).
    pub fn perm(&self) -> u32 {
        self.mode & 0o7777
    }

    /// Name as UTF-8, if it is.
    pub fn name_str(&self) -> Option<&'a str> {
        core::str::from_utf8(self.name).ok()
    }

    /// Name with leading `/` and `./` components removed (`"."` and `"./"`
    /// become `""`, the root). Interior components are untouched.
    pub fn path(&self) -> &'a [u8] {
        let mut p = self.name;
        loop {
            if let Some(r) = p.strip_prefix(b"./") {
                p = r;
            } else if let Some(r) = p.strip_prefix(b"/") {
                p = r;
            } else if p == b"." {
                p = &p[1..];
            } else {
                return p;
            }
        }
    }

    /// Symlink target (the entry data), for symlinks only.
    pub fn symlink_target(&self) -> Option<&'a [u8]> {
        match self.file_type() {
            FileType::Symlink => Some(self.data),
            _ => None,
        }
    }

    /// `(dev, ino)` identity for hard-link grouping.
    pub fn dev(&self) -> (u32, u32) {
        (self.dev_major, self.dev_minor)
    }

    /// Whether this is the end-of-archive marker.
    pub fn is_trailer(&self) -> bool {
        self.name == TRAILER
    }
}

/// What went wrong, and where (`offset` is a byte offset into the input).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Input ended inside a header, name or data.
    Truncated,
    /// Header does not start with `070701`/`070702` (e.g. a compressed
    /// initramfs, or the old binary/odc formats).
    BadMagic,
    /// A header field is not 8 hex digits.
    BadHex,
    /// `namesize` is 0, or the name is not NUL-terminated / contains NUL.
    BadName,
    /// `crc` format checksum mismatch.
    ChecksumMismatch,
    /// Input ended without a `TRAILER!!!` entry.
    MissingTrailer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub offset: usize,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "cpio: {:?} at offset {}", self.kind, self.offset)
    }
}

const fn align4(n: usize) -> Option<usize> {
    match n.checked_add(3) {
        Some(v) => Some(v & !3),
        None => None,
    }
}

fn hex8(b: &[u8]) -> Option<u32> {
    if b.len() != 8 {
        return None;
    }
    let mut v = 0u32;
    for &c in b {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u32;
    }
    Some(v)
}

/// Iterator over archive entries (the trailer itself is not yielded).
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    done: bool,
    concatenated: bool,
    verify_crc: bool,
    /// Saw at least one trailer (initramfs mode).
    saw_trailer: bool,
}

impl<'a> Reader<'a> {
    /// Read a single archive; stops at the first `TRAILER!!!`. Bytes after
    /// it are ignored.
    pub fn new(data: &'a [u8]) -> Self {
        Reader {
            data,
            pos: 0,
            done: false,
            concatenated: false,
            verify_crc: true,
            saw_trailer: false,
        }
    }

    /// Read concatenated archives as the Linux initramfs unpacker does:
    /// after each trailer, skip NUL padding and continue if more data
    /// follows. An empty input or one that is all NULs is an empty archive.
    pub fn initramfs(data: &'a [u8]) -> Self {
        Reader {
            concatenated: true,
            ..Self::new(data)
        }
    }

    /// Disable checksum verification for `070702` entries.
    pub fn verify_crc(mut self, on: bool) -> Self {
        self.verify_crc = on;
        self
    }

    /// Byte offset of the next header.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn fail(&mut self, kind: ErrorKind, offset: usize) -> Option<Result<Entry<'a>, Error>> {
        self.done = true;
        Some(Err(Error { kind, offset }))
    }

    fn parse_one(&mut self) -> Result<Entry<'a>, Error> {
        let start = self.pos;
        let err = |kind, offset| Error { kind, offset };
        let hdr = self
            .data
            .get(start..)
            .ok_or(err(ErrorKind::Truncated, start))?;
        if hdr.len() < 6 {
            return Err(err(ErrorKind::Truncated, start));
        }
        let format = match &hdr[..6] {
            m if m == MAGIC_NEWC => Format::Newc,
            m if m == MAGIC_CRC => Format::Crc,
            _ => return Err(err(ErrorKind::BadMagic, start)),
        };
        if hdr.len() < HEADER_LEN {
            return Err(err(ErrorKind::Truncated, start));
        }
        let mut f = [0u32; 13];
        for (i, slot) in f.iter_mut().enumerate() {
            let o = 6 + i * 8;
            *slot = hex8(&hdr[o..o + 8]).ok_or(err(ErrorKind::BadHex, start + o))?;
        }
        let [ino, mode, uid, gid, nlink, mtime, filesize, dev_major, dev_minor, rdev_major, rdev_minor, namesize, check] =
            f;

        let name_start = start + HEADER_LEN;
        let namesize = namesize as usize;
        if namesize == 0 {
            return Err(err(ErrorKind::BadName, name_start));
        }
        let name_end = name_start
            .checked_add(namesize)
            .ok_or(err(ErrorKind::Truncated, name_start))?;
        let name_full = self
            .data
            .get(name_start..name_end)
            .ok_or(err(ErrorKind::Truncated, name_start))?;
        let name = &name_full[..namesize - 1];
        if name_full[namesize - 1] != 0 || name.contains(&0) {
            return Err(err(ErrorKind::BadName, name_start));
        }

        let data_start = align4(name_end).ok_or(err(ErrorKind::Truncated, name_end))?;
        let data_end = data_start
            .checked_add(filesize as usize)
            .ok_or(err(ErrorKind::Truncated, data_start))?;
        let data = self
            .data
            .get(data_start..data_end)
            .ok_or(err(ErrorKind::Truncated, data_start))?;
        // Padding after the data may be missing at the very end of the input.
        let next = align4(data_end).ok_or(err(ErrorKind::Truncated, data_end))?;

        if format == Format::Crc && self.verify_crc && name != TRAILER {
            let sum = data.iter().fold(0u32, |a, &b| a.wrapping_add(b as u32));
            if sum != check {
                return Err(err(ErrorKind::ChecksumMismatch, start));
            }
        }

        self.pos = next.min(self.data.len());
        Ok(Entry {
            format,
            name,
            ino,
            mode,
            uid,
            gid,
            nlink,
            mtime,
            dev_major,
            dev_minor,
            rdev_major,
            rdev_minor,
            check,
            data,
            offset: start,
        })
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = Result<Entry<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if self.concatenated {
                // Skip NUL padding between (and after) archives.
                while self.data.get(self.pos) == Some(&0) {
                    self.pos += 1;
                }
                if self.pos >= self.data.len() {
                    self.done = true;
                    // A dangling archive without trailer is an error unless
                    // there was no archive at all.
                    return if self.saw_trailer || self.data.iter().all(|&b| b == 0) {
                        None
                    } else {
                        self.fail(ErrorKind::MissingTrailer, self.pos)
                    };
                }
                self.saw_trailer = false;
            } else if self.pos >= self.data.len() {
                let p = self.pos;
                return self.fail(ErrorKind::MissingTrailer, p);
            }

            match self.parse_one() {
                Ok(e) if e.is_trailer() => {
                    if self.concatenated {
                        self.saw_trailer = true;
                        continue;
                    }
                    self.done = true;
                    return None;
                }
                Ok(e) => return Some(Ok(e)),
                Err(e) => return self.fail(e.kind, e.offset),
            }
        }
    }
}

/// Build newc/crc archives (used by tests and handy for host tooling).
/// Writes into a caller-provided buffer to stay allocation-free.
#[derive(Debug)]
pub struct Writer<'b> {
    buf: &'b mut [u8],
    len: usize,
    format: Format,
}

/// Header fields for [`Writer::entry`] (name and data are passed
/// separately; sizes and checksum are computed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryMeta {
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub mtime: u32,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub rdev_major: u32,
    pub rdev_minor: u32,
}

impl<'b> Writer<'b> {
    pub fn new(buf: &'b mut [u8], format: Format) -> Self {
        Writer {
            buf,
            len: 0,
            format,
        }
    }

    fn put(&mut self, b: &[u8]) -> Option<()> {
        let end = self.len.checked_add(b.len())?;
        self.buf.get_mut(self.len..end)?.copy_from_slice(b);
        self.len = end;
        Some(())
    }

    fn pad4(&mut self) -> Option<()> {
        while !self.len.is_multiple_of(4) {
            self.put(&[0])?;
        }
        Some(())
    }

    fn hex(&mut self, v: u32) -> Option<()> {
        let mut h = [0u8; 8];
        for (i, c) in h.iter_mut().enumerate() {
            let d = ((v >> (28 - 4 * i)) & 0xf) as u8;
            *c = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        }
        self.put(&h)
    }

    /// Append one entry. Returns `None` if the buffer is too small or the
    /// name/data are too large for the format.
    pub fn entry(&mut self, m: &EntryMeta, name: &[u8], data: &[u8]) -> Option<()> {
        let namesize = u32::try_from(name.len().checked_add(1)?).ok()?;
        let filesize = u32::try_from(data.len()).ok()?;
        let check = match self.format {
            Format::Newc => 0,
            Format::Crc => data.iter().fold(0u32, |a, &b| a.wrapping_add(b as u32)),
        };
        self.put(match self.format {
            Format::Newc => MAGIC_NEWC,
            Format::Crc => MAGIC_CRC,
        })?;
        for v in [
            m.ino,
            m.mode,
            m.uid,
            m.gid,
            m.nlink,
            m.mtime,
            filesize,
            m.dev_major,
            m.dev_minor,
            m.rdev_major,
            m.rdev_minor,
            namesize,
            check,
        ] {
            self.hex(v)?;
        }
        self.put(name)?;
        self.put(&[0])?;
        self.pad4()?;
        self.put(data)?;
        self.pad4()
    }

    /// Append the `TRAILER!!!` entry and return the archive length.
    pub fn finish(mut self) -> Option<usize> {
        self.entry(
            &EntryMeta {
                nlink: 1,
                ..Default::default()
            },
            TRAILER,
            &[],
        )?;
        Some(self.len)
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests;
