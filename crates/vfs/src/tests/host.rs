//! Differential test against the host filesystem (macOS or Linux) for the
//! subset of semantics POSIX hosts agree on: create/`O_EXCL`, mkdir,
//! rmdir, unlink of non-directories, plain rename, link, write, truncate.
//! Paths never carry trailing slashes (macOS differs there) and unlink is
//! never aimed at a directory (macOS says `EPERM`, Linux `EISDIR`). The
//! resulting trees are compared after every step.

use super::*;
use alloc::string::String;
use std::format;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Host errno -> Linux errno for the values these operations produce.
fn host_errno(e: &std::io::Error) -> i32 {
    let raw = e.raw_os_error().expect("os error");
    if cfg!(target_os = "linux") {
        return raw;
    }
    // BSD/macOS numbering.
    match raw {
        66 => ENOTEMPTY,
        62 => ELOOP,
        63 => ENAMETOOLONG,
        n @ (1 | 2 | 17 | 18 | 20 | 21 | 22) => n,
        n => panic!("unmapped host errno {n} ({e})"),
    }
}

fn host<T>(r: std::io::Result<T>) -> core::result::Result<(), i32> {
    r.map(|_| ()).map_err(|e| host_errno(&e))
}

fn ours<T>(r: Result<T>) -> core::result::Result<(), i32> {
    r.map(|_| ()).map_err(|e| e.0)
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
    fn path(&mut self) -> String {
        let depth = 1 + self.below(3);
        let mut s = String::new();
        for _ in 0..depth {
            s.push('/');
            s.push_str(["a", "b", "c"][self.below(3) as usize]);
        }
        s
    }
}

fn compare_tree(fs: &mut Fs, dir: Ino, hdir: &Path, ctx: &str) {
    let mut want: Vec<String> = std::fs::read_dir(hdir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    want.sort();
    let mut got: Vec<(String, Ino)> = fs
        .read_dir(dir)
        .unwrap()
        .into_iter()
        .map(|(n, i, _)| (String::from_utf8(n).unwrap(), i))
        .collect();
    got.sort();
    let names: Vec<String> = got.iter().map(|g| g.0.clone()).collect();
    assert_eq!(names, want, "{ctx}: entries of {}", hdir.display());
    for (name, ino) in got {
        let hp = hdir.join(&name);
        let hm = std::fs::symlink_metadata(&hp).unwrap();
        let m = fs.stat(ino).unwrap();
        if hm.is_dir() {
            assert_eq!(
                m.file_type(),
                Some(FileType::Directory),
                "{ctx}: {}",
                hp.display()
            );
            compare_tree(fs, ino, &hp, ctx);
        } else {
            assert_eq!(
                m.file_type(),
                Some(FileType::Regular),
                "{ctx}: {}",
                hp.display()
            );
            assert_eq!(m.size, hm.len(), "{ctx}: size of {}", hp.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(
                    m.nlink as u64,
                    hm.nlink(),
                    "{ctx}: nlink of {}",
                    hp.display()
                );
            }
            let mut hd = Vec::new();
            std::fs::File::open(&hp)
                .unwrap()
                .read_to_end(&mut hd)
                .unwrap();
            assert_eq!(
                read_all(fs, ino).unwrap(),
                hd,
                "{ctx}: data of {}",
                hp.display()
            );
        }
    }
}

fn run(seed: u64, steps: usize, base: &Path) {
    let root: PathBuf = base.join(format!("s{seed}"));
    std::fs::create_dir_all(&root).unwrap();
    let hp = |p: &str| root.join(&p[1..]);
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut fs = new_fs();
    for step in 0..steps {
        let op = rng.below(9);
        let a = rng.path();
        let mut ctx = format!("seed {seed} step {step} op {op} {a}");
        let mut dst: Option<String> = None;
        let (g, w) = match op {
            0 | 1 => {
                let excl = rng.below(2) == 0;
                let g = fs.open(R, a.as_bytes(), &OpenOptions { excl, ..ow() });
                if let Ok(o) = &g {
                    fs.put_ref(o.ino);
                }
                let w = std::fs::OpenOptions::new()
                    .write(true)
                    .create(!excl)
                    .create_new(excl)
                    .open(hp(&a));
                (ours(g), host(w))
            }
            2 => (
                ours(fs.mkdir(R, a.as_bytes(), 0o755)),
                host(std::fs::create_dir(hp(&a))),
            ),
            3 => (
                ours(fs.rmdir(R, a.as_bytes())),
                host(std::fs::remove_dir(hp(&a))),
            ),
            4 => {
                if std::fs::symlink_metadata(hp(&a)).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                (
                    ours(fs.unlink(R, a.as_bytes())),
                    host(std::fs::remove_file(hp(&a))),
                )
            }
            5 | 6 => {
                let b = rng.path();
                ctx = format!("{ctx} -> {b}");
                dst = Some(b.clone());
                (
                    ours(fs.rename(R, a.as_bytes(), R, b.as_bytes(), 0)),
                    host(std::fs::rename(hp(&a), hp(&b))),
                )
            }
            7 => {
                let b = rng.path();
                ctx = format!("{ctx} -> {b}");
                if std::fs::symlink_metadata(hp(&a)).is_ok_and(|m| m.is_dir()) {
                    continue; // hosts differ on linking directories
                }
                (
                    ours(fs.link(R, a.as_bytes(), false, R, b.as_bytes())),
                    host(std::fs::hard_link(hp(&a), hp(&b))),
                )
            }
            _ => {
                let off = rng.below(6000);
                let len = rng.below(3000) as usize;
                let data: Vec<u8> = (0..len).map(|i| (i as u64 + seed) as u8).collect();
                let trunc = rng.below(4) == 0;
                let g = fs.resolve(R, a.as_bytes(), true).and_then(|ino| {
                    if trunc {
                        fs.truncate(ino, off)
                    } else {
                        fs.write_at(ino, off, &data).map(|_| ())
                    }
                });
                let w = (|| -> std::io::Result<()> {
                    if std::fs::metadata(hp(&a))?.is_dir() {
                        return Err(std::io::Error::from_raw_os_error(21));
                    }
                    let mut f = std::fs::OpenOptions::new().write(true).open(hp(&a))?;
                    if trunc {
                        f.set_len(off)?;
                    } else {
                        f.seek(SeekFrom::Start(off))?;
                        f.write_all(&data)?;
                    }
                    Ok(())
                })();
                (ours(g), host(w))
            }
        };
        // Two-path operations: Linux resolves both parents before looking
        // at the source, macOS checks the source first, so a missing source
        // plus a bad destination parent is ENOTDIR here and ENOENT there.
        let two_path = matches!(op, 5..=7);
        let mut tolerated = two_path && g == Err(ENOTDIR) && w == Err(ENOENT);
        // Renaming onto an ancestor of the source: Linux's lock_rename trap
        // gives ENOTEMPTY before any type check; macOS checks types first.
        if let Some(dst) = &dst {
            if matches!(op, 5 | 6) && a.starts_with(&format!("{dst}/")) && g == Err(ENOTEMPTY) {
                tolerated |= matches!(w, Err(EISDIR) | Err(ENOTDIR) | Err(ENOTEMPTY));
            }
            // Likewise moving a directory under itself is EINVAL on Linux
            // even when the destination is an existing non-directory.
            if matches!(op, 5 | 6) && dst.starts_with(&format!("{a}/")) && g == Err(EINVAL) {
                tolerated |= matches!(w, Err(EISDIR) | Err(ENOTDIR) | Err(EINVAL));
            }
        }
        if !tolerated {
            assert_eq!(g, w, "{ctx}: ours vs host");
        }
        compare_tree(&mut fs, R, &root, &ctx);
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn differential_against_host_fs() {
    let base = std::env::temp_dir().join(format!("patina-vfs-host-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    for seed in 1..=100 {
        run(seed, 200, &base);
    }
    std::fs::remove_dir_all(&base).unwrap();
}
