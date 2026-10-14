extern crate std;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, Ordering};

use patina_abi::errno;

use crate::*;

mod basic;
mod cpio;
mod dirs;
mod handle;
#[cfg(unix)]
mod host;
mod links;
mod model;
mod paths;
mod rename;

/// A clock that advances 1 µs per reading, starting at 1_000_000 s, so
/// every update is observable.
pub(crate) fn ticking_clock() -> (Box<dyn Clock>, Arc<AtomicI64>) {
    let t = Arc::new(AtomicI64::new(1_000_000_000_000_000));
    let t2 = t.clone();
    let c = move || {
        let ns = t2.fetch_add(1_000, Ordering::Relaxed);
        Timespec {
            tv_sec: ns / 1_000_000_000,
            tv_nsec: ns % 1_000_000_000,
        }
    };
    (Box::new(c), t)
}

pub(crate) fn new_fs() -> Fs {
    Fs::new(ticking_clock().0)
}

pub(crate) const R: Ino = ROOT_INO;

pub(crate) fn err(n: i32) -> Errno {
    Errno(n)
}

pub(crate) fn ow() -> OpenOptions {
    OpenOptions {
        write: true,
        create: true,
        mode: 0o644,
        ..Default::default()
    }
}

pub(crate) fn rd() -> OpenOptions {
    OpenOptions::default()
}

/// Create (or truncate) `path` with `data`.
pub(crate) fn put(fs: &mut Fs, path: &str, data: &[u8]) -> Ino {
    let o = fs
        .open(
            R,
            path.as_bytes(),
            &OpenOptions {
                trunc: true,
                ..ow()
            },
        )
        .unwrap();
    fs.write_at(o.ino, 0, data).unwrap();
    fs.put_ref(o.ino);
    o.ino
}

/// Whole contents of the file at `path`.
pub(crate) fn get(fs: &mut Fs, path: &str) -> Result<Vec<u8>> {
    let ino = fs.resolve(R, path.as_bytes(), true)?;
    read_all(fs, ino)
}

pub(crate) fn read_all(fs: &mut Fs, ino: Ino) -> Result<Vec<u8>> {
    let size = fs.size(ino)? as usize;
    let mut v = alloc::vec![0u8; size + 10];
    let n = fs.read_at(ino, 0, &mut v)?;
    v.truncate(n);
    Ok(v)
}

/// Names from a full getdents64 pass with a buffer of `bufsize`.
pub(crate) fn list(fs: &mut Fs, ino: Ino, bufsize: usize) -> Result<Vec<(Vec<u8>, u64, u8)>> {
    let mut pos = 0;
    let mut out = Vec::new();
    let mut buf = alloc::vec![0u8; bufsize];
    loop {
        let n = fs.getdents64(ino, &mut pos, &mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend(
            parse_dirents(&buf[..n])
                .into_iter()
                .map(|d| (d.name, d.ino, d.dtype)),
        );
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Dent {
    pub ino: u64,
    pub off: i64,
    pub dtype: u8,
    pub name: Vec<u8>,
}

pub(crate) fn parse_dirents(mut b: &[u8]) -> Vec<Dent> {
    let mut v = Vec::new();
    while !b.is_empty() {
        let ino = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let off = i64::from_le_bytes(b[8..16].try_into().unwrap());
        let reclen = u16::from_le_bytes(b[16..18].try_into().unwrap()) as usize;
        assert_eq!(reclen % 8, 0);
        let dtype = b[18];
        let name_area = &b[19..reclen];
        let nul = name_area.iter().position(|&c| c == 0).unwrap();
        v.push(Dent {
            ino,
            off,
            dtype,
            name: name_area[..nul].to_vec(),
        });
        b = &b[reclen..];
    }
    v
}

pub(crate) fn names(fs: &mut Fs, path: &str) -> Vec<std::string::String> {
    let ino = fs.resolve(R, path.as_bytes(), true).unwrap();
    let mut v: Vec<_> = fs
        .read_dir(ino)
        .unwrap()
        .into_iter()
        .map(|(n, _, _)| std::string::String::from_utf8(n).unwrap())
        .collect();
    v.sort();
    v
}

#[allow(dead_code)]
pub(crate) fn e_(n: i32) -> Result<()> {
    Err(Errno(n))
}

pub(crate) use errno::*;

/// Orderable view of a timestamp.
pub(crate) fn ts(t: Timespec) -> (i64, i64) {
    (t.tv_sec, t.tv_nsec)
}
