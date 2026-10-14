//! Unpack a cpio (initramfs) archive into an [`Fs`], following
//! `init/initramfs.c`:
//!
//! - an existing node whose type differs from the entry's is removed first
//!   (`clean_path`; a non-empty directory stays and the entry then fails);
//! - regular files are created or truncated, then written; directories
//!   that already exist are kept and get the entry's owner and mode;
//!   symlinks replace whatever is there;
//! - hard links (`nlink >= 2`) are grouped by `(dev, ino, mode)`: later
//!   members are linked to the first, and data (stored with the last member
//!   in newc) is written through whichever member carries it;
//! - mode, owner and mtime (as atime and mtime) come from the header;
//!   directory mtimes are applied after the whole archive, since adding
//!   children changes them;
//! - per-entry failures are counted and skipped, as the kernel ignores
//!   them; a malformed archive stops unpacking with an error.
//!
//! One deliberate difference: with [`PopulateOptions::create_parents`]
//! (the default) missing parent directories are created (`0755`, owner 0)
//! instead of the entry being skipped.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use patina_abi::fs as abi;
use patina_abi::time::Timespec;
use patina_cpio::{Entry, FileType as CpioType, Reader};

use crate::data::FileData;
use crate::fs::Kind;
use crate::ops::{OpenOptions, SetTime};
use crate::{e, Errno, Fs, Ino, Result, ROOT_INO};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PopulateOptions {
    /// Create missing parent directories instead of skipping the entry.
    pub create_parents: bool,
}

impl Default for PopulateOptions {
    fn default() -> Self {
        PopulateOptions {
            create_parents: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PopulateReport {
    /// Archive members processed (the trailer excluded).
    pub entries: usize,
    /// Members that could not be applied.
    pub skipped: usize,
    /// The first failure: archive offset of the member and the error.
    pub first_error: Option<(usize, Errno)>,
}

/// Populate `fs` from `reader`, copying file data onto the heap.
pub fn populate_from_cpio(
    fs: &mut Fs,
    reader: Reader<'_>,
    opts: &PopulateOptions,
) -> core::result::Result<PopulateReport, patina_cpio::Error> {
    populate(fs, reader, opts, |fs, ino, data| {
        fs.truncate(ino, 0)?;
        fs.write_at(ino, 0, data).map(|_| ())
    })
}

/// Populate `fs` from an archive that lives forever (the initrd), without
/// copying: file pages are borrowed from the archive and copied only when
/// first written.
pub fn populate_from_cpio_static(
    fs: &mut Fs,
    reader: Reader<'static>,
    opts: &PopulateOptions,
) -> core::result::Result<PopulateReport, patina_cpio::Error> {
    populate(fs, reader, opts, |fs, ino, data| {
        fs.truncate(ino, 0)?;
        let n = fs.node_mut(ino)?;
        match &mut n.kind {
            Kind::File(d) => *d = FileData::from_static(data),
            _ => return Err(e::EINVAL),
        }
        Ok(())
    })
}

fn path_of(e: &Entry<'_>) -> Vec<u8> {
    let mut p = e.path().to_vec();
    while p.last() == Some(&b'/') {
        p.pop();
    }
    p
}

/// Remove whatever is at `path` if its type differs from `fmode`'s (0
/// removes anything). Errors are ignored, as in the kernel.
fn clean_path(fs: &mut Fs, path: &[u8], fmode: u32) {
    if let Ok(ino) = fs.resolve(ROOT_INO, path, false) {
        let mode = fs.node(ino).map_or(0, |n| n.mode);
        if (mode ^ fmode) & abi::S_IFMT != 0 {
            let _ = if mode & abi::S_IFMT == abi::S_IFDIR {
                fs.rmdir(ROOT_INO, path)
            } else {
                fs.unlink(ROOT_INO, path)
            };
        }
    }
}

fn populate<'a>(
    fs: &mut Fs,
    reader: Reader<'a>,
    opts: &PopulateOptions,
    set_data: impl Fn(&mut Fs, Ino, &'a [u8]) -> Result<()>,
) -> core::result::Result<PopulateReport, patina_cpio::Error> {
    let mut report = PopulateReport::default();
    let mut links: BTreeMap<(u32, u32, u32, u32), Ino> = BTreeMap::new();
    let mut dir_times: Vec<(Ino, Timespec)> = Vec::new();
    let mut result = Ok(());
    for item in reader {
        let ent = match item {
            Ok(e) => e,
            Err(err) => {
                result = Err(err);
                break;
            }
        };
        report.entries += 1;
        if let Err(err) = apply(fs, &ent, opts, &mut links, &mut dir_times, &set_data) {
            report.skipped += 1;
            if report.first_error.is_none() {
                report.first_error = Some((ent.offset, err));
            }
        }
    }
    for (ino, t) in dir_times {
        let _ = fs.utimens(ino, SetTime::At(t), SetTime::At(t));
    }
    result.map(|_| report)
}

fn apply<'a>(
    fs: &mut Fs,
    ent: &Entry<'a>,
    opts: &PopulateOptions,
    links: &mut BTreeMap<(u32, u32, u32, u32), Ino>,
    dir_times: &mut Vec<(Ino, Timespec)>,
    set_data: &impl Fn(&mut Fs, Ino, &'a [u8]) -> Result<()>,
) -> Result<()> {
    let path = path_of(ent);
    let mtime = Timespec {
        tv_sec: ent.mtime as i64,
        tv_nsec: 0,
    };
    let perm = ent.perm();
    let set_meta = |fs: &mut Fs, ino: Ino, chmod: bool| -> Result<()> {
        fs.chown(ino, Some(ent.uid), Some(ent.gid))?;
        if chmod {
            fs.chmod(ino, perm)?;
        }
        Ok(())
    };

    if path.is_empty() {
        // "." : the root itself.
        if ent.file_type() == CpioType::Directory {
            set_meta(fs, ROOT_INO, true)?;
            dir_times.push((ROOT_INO, mtime));
        }
        return Ok(());
    }
    if opts.create_parents {
        if let Some(i) = path.iter().rposition(|&b| b == b'/') {
            if i > 0 {
                let _ = fs.mkdir_all(ROOT_INO, &path[..i], 0o755);
            }
        }
    }
    let key = (ent.dev_major, ent.dev_minor, ent.ino, ent.mode);
    // `maybe_link`: link to an earlier member of the same hard-link group.
    let maybe_link = |fs: &mut Fs| -> Result<Option<Ino>> {
        if ent.nlink < 2 {
            return Ok(None);
        }
        match links.get(&key) {
            Some(&old) if fs.node(old).is_ok_and(|n| n.nlink > 0) => {
                clean_path(fs, &path, 0);
                fs.link_ino(old, ROOT_INO, &path)?;
                Ok(Some(old))
            }
            _ => Ok(None),
        }
    };

    match ent.file_type() {
        CpioType::Regular => {
            clean_path(fs, &path, abi::S_IFREG);
            let ino = match maybe_link(fs)? {
                Some(old) => {
                    if !ent.data.is_empty() {
                        set_data(fs, old, ent.data)?;
                    }
                    old
                }
                None => {
                    let o = fs.open(
                        ROOT_INO,
                        &path,
                        &OpenOptions {
                            write: true,
                            create: true,
                            trunc: true,
                            mode: perm,
                            ..Default::default()
                        },
                    )?;
                    fs.put_ref(o.ino);
                    if ent.nlink >= 2 {
                        links.insert(key, o.ino);
                    }
                    set_data(fs, o.ino, ent.data)?;
                    o.ino
                }
            };
            set_meta(fs, ino, true)?;
            fs.utimens(ino, SetTime::At(mtime), SetTime::At(mtime))
        }
        CpioType::Directory => {
            clean_path(fs, &path, abi::S_IFDIR);
            let ino = match fs.mkdir(ROOT_INO, &path, perm) {
                Ok(i) => i,
                Err(x) if x == e::EEXIST => fs.resolve(ROOT_INO, &path, true)?,
                Err(x) => return Err(x),
            };
            set_meta(fs, ino, true)?;
            dir_times.push((ino, mtime));
            Ok(())
        }
        CpioType::Symlink => {
            clean_path(fs, &path, 0);
            let ino = fs.symlink(ent.data, ROOT_INO, &path)?;
            set_meta(fs, ino, false)?;
            fs.utimens(ino, SetTime::At(mtime), SetTime::At(mtime))
        }
        CpioType::CharDevice | CpioType::BlockDevice | CpioType::Fifo | CpioType::Socket => {
            clean_path(fs, &path, ent.mode);
            if maybe_link(fs)?.is_some() {
                return Ok(());
            }
            let rdev = abi::makedev(ent.rdev_major, ent.rdev_minor);
            let ino = fs.mknod(ROOT_INO, &path, ent.mode, rdev, None)?;
            if ent.nlink >= 2 {
                links.insert(key, ino);
            }
            set_meta(fs, ino, true)?;
            fs.utimens(ino, SetTime::At(mtime), SetTime::At(mtime))
        }
        CpioType::Unknown(_) => Err(e::EINVAL),
    }
}
