//! Inode metadata and its `struct stat` / `struct statx` encodings.

use patina_abi::fs as abi;
use patina_abi::fs::{Statx, StatxTimestamp};
use patina_abi::time::Timespec;

use crate::data::PAGE_SIZE;
use crate::fs::{FileType, Kind};
use crate::{Fs, Ino, Result};

/// tmpfs reports each directory entry as 20 bytes of `st_size`
/// (`BOGO_DIRENT_SIZE`), plus two for `.` and `..`.
const BOGO_DIRENT_SIZE: u64 = 20;

/// Arch-neutral `stat` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub dev: u64,
    pub ino: Ino,
    /// Type and permission bits.
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u32,
    /// 512-byte blocks.
    pub blocks: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub btime: Timespec,
}

impl Meta {
    pub fn file_type(&self) -> Option<FileType> {
        FileType::from_mode(self.mode)
    }

    /// x86_64 `struct stat`.
    pub fn to_stat_x86_64(&self) -> patina_abi::x86_64::Stat {
        patina_abi::x86_64::Stat {
            st_dev: self.dev,
            st_ino: self.ino,
            st_nlink: self.nlink as u64,
            st_mode: self.mode,
            st_uid: self.uid,
            st_gid: self.gid,
            st_rdev: self.rdev,
            st_size: self.size as i64,
            st_blksize: self.blksize as i64,
            st_blocks: self.blocks as i64,
            st_atime: self.atime.tv_sec,
            st_atime_nsec: self.atime.tv_nsec as u64,
            st_mtime: self.mtime.tv_sec,
            st_mtime_nsec: self.mtime.tv_nsec as u64,
            st_ctime: self.ctime.tv_sec,
            st_ctime_nsec: self.ctime.tv_nsec as u64,
            ..Default::default()
        }
    }

    /// aarch64 (asm-generic) `struct stat`.
    pub fn to_stat_aarch64(&self) -> patina_abi::aarch64::Stat {
        patina_abi::aarch64::Stat {
            st_dev: self.dev,
            st_ino: self.ino,
            st_mode: self.mode,
            st_nlink: self.nlink,
            st_uid: self.uid,
            st_gid: self.gid,
            st_rdev: self.rdev,
            st_size: self.size as i64,
            st_blksize: self.blksize as i32,
            st_blocks: self.blocks as i64,
            st_atime: self.atime.tv_sec,
            st_atime_nsec: self.atime.tv_nsec as u64,
            st_mtime: self.mtime.tv_sec,
            st_mtime_nsec: self.mtime.tv_nsec as u64,
            st_ctime: self.ctime.tv_sec,
            st_ctime_nsec: self.ctime.tv_nsec as u64,
            ..Default::default()
        }
    }

    /// `struct stat` for the architecture being compiled for.
    #[cfg(target_arch = "x86_64")]
    pub fn to_stat(&self) -> patina_abi::current::Stat {
        self.to_stat_x86_64()
    }

    /// `struct stat` for the architecture being compiled for.
    #[cfg(target_arch = "aarch64")]
    pub fn to_stat(&self) -> patina_abi::current::Stat {
        self.to_stat_aarch64()
    }

    /// `struct statx`. Everything in `STATX_BASIC_STATS | STATX_BTIME` is
    /// always filled in (the kernel may return more than requested).
    pub fn to_statx(&self) -> Statx {
        let ts = |t: Timespec| StatxTimestamp {
            tv_sec: t.tv_sec,
            tv_nsec: t.tv_nsec as u32,
            _reserved: 0,
        };
        Statx {
            stx_mask: abi::STATX_BASIC_STATS | abi::STATX_BTIME,
            stx_blksize: self.blksize,
            stx_nlink: self.nlink,
            stx_uid: self.uid,
            stx_gid: self.gid,
            stx_mode: self.mode as u16,
            stx_ino: self.ino,
            stx_size: self.size,
            stx_blocks: self.blocks,
            stx_atime: ts(self.atime),
            stx_btime: ts(self.btime),
            stx_ctime: ts(self.ctime),
            stx_mtime: ts(self.mtime),
            stx_rdev_major: abi::dev_major(self.rdev),
            stx_rdev_minor: abi::dev_minor(self.rdev),
            stx_dev_major: abi::dev_major(self.dev),
            stx_dev_minor: abi::dev_minor(self.dev),
            ..Default::default()
        }
    }
}

impl Fs {
    /// Metadata of `ino` (`fstat`; combine with [`Fs::resolve`] for
    /// `newfstatat`/`statx`).
    pub fn stat(&self, ino: Ino) -> Result<Meta> {
        let n = self.node(ino)?;
        let (size, blocks) = match &n.kind {
            Kind::File(d) => (d.size(), d.resident_pages() * (PAGE_SIZE as u64 / 512)),
            Kind::Dir(d) => ((d.len() as u64 + 2) * BOGO_DIRENT_SIZE, 0),
            Kind::Symlink(t) => (t.len() as u64, 0),
            Kind::Special(s) => (s.as_ref().map_or(0, |s| s.size()), 0),
        };
        Ok(Meta {
            dev: self.dev,
            ino,
            mode: n.mode,
            nlink: n.nlink,
            uid: n.uid,
            gid: n.gid,
            rdev: n.rdev,
            size,
            blksize: PAGE_SIZE as u32,
            blocks,
            atime: n.atime,
            mtime: n.mtime,
            ctime: n.ctime,
            btime: n.btime,
        })
    }
}
