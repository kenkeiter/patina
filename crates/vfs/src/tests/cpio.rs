use super::*;
use alloc::vec;
use patina_abi::fs as abi;
use patina_cpio::{EntryMeta, Format, Reader, Writer};

const SAMPLE: &[u8] = include_bytes!("../../../cpio/tests/fixtures/sample.newc.cpio");
const MTIME: i64 = 1_735_787_045;

fn meta(ino: u32, mode: u32) -> EntryMeta {
    EntryMeta {
        ino,
        mode,
        nlink: 1,
        mtime: 1_600_000_000,
        ..Default::default()
    }
}

fn build(entries: &[(EntryMeta, &[u8], &[u8])]) -> Vec<u8> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut w = Writer::new(&mut buf, Format::Newc);
    for (m, n, d) in entries {
        w.entry(m, n, d).unwrap();
    }
    let n = w.finish().unwrap();
    buf.truncate(n);
    buf
}

fn check_sample(fs: &mut Fs) {
    // Stat before reading: relatime moves atime on the first read.
    let app = fs.resolve(R, b"/app", true).unwrap();
    assert_eq!(fs.resolve(R, b"/app.hard", true), Ok(app));
    let m = fs.stat(app).unwrap();
    assert_eq!(m.nlink, 2);
    assert_eq!(m.mode, abi::S_IFREG | 0o755);
    assert_eq!(m.mtime.tv_sec, MTIME);
    assert_eq!(m.atime.tv_sec, MTIME);
    assert_eq!(get(fs, "/app").unwrap(), b"hello from patina\n");
    assert_eq!(get(fs, "/app.hard").unwrap(), b"hello from patina\n");
    assert_ne!(fs.stat(app).unwrap().atime.tv_sec, MTIME);
    assert_eq!(get(fs, "/dir/five").unwrap(), b"abcde");
    let five = fs.resolve(R, b"/dir/five", true).unwrap();
    assert_eq!(fs.stat(five).unwrap().mode & 0o7777, 0o600);
    assert_eq!(get(fs, "/dir/sub/empty").unwrap(), b"");
    assert_eq!(get(fs, "/etc/hosts").unwrap(), b"127.0.0.1 localhost\n");
    assert_eq!(get(fs, "/odd-name-len").unwrap(), b"x");
    assert_eq!(fs.readlink(R, b"/link-to-app").unwrap(), b"/app");
    assert_eq!(get(fs, "/link-to-app").unwrap(), b"hello from patina\n");
    let ed = fs.resolve(R, b"/empty-dir", true).unwrap();
    let m = fs.stat(ed).unwrap();
    assert_eq!(m.mode, abi::S_IFDIR | 0o700);
    // Directory mtimes are applied after their children were added.
    assert_eq!(m.mtime.tv_sec, MTIME);
    let dir = fs.resolve(R, b"/dir", true).unwrap();
    assert_eq!(fs.stat(dir).unwrap().mtime.tv_sec, MTIME);
    assert_eq!(fs.stat(R).unwrap().mtime.tv_sec, MTIME);
    assert_eq!(fs.stat(dir).unwrap().nlink, 3);
    assert_eq!(
        names(fs, "/"),
        [
            "app",
            "app.hard",
            "dir",
            "empty-dir",
            "etc",
            "link-to-app",
            "odd-name-len"
        ]
    );
}

#[test]
fn bsdcpio_fixture_copy() {
    let mut fs = new_fs();
    let rep = populate_from_cpio(
        &mut fs,
        Reader::initramfs(SAMPLE),
        &PopulateOptions::default(),
    )
    .unwrap();
    assert_eq!(rep.entries, 12);
    assert_eq!(rep.skipped, 0, "{rep:?}");
    check_sample(&mut fs);
    assert!(fs.pages_used() > 0);
}

#[test]
fn bsdcpio_fixture_static_zero_copy() {
    let mut fs = new_fs();
    let rep = populate_from_cpio_static(
        &mut fs,
        Reader::initramfs(SAMPLE),
        &PopulateOptions::default(),
    )
    .unwrap();
    assert_eq!(rep.skipped, 0);
    check_sample(&mut fs);
    assert_eq!(fs.pages_used(), 0, "file data borrowed from the archive");
    // Copy-on-write: writing materialises a page; the archive is untouched.
    let ino = fs.resolve(R, b"/etc/hosts", true).unwrap();
    assert_eq!(fs.stat(ino).unwrap().blocks, 8);
    assert_eq!(fs.static_contents(ino), Some(&b"127.0.0.1 localhost\n"[..]));
    assert_eq!(fs.static_contents(R), None);
    fs.write_at(ino, 0, b"10").unwrap();
    assert_eq!(
        fs.static_contents(ino),
        None,
        "written: no longer the backing"
    );
    assert_eq!(fs.pages_used(), 1);
    assert_eq!(
        get(&mut fs, "/etc/hosts").unwrap(),
        b"107.0.0.1 localhost\n"
    );
    // Shrink then grow: the old backing bytes must not reappear.
    let five = fs.resolve(R, b"/dir/five", true).unwrap();
    assert!(fs.static_contents(five).is_some());
    fs.truncate(five, 2).unwrap();
    assert_eq!(fs.static_contents(five), Some(&b"ab"[..]), "shrunk backing");
    fs.truncate(five, 5).unwrap();
    assert_eq!(fs.static_contents(five), None, "grown past the backing");
    assert_eq!(get(&mut fs, "/dir/five").unwrap(), b"ab\0\0\0");
    fs.write_at(five, 4, b"Z").unwrap();
    assert_eq!(get(&mut fs, "/dir/five").unwrap(), b"ab\0\0Z");
}

#[test]
fn static_large_file_partial_pages() {
    // A multi-page file read across page boundaries from the backing.
    let data: Vec<u8> = (0..20000u32).map(|i| (i % 251) as u8).collect();
    let arch = build(&[(meta(1, abi::S_IFREG | 0o644), b"big", &data)]);
    let arch: &'static [u8] = Vec::leak(arch);
    let mut fs = new_fs();
    populate_from_cpio_static(&mut fs, Reader::new(arch), &PopulateOptions::default()).unwrap();
    let ino = fs.resolve(R, b"/big", true).unwrap();
    assert_eq!(read_all(&mut fs, ino).unwrap(), data);
    let mut b = vec![0u8; 5000];
    fs.read_at(ino, 4000, &mut b).unwrap();
    assert_eq!(b, &data[4000..9000]);
    fs.write_at(ino, 4095, b"XY").unwrap();
    assert_eq!(fs.pages_used(), 2);
    let mut want = data.clone();
    want[4095] = b'X';
    want[4096] = b'Y';
    assert_eq!(read_all(&mut fs, ino).unwrap(), want);
    assert_eq!(fs.stat(ino).unwrap().blocks, 5 * 8);
    fs.unlink(R, b"/big").unwrap();
    assert_eq!(fs.pages_used(), 0);
}

#[test]
fn missing_parents_and_devices() {
    let arch = build(&[
        (meta(1, abi::S_IFREG | 0o755), b"./app", b"ELF"),
        (
            EntryMeta {
                rdev_major: 1,
                rdev_minor: 3,
                ..meta(2, abi::S_IFCHR | 0o666)
            },
            b"dev/null",
            b"",
        ),
        (meta(3, abi::S_IFIFO | 0o600), b"run/fifo", b""),
        (meta(4, abi::S_IFREG | 0o644), b"/abs/deep/file", b"x"),
    ]);
    let mut fs = new_fs();
    let rep = populate_from_cpio(&mut fs, Reader::new(&arch), &PopulateOptions::default()).unwrap();
    assert_eq!(rep.skipped, 0);
    let null = fs.resolve(R, b"/dev/null", true).unwrap();
    let m = fs.stat(null).unwrap();
    assert_eq!(m.mode, abi::S_IFCHR | 0o666);
    assert_eq!(m.rdev, abi::makedev(1, 3));
    assert_eq!(m.mtime.tv_sec, 1_600_000_000);
    assert!(fs.special(null).is_none());
    assert_eq!(
        fs.file_type(fs.resolve(R, b"/run/fifo", true).unwrap())
            .unwrap(),
        FileType::Fifo
    );
    assert_eq!(get(&mut fs, "/abs/deep/file").unwrap(), b"x");
    let dev = fs.resolve(R, b"/dev", true).unwrap();
    assert_eq!(fs.stat(dev).unwrap().mode, abi::S_IFDIR | 0o755);

    // Linux-strict: entries with missing parents are skipped.
    let mut fs = new_fs();
    let rep = populate_from_cpio(
        &mut fs,
        Reader::new(&arch),
        &PopulateOptions {
            create_parents: false,
        },
    )
    .unwrap();
    assert_eq!(rep.skipped, 3);
    assert_eq!(rep.first_error.map(|e| e.1), Some(err(ENOENT)));
    assert_eq!(get(&mut fs, "/app").unwrap(), b"ELF");
}

#[test]
fn duplicates_and_type_changes() {
    let arch = build(&[
        (meta(1, abi::S_IFDIR | 0o755), b"d", b""),
        (meta(2, abi::S_IFREG | 0o644), b"d/f", b"one"),
        // Same file again: replaced (truncated, rewritten).
        (meta(3, abi::S_IFREG | 0o600), b"d/f", b"2"),
        // Directory again: kept, mode updated.
        (meta(4, abi::S_IFDIR | 0o700), b"d", b""),
        // File where a symlink was: symlink removed.
        (meta(5, abi::S_IFLNK | 0o777), b"x", b"/d/f"),
        (meta(6, abi::S_IFREG | 0o644), b"x", b"file now"),
        // Symlink replacing a file.
        (meta(7, abi::S_IFREG | 0o644), b"y", b"file"),
        (meta(8, abi::S_IFLNK | 0o777), b"y", b"/d"),
        // Directory replacing an empty directory of... a file.
        (meta(9, abi::S_IFREG | 0o644), b"z", b"file"),
        (meta(10, abi::S_IFDIR | 0o755), b"z", b""),
        // A file cannot replace a non-empty directory: skipped.
        (meta(11, abi::S_IFREG | 0o644), b"d", b"nope"),
        // Root entry sets the root's mode.
        (meta(12, abi::S_IFDIR | 0o700), b".", b""),
    ]);
    let mut fs = new_fs();
    let rep = populate_from_cpio(&mut fs, Reader::new(&arch), &PopulateOptions::default()).unwrap();
    assert_eq!(rep.entries, 12);
    assert_eq!(rep.skipped, 1);
    assert_eq!(rep.first_error.map(|e| e.1), Some(err(EISDIR)));
    assert_eq!(get(&mut fs, "/d/f").unwrap(), b"2");
    let f = fs.resolve(R, b"/d/f", true).unwrap();
    assert_eq!(fs.stat(f).unwrap().mode & 0o7777, 0o600);
    let d = fs.resolve(R, b"/d", true).unwrap();
    assert_eq!(fs.stat(d).unwrap().mode, abi::S_IFDIR | 0o700);
    assert_eq!(
        fs.file_type(fs.resolve(R, b"/x", false).unwrap()).unwrap(),
        FileType::Regular
    );
    assert_eq!(get(&mut fs, "/x").unwrap(), b"file now");
    assert_eq!(fs.readlink(R, b"/y").unwrap(), b"/d");
    assert_eq!(
        fs.file_type(fs.resolve(R, b"/z", false).unwrap()).unwrap(),
        FileType::Directory
    );
    assert_eq!(fs.stat(R).unwrap().mode, abi::S_IFDIR | 0o700);
}

#[test]
fn hardlink_groups() {
    let hl = |ino: u32| EntryMeta {
        nlink: 3,
        ..meta(ino, abi::S_IFREG | 0o755)
    };
    let arch = build(&[
        (hl(10), b"a", b""),
        (hl(10), b"b", b""),
        (hl(10), b"c", b"shared data"),
        // Same ino but different dev is a different file.
        (
            EntryMeta {
                dev_minor: 1,
                ..hl(10)
            },
            b"other",
            b"other data",
        ),
        // nlink 1 entries never link even with equal ino.
        (meta(10, abi::S_IFREG | 0o755), b"single", b"s"),
    ]);
    let mut fs = new_fs();
    let rep = populate_from_cpio(&mut fs, Reader::new(&arch), &PopulateOptions::default()).unwrap();
    assert_eq!(rep.skipped, 0);
    let a = fs.resolve(R, b"/a", true).unwrap();
    assert_eq!(fs.resolve(R, b"/b", true), Ok(a));
    assert_eq!(fs.resolve(R, b"/c", true), Ok(a));
    assert_eq!(fs.stat(a).unwrap().nlink, 3);
    assert_eq!(get(&mut fs, "/a").unwrap(), b"shared data");
    assert_ne!(fs.resolve(R, b"/other", true), Ok(a));
    assert_eq!(get(&mut fs, "/other").unwrap(), b"other data");
    assert_ne!(fs.resolve(R, b"/single", true), Ok(a));
    assert_eq!(get(&mut fs, "/single").unwrap(), b"s");
}

#[test]
fn malformed_archive_reports_error_keeps_prefix() {
    let mut arch = build(&[
        (meta(1, abi::S_IFREG | 0o644), b"ok", b"fine"),
        (meta(2, abi::S_IFREG | 0o644), b"cut", b"0123456789"),
    ]);
    // Chop the archive inside the second entry's data.
    let at = arch.windows(3).position(|w| w == b"cut").unwrap() + 8;
    arch.truncate(at);
    let mut fs = new_fs();
    let r = populate_from_cpio(&mut fs, Reader::new(&arch), &PopulateOptions::default());
    assert!(r.is_err());
    assert_eq!(get(&mut fs, "/ok").unwrap(), b"fine");
}

#[test]
fn concatenated_archives_override() {
    let mut a = build(&[(meta(1, abi::S_IFREG | 0o644), b"etc/conf", b"base")]);
    a.extend_from_slice(&[0; 16]);
    a.extend(build(&[(
        meta(1, abi::S_IFREG | 0o644),
        b"etc/conf",
        b"overlay",
    )]));
    let mut fs = new_fs();
    populate_from_cpio(&mut fs, Reader::initramfs(&a), &PopulateOptions::default()).unwrap();
    assert_eq!(get(&mut fs, "/etc/conf").unwrap(), b"overlay");
}
