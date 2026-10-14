use super::*;
use patina_abi::fs as abi;

#[test]
fn root_stat() {
    let fs = new_fs();
    let m = fs.stat(R).unwrap();
    assert_eq!(m.ino, ROOT_INO);
    assert_eq!(m.mode, abi::S_IFDIR | 0o755);
    assert_eq!(m.nlink, 2);
    assert_eq!(m.size, 40);
    assert_eq!(m.blksize, 4096);
    assert_eq!(fs.getcwd(R).unwrap(), b"/");
}

#[test]
fn create_write_read() {
    let mut fs = new_fs();
    let o = fs.open(R, b"/f", &ow()).unwrap();
    assert!(o.created);
    assert_eq!(o.ftype, FileType::Regular);
    assert_eq!(fs.ref_count(o.ino), 1);
    assert_eq!(fs.write_at(o.ino, 0, b"hello").unwrap(), 5);
    assert_eq!(fs.write_at(o.ino, 5, b" world").unwrap(), 6);
    let mut b = [0u8; 32];
    assert_eq!(fs.read_at(o.ino, 0, &mut b).unwrap(), 11);
    assert_eq!(&b[..11], b"hello world");
    assert_eq!(fs.read_at(o.ino, 6, &mut b[..3]).unwrap(), 3);
    assert_eq!(&b[..3], b"wor");
    assert_eq!(fs.read_at(o.ino, 11, &mut b).unwrap(), 0);
    assert_eq!(fs.read_at(o.ino, 100, &mut b).unwrap(), 0);
    let m = fs.stat(o.ino).unwrap();
    assert_eq!(m.size, 11);
    assert_eq!(m.mode, abi::S_IFREG | 0o644);
    assert_eq!(m.nlink, 1);
    assert_eq!(m.blocks, 8);
    // Re-open without O_CREAT finds the same inode.
    let o2 = fs.open(R, b"f", &rd()).unwrap();
    assert_eq!(o2.ino, o.ino);
    assert!(!o2.created);
    assert_eq!(fs.ref_count(o.ino), 2);
    fs.put_ref(o.ino);
    fs.put_ref(o.ino);
    assert_eq!(fs.ref_count(o.ino), 0);
    assert_eq!(get(&mut fs, "/f").unwrap(), b"hello world");
}

#[test]
fn open_errors() {
    let mut fs = new_fs();
    put(&mut fs, "/f", b"x");
    fs.mkdir(R, b"/d", 0o755).unwrap();
    let excl = OpenOptions { excl: true, ..ow() };
    assert_eq!(fs.open(R, b"/f", &excl), Err(err(EEXIST)));
    assert_eq!(fs.open(R, b"/d", &excl), Err(err(EEXIST)));
    assert_eq!(fs.open(R, b"/nope", &rd()), Err(err(ENOENT)));
    assert_eq!(fs.open(R, b"", &rd()), Err(err(ENOENT)));
    assert_eq!(fs.open(R, b"/nodir/f", &ow()), Err(err(ENOENT)));
    assert_eq!(fs.open(R, b"/f/x", &ow()), Err(err(ENOTDIR)));
    assert_eq!(fs.open(R, b"/f/", &rd()), Err(err(ENOTDIR)));
    // O_CREAT on a directory or with a trailing slash.
    assert_eq!(fs.open(R, b"/d", &ow()), Err(err(EISDIR)));
    assert_eq!(fs.open(R, b"/d/", &ow()), Err(err(EISDIR)));
    assert_eq!(fs.open(R, b"/new/", &ow()), Err(err(EISDIR)));
    assert_eq!(fs.open(R, b"/", &ow()), Err(err(EISDIR)));
    assert_eq!(fs.open(R, b".", &ow()), Err(err(EISDIR)));
    // Writing a directory.
    let w = OpenOptions {
        write: true,
        ..rd()
    };
    assert_eq!(fs.open(R, b"/d", &w), Err(err(EISDIR)));
    let t = OpenOptions {
        trunc: true,
        ..rd()
    };
    assert_eq!(fs.open(R, b"/d", &t), Err(err(EISDIR)));
    // O_DIRECTORY.
    let dir = OpenOptions {
        directory: true,
        ..rd()
    };
    assert_eq!(fs.open(R, b"/f", &dir), Err(err(ENOTDIR)));
    let o = fs.open(R, b"/d", &dir).unwrap();
    assert_eq!(o.ftype, FileType::Directory);
    fs.put_ref(o.ino);
    let o = fs.open(R, b"/d/", &rd()).unwrap();
    fs.put_ref(o.ino);
    assert_eq!(
        fs.open(
            R,
            b"/x",
            &OpenOptions {
                directory: true,
                ..ow()
            }
        ),
        Err(err(EINVAL))
    );
    // Nothing leaked a reference.
    assert_eq!(fs.ref_count(fs.resolve(R, b"/d", true).unwrap()), 0);
}

#[test]
fn trunc_and_excl_create() {
    let mut fs = new_fs();
    put(&mut fs, "/f", b"0123456789");
    let o = fs
        .open(
            R,
            b"/f",
            &OpenOptions {
                trunc: true,
                ..rd()
            },
        )
        .unwrap();
    assert_eq!(
        fs.size(o.ino).unwrap(),
        0,
        "O_RDONLY|O_TRUNC truncates on Linux"
    );
    fs.put_ref(o.ino);
    let o = fs
        .open(R, b"/g", &OpenOptions { excl: true, ..ow() })
        .unwrap();
    assert!(o.created);
    fs.put_ref(o.ino);
}

#[test]
fn sparse_and_truncate() {
    let mut fs = new_fs();
    let ino = put(&mut fs, "/s", b"");
    fs.write_at(ino, 1 << 20, b"end").unwrap();
    let m = fs.stat(ino).unwrap();
    assert_eq!(m.size, (1 << 20) + 3);
    assert_eq!(m.blocks, 8, "only one page materialised");
    assert_eq!(fs.pages_used(), 1);
    let mut b = [1u8; 8];
    fs.read_at(ino, 4096 * 3, &mut b).unwrap();
    assert_eq!(b, [0; 8]);
    // ftruncate up: no pages, zeros.
    fs.truncate(ino, 10 << 20).unwrap();
    assert_eq!(fs.stat(ino).unwrap().size, 10 << 20);
    assert_eq!(fs.pages_used(), 1);
    // Shrink into the middle of a written page, then grow: tail reads 0.
    fs.write_at(ino, 0, &[0xaa; 5000]).unwrap();
    assert_eq!(fs.pages_used(), 3);
    fs.truncate(ino, 100).unwrap();
    assert_eq!(fs.pages_used(), 1);
    fs.truncate(ino, 8192).unwrap();
    let data = read_all(&mut fs, ino).unwrap();
    assert_eq!(data.len(), 8192);
    assert!(data[..100].iter().all(|&c| c == 0xaa));
    assert!(data[100..].iter().all(|&c| c == 0));
    fs.truncate(ino, 0).unwrap();
    assert_eq!(fs.pages_used(), 0);
    assert_eq!(fs.truncate(ino, u64::MAX), Err(err(EINVAL)));
    assert_eq!(fs.write_at(ino, i64::MAX as u64, b"x"), Err(err(EFBIG)));
    assert_eq!(fs.truncate(R, 0), Err(err(EISDIR)));
}

#[test]
fn page_limit_enospc() {
    let mut fs = new_fs();
    fs.set_page_limit(Some(2));
    let ino = put(&mut fs, "/f", &[1; 8192]);
    assert_eq!(fs.write_at(ino, 8192, b"x"), Err(err(ENOSPC)));
    assert_eq!(fs.size(ino).unwrap(), 8192);
    // Overwriting existing pages needs nothing new.
    fs.write_at(ino, 10, b"abc").unwrap();
    fs.unlink(R, b"/f").unwrap();
    assert_eq!(fs.pages_used(), 0);
}

#[test]
fn page_hook_charges_and_refuses() {
    use core::sync::atomic::{AtomicI64, Ordering};
    static HELD: AtomicI64 = AtomicI64::new(0);
    fn hook(d: i64) -> bool {
        if d > 0 && HELD.load(Ordering::SeqCst) + d > 3 {
            return false;
        }
        HELD.fetch_add(d, Ordering::SeqCst);
        true
    }
    let mut fs = new_fs();
    fs.set_page_hook(Some(hook));
    let ino = put(&mut fs, "/f", &[1; 8192]);
    assert_eq!(HELD.load(Ordering::SeqCst), 2);
    // Two more pages would pass the hook's limit of 3: all or nothing.
    assert_eq!(fs.write_at(ino, 8192, &[2; 8192]), Err(err(ENOSPC)));
    assert_eq!(HELD.load(Ordering::SeqCst), 2);
    fs.write_at(ino, 8192, b"x").unwrap();
    assert_eq!(HELD.load(Ordering::SeqCst), 3);
    fs.truncate(ino, 4096).unwrap();
    assert_eq!(HELD.load(Ordering::SeqCst), 1);
    fs.unlink(R, b"/f").unwrap();
    assert_eq!(HELD.load(Ordering::SeqCst), 0);
}

#[test]
fn read_write_on_non_files() {
    let mut fs = new_fs();
    let mut b = [0u8; 4];
    assert_eq!(fs.read_at(R, 0, &mut b), Err(err(EISDIR)));
    assert_eq!(fs.write_at(R, 0, b"x"), Err(err(EISDIR)));
    let l = fs.symlink(b"x", R, b"/l").unwrap();
    assert_eq!(fs.read_at(l, 0, &mut b), Err(err(EINVAL)));
    let n = fs
        .mknod(R, b"/null", abi::S_IFCHR | 0o666, abi::makedev(1, 3), None)
        .unwrap();
    assert_eq!(fs.write_at(n, 0, b"x"), Err(err(EINVAL)));
    assert_eq!(fs.truncate(n, 0), Err(err(EINVAL)));
}

#[test]
fn append_helper() {
    let mut fs = new_fs();
    let ino = put(&mut fs, "/log", b"ab");
    assert_eq!(fs.append(ino, b"cd").unwrap(), (2, 4));
    assert_eq!(fs.append(ino, b"e").unwrap(), (1, 5));
    assert_eq!(read_all(&mut fs, ino).unwrap(), b"abcde");
}

#[test]
fn timestamps() {
    let mut fs = new_fs();
    let d0 = fs.stat(R).unwrap();
    let ino = put(&mut fs, "/f", b"x");
    let d1 = fs.stat(R).unwrap();
    assert!(
        ts(d1.mtime) > ts(d0.mtime) && ts(d1.ctime) > ts(d0.ctime),
        "create updates parent"
    );
    let m0 = fs.stat(ino).unwrap();
    fs.write_at(ino, 0, b"y").unwrap();
    let m1 = fs.stat(ino).unwrap();
    assert!(ts(m1.mtime) > ts(m0.mtime) && ts(m1.ctime) > ts(m0.ctime));
    assert_eq!(m1.atime, m0.atime);
    // relatime: first read after a write updates atime, the next doesn't.
    let mut b = [0u8; 1];
    fs.read_at(ino, 0, &mut b).unwrap();
    let m2 = fs.stat(ino).unwrap();
    assert!(ts(m2.atime) > ts(m1.mtime));
    assert_eq!(m2.mtime, m1.mtime);
    fs.read_at(ino, 0, &mut b).unwrap();
    assert_eq!(fs.stat(ino).unwrap().atime, m2.atime);
    // chmod/chown touch ctime only.
    fs.chmod(ino, 0o4755).unwrap();
    let m3 = fs.stat(ino).unwrap();
    assert_eq!(m3.mode, abi::S_IFREG | 0o4755);
    assert!(ts(m3.ctime) > ts(m2.ctime));
    assert_eq!(m3.mtime, m2.mtime);
    // utimens.
    let t = Timespec {
        tv_sec: 1234,
        tv_nsec: 5678,
    };
    fs.utimens(ino, SetTime::At(t), SetTime::Omit).unwrap();
    let m4 = fs.stat(ino).unwrap();
    assert_eq!(m4.atime, t);
    assert_eq!(m4.mtime, m3.mtime);
    assert!(ts(m4.ctime) > ts(m3.ctime));
    fs.utimens(ino, SetTime::Omit, SetTime::Omit).unwrap();
    assert_eq!(fs.stat(ino).unwrap(), m4);
    fs.utimens(ino, SetTime::Omit, SetTime::Now).unwrap();
    let m5 = fs.stat(ino).unwrap();
    assert_eq!(m5.mtime, m5.ctime);
    assert_eq!(
        fs.utimens(
            ino,
            SetTime::At(Timespec {
                tv_sec: 0,
                tv_nsec: 2_000_000_000
            }),
            SetTime::Omit
        ),
        Err(err(EINVAL))
    );
    use patina_abi::time::{UTIME_NOW, UTIME_OMIT};
    let ts = |n| {
        Some(Timespec {
            tv_sec: 9,
            tv_nsec: n,
        })
    };
    assert_eq!(SetTime::from_timespec(None), Ok(SetTime::Now));
    assert_eq!(SetTime::from_timespec(ts(UTIME_NOW)), Ok(SetTime::Now));
    assert_eq!(SetTime::from_timespec(ts(UTIME_OMIT)), Ok(SetTime::Omit));
    assert_eq!(SetTime::from_timespec(ts(-1)), Err(err(EINVAL)));
    assert_eq!(
        SetTime::from_timespec(ts(7)),
        Ok(SetTime::At(Timespec {
            tv_sec: 9,
            tv_nsec: 7
        }))
    );
}

#[test]
fn chown_clears_setid() {
    let mut fs = new_fs();
    let ino = put(&mut fs, "/f", b"");
    fs.chmod(ino, 0o6755).unwrap();
    fs.chown(ino, Some(5), None).unwrap();
    let m = fs.stat(ino).unwrap();
    assert_eq!((m.uid, m.gid), (5, 0));
    assert_eq!(m.mode & 0o7777, 0o0755);
    // setgid without group-exec is mandatory-locking, kept.
    fs.chmod(ino, 0o2644).unwrap();
    fs.chown(ino, None, Some(7)).unwrap();
    let m = fs.stat(ino).unwrap();
    assert_eq!((m.uid, m.gid), (5, 7));
    assert_eq!(m.mode & 0o7777, 0o2644);
    // Directories keep their bits.
    // mkdir drops set-group-ID from the mode (it is only inherited).
    let d = fs.mkdir(R, b"/d", 0o2755).unwrap();
    assert_eq!(fs.stat(d).unwrap().mode & 0o7777, 0o0755);
    fs.chmod(d, 0o2755).unwrap();
    fs.chown(d, Some(1), Some(1)).unwrap();
    assert_eq!(fs.stat(d).unwrap().mode & 0o7777, 0o2755);
}

#[test]
fn setgid_dir_inheritance_and_owner() {
    let mut fs = new_fs();
    let d = fs.mkdir(R, b"/g", 0o755).unwrap();
    fs.chown(d, None, Some(50)).unwrap();
    fs.chmod(d, 0o2775).unwrap();
    fs.set_owner(10, 20);
    let f = put(&mut fs, "/g/f", b"");
    let sub = fs.mkdir(R, b"/g/sub", 0o755).unwrap();
    let mf = fs.stat(f).unwrap();
    assert_eq!((mf.uid, mf.gid), (10, 50));
    let ms = fs.stat(sub).unwrap();
    assert_eq!(ms.gid, 50);
    assert_eq!(ms.mode & 0o7777, 0o2755);
    let other = put(&mut fs, "/o", b"");
    assert_eq!(fs.stat(other).unwrap().gid, 20);
}

#[test]
fn mkdir_mode_masks() {
    let mut fs = new_fs();
    let d = fs.mkdir(R, b"/d", 0o7777).unwrap();
    assert_eq!(fs.stat(d).unwrap().mode, abi::S_IFDIR | 0o1777);
    let o = fs
        .open(
            R,
            b"/f",
            &OpenOptions {
                mode: 0o107777,
                ..ow()
            },
        )
        .unwrap();
    assert_eq!(fs.stat(o.ino).unwrap().mode, abi::S_IFREG | 0o7777);
    fs.put_ref(o.ino);
}

#[test]
fn stat_encodings() {
    let mut fs = new_fs();
    fs.set_dev(abi::makedev(0, 0x2a));
    let ino = put(&mut fs, "/f", b"12345");
    let dev = fs
        .mknod(R, b"/tty", abi::S_IFCHR | 0o620, abi::makedev(5, 0), None)
        .unwrap();
    let t = Timespec {
        tv_sec: 1_700_000_000,
        tv_nsec: 123_456_789,
    };
    fs.utimens(ino, SetTime::At(t), SetTime::At(t)).unwrap();
    let m = fs.stat(ino).unwrap();
    let x = m.to_stat_x86_64();
    assert_eq!(x.st_ino, ino);
    assert_eq!(x.st_size, 5);
    assert_eq!(x.st_nlink, 1);
    assert_eq!(x.st_mode, abi::S_IFREG | 0o644);
    assert_eq!(x.st_dev, abi::makedev(0, 0x2a));
    assert_eq!(x.st_mtime, 1_700_000_000);
    assert_eq!(x.st_mtime_nsec, 123_456_789);
    assert_eq!(x.st_blksize, 4096);
    assert_eq!(x.st_blocks, 8);
    let a = m.to_stat_aarch64();
    assert_eq!(a.st_ino, ino);
    assert_eq!(a.st_size, 5);
    assert_eq!(a.st_nlink, 1);
    assert_eq!(a.st_blksize, 4096);
    assert_eq!(a.st_atime_nsec, 123_456_789);
    let _ = m.to_stat();
    let sx = m.to_statx();
    assert_eq!(sx.stx_mask, abi::STATX_BASIC_STATS | abi::STATX_BTIME);
    assert_eq!(sx.stx_mode as u32, abi::S_IFREG | 0o644);
    assert_eq!(sx.stx_size, 5);
    assert_eq!(sx.stx_mtime.tv_sec, 1_700_000_000);
    assert_eq!(sx.stx_mtime.tv_nsec, 123_456_789);
    assert_eq!((sx.stx_dev_major, sx.stx_dev_minor), (0, 0x2a));
    let d = fs.stat(dev).unwrap();
    assert_eq!(d.rdev, abi::makedev(5, 0));
    let sx = d.to_statx();
    assert_eq!((sx.stx_rdev_major, sx.stx_rdev_minor), (5, 0));
    assert_eq!(d.to_stat_x86_64().st_rdev, abi::makedev(5, 0));
    assert_eq!(fs.rdev(dev).unwrap(), abi::makedev(5, 0));
}

#[test]
fn access_checks() {
    let mut fs = new_fs();
    let f = put(&mut fs, "/f", b"");
    assert_eq!(fs.access(f, abi::F_OK), Ok(()));
    assert_eq!(fs.access(f, abi::R_OK | abi::W_OK), Ok(()));
    assert_eq!(fs.access(f, abi::X_OK), Err(err(EACCES)));
    fs.chmod(f, 0o700).unwrap();
    assert_eq!(fs.access(f, abi::X_OK), Ok(()));
    assert_eq!(fs.access(R, abi::X_OK), Ok(()));
    assert_eq!(fs.access(f, 8), Err(err(EINVAL)));
}

#[derive(Debug)]
struct Dev(u32);
impl Special for Dev {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn size(&self) -> u64 {
        self.0 as u64
    }
}

#[test]
fn specials() {
    let mut fs = new_fs();
    fs.mkdir(R, b"/dev", 0o755).unwrap();
    let null = fs
        .mknod(
            R,
            b"/dev/null",
            abi::S_IFCHR | 0o666,
            abi::makedev(1, 3),
            Some(Arc::new(Dev(0))),
        )
        .unwrap();
    let o = fs
        .open(
            R,
            b"/dev/null",
            &OpenOptions {
                write: true,
                trunc: true,
                ..rd()
            },
        )
        .unwrap();
    assert_eq!(o.ino, null);
    assert_eq!(o.ftype, FileType::CharDevice);
    let s = fs.special(o.ino).unwrap();
    assert!(s.as_any().downcast_ref::<Dev>().is_some());
    fs.put_ref(o.ino);
    // A generated regular file (procfs style).
    let p = fs
        .mknod(
            R,
            b"/meminfo",
            abi::S_IFREG | 0o444,
            0,
            Some(Arc::new(Dev(77))),
        )
        .unwrap();
    assert_eq!(fs.file_type(p).unwrap(), FileType::Regular);
    assert_eq!(fs.stat(p).unwrap().size, 77);
    assert!(fs.special(p).is_some());
    // Plain mknod regular file is a normal file.
    let r = fs.mknod(R, b"/plain", 0o600, 0, None).unwrap();
    assert!(fs.special(r).is_none());
    fs.write_at(r, 0, b"ok").unwrap();
    // Socket, fifo; attach a handle later.
    let sock = fs
        .mknod(R, b"/sock", abi::S_IFSOCK | 0o755, 0, None)
        .unwrap();
    assert!(fs.special(sock).is_none());
    fs.set_special(sock, Some(Arc::new(Dev(1)))).unwrap();
    assert!(fs.special(sock).is_some());
    assert_eq!(fs.set_special(r, None), Err(err(EINVAL)));
    let fifo = fs
        .mknod(R, b"/fifo", abi::S_IFIFO | 0o600, 0, None)
        .unwrap();
    assert_eq!(fs.file_type(fifo).unwrap(), FileType::Fifo);
    assert_eq!(fs.stat(fifo).unwrap().rdev, 0);
    // Errors.
    assert_eq!(
        fs.mknod(R, b"/d", abi::S_IFDIR | 0o755, 0, None),
        Err(err(EPERM))
    );
    assert_eq!(fs.mknod(R, b"/d", 0o170000, 0, None), Err(err(EINVAL)));
    assert_eq!(
        fs.mknod(R, b"/sock", abi::S_IFSOCK, 0, None),
        Err(err(EEXIST))
    );
    assert_eq!(
        fs.mknod(R, b"/new/", abi::S_IFSOCK, 0, None),
        Err(err(ENOENT))
    );
    // Unlinking and freeing a special drops the handle.
    let h: Arc<dyn Special> = Arc::new(Dev(5));
    fs.mknod(R, b"/h", abi::S_IFCHR, 0, Some(h.clone()))
        .unwrap();
    assert_eq!(Arc::strong_count(&h), 2);
    fs.unlink(R, b"/h").unwrap();
    assert_eq!(Arc::strong_count(&h), 1);
}

#[test]
fn mkdir_all_helper() {
    let mut fs = new_fs();
    let d = fs.mkdir_all(R, b"/a/b/c", 0o755).unwrap();
    assert_eq!(fs.getcwd(d).unwrap(), b"/a/b/c");
    assert_eq!(fs.mkdir_all(R, b"a//b/c/", 0o755).unwrap(), d);
    put(&mut fs, "/a/f", b"");
    assert_eq!(fs.mkdir_all(R, b"/a/f/g", 0o755), Err(err(ENOTDIR)));
    fs.symlink(b"a/b", R, b"/lb").unwrap();
    let e = fs.mkdir_all(R, b"/lb/c/e", 0o700).unwrap();
    assert_eq!(fs.getcwd(e).unwrap(), b"/a/b/c/e");
}
