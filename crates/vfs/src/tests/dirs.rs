use super::*;
use alloc::collections::BTreeSet;
use alloc::vec;
use patina_abi::fs as abi;
use std::format;

#[test]
fn mkdir_rmdir_and_nlink() {
    let mut fs = new_fs();
    let a = fs.mkdir(R, b"/a", 0o755).unwrap();
    assert_eq!(fs.stat(R).unwrap().nlink, 3);
    assert_eq!(fs.stat(a).unwrap().nlink, 2);
    fs.mkdir(R, b"/a/x", 0o755).unwrap();
    fs.mkdir(R, b"a/y/", 0o755).unwrap();
    put(&mut fs, "/a/f", b"");
    assert_eq!(fs.stat(a).unwrap().nlink, 4);
    assert_eq!(fs.stat(a).unwrap().size, 5 * 20);
    assert_eq!(fs.mkdir(R, b"/a", 0o755), Err(err(EEXIST)));
    assert_eq!(fs.mkdir(R, b"/a/f", 0o755), Err(err(EEXIST)));
    assert_eq!(fs.mkdir(R, b"/a/f/", 0o755), Err(err(EEXIST)));
    assert_eq!(fs.mkdir(R, b"/a/f/g", 0o755), Err(err(ENOTDIR)));
    assert_eq!(fs.mkdir(R, b"/", 0o755), Err(err(EEXIST)));
    assert_eq!(fs.mkdir(R, b"/a/.", 0o755), Err(err(EEXIST)));
    assert_eq!(fs.mkdir(R, b"/a/..", 0o755), Err(err(EEXIST)));
    assert_eq!(fs.mkdir(R, b"/no/x", 0o755), Err(err(ENOENT)));
    assert_eq!(fs.mkdir(R, b"", 0o755), Err(err(ENOENT)));
    fs.symlink(b"/nowhere", R, b"/dl").unwrap();
    assert_eq!(
        fs.mkdir(R, b"/dl", 0o755),
        Err(err(EEXIST)),
        "dangling symlink is EEXIST"
    );

    assert_eq!(fs.rmdir(R, b"/a"), Err(err(ENOTEMPTY)));
    assert_eq!(fs.rmdir(R, b"/a/f"), Err(err(ENOTDIR)));
    assert_eq!(fs.rmdir(R, b"/a/nope"), Err(err(ENOENT)));
    assert_eq!(fs.rmdir(R, b"/a/x/."), Err(err(EINVAL)));
    assert_eq!(fs.rmdir(R, b"/a/x/.."), Err(err(ENOTEMPTY)));
    assert_eq!(fs.rmdir(R, b"/"), Err(err(EBUSY)));
    assert_eq!(fs.rmdir(R, b"/a/f/"), Err(err(ENOTDIR)));
    fs.symlink(b"x", R, b"/a/lx").unwrap();
    assert_eq!(
        fs.rmdir(R, b"/a/lx"),
        Err(err(ENOTDIR)),
        "rmdir does not follow"
    );
    fs.rmdir(R, b"/a/x/").unwrap();
    assert_eq!(fs.stat(a).unwrap().nlink, 3);
    fs.rmdir(R, b"/a/y").unwrap();
    assert_eq!(fs.stat(a).unwrap().nlink, 2);

    assert_eq!(fs.unlink(R, b"/a"), Err(err(EISDIR)));
    assert_eq!(fs.unlink(R, b"/a/"), Err(err(EISDIR)));
    assert_eq!(fs.unlink(R, b"/a/f/"), Err(err(ENOTDIR)));
    assert_eq!(fs.unlink(R, b"/a/lx/"), Err(err(ENOTDIR)));
    assert_eq!(fs.unlink(R, b"/a/nope/"), Err(err(ENOENT)));
    assert_eq!(fs.unlink(R, b"/"), Err(err(EISDIR)));
    assert_eq!(fs.unlink(R, b"/a/."), Err(err(EISDIR)));
    assert_eq!(fs.unlink(R, b"/a/f/x"), Err(err(ENOTDIR)));
    fs.unlink(R, b"/a/lx").unwrap();
    fs.unlink(R, b"/a/f").unwrap();
    fs.rmdir(R, b"/a").unwrap();
    assert_eq!(fs.stat(R).unwrap().nlink, 2);
    // Everything but root and /dl freed.
    assert_eq!(fs.inode_count(), 2);
}

#[test]
fn getdents_format() {
    let mut fs = new_fs();
    let d = fs.mkdir(R, b"/d", 0o755).unwrap();
    let f = put(&mut fs, "/d/file", b"");
    let s = fs.mkdir(R, b"/d/sub", 0o755).unwrap();
    let l = fs.symlink(b"x", R, b"/d/link").unwrap();
    let c = fs
        .mknod(R, b"/d/chr", abi::S_IFCHR, abi::makedev(1, 3), None)
        .unwrap();
    let mut buf = vec![0u8; 4096];
    let mut pos = 0;
    let n = fs.getdents64(d, &mut pos, &mut buf).unwrap();
    let ents = parse_dirents(&buf[..n]);
    let got: Vec<_> = ents
        .iter()
        .map(|e| (std::str::from_utf8(&e.name).unwrap(), e.ino, e.dtype))
        .collect();
    assert_eq!(
        got,
        [
            (".", d, abi::DT_DIR),
            ("..", R, abi::DT_DIR),
            ("file", f, abi::DT_REG),
            ("sub", s, abi::DT_DIR),
            ("link", l, abi::DT_LNK),
            ("chr", c, abi::DT_CHR),
        ]
    );
    // d_off of the last entry is the stream position.
    assert_eq!(ents.last().unwrap().off as u64, pos);
    assert_eq!(fs.getdents64(d, &mut pos, &mut buf).unwrap(), 0);
    assert_eq!(fs.getdents64(d, &mut pos, &mut buf).unwrap(), 0);
    // Root's `..` is itself.
    let r = list(&mut fs, R, 4096).unwrap();
    assert_eq!(r[1], (b"..".to_vec(), R, abi::DT_DIR));
    // Resume from every reported d_off (seekdir/telldir).
    for (i, e) in ents.iter().enumerate() {
        let mut p = e.off as u64;
        let n = fs.getdents64(d, &mut p, &mut buf).unwrap();
        let rest = parse_dirents(&buf[..n]);
        let want: Vec<_> = ents[i + 1..].iter().map(|e| e.name.clone()).collect();
        let have: Vec<_> = rest.iter().map(|e| e.name.clone()).collect();
        assert_eq!(have, want);
    }
    // Small buffers: one record at a time; too small for any is EINVAL.
    let one = list(&mut fs, d, 32).unwrap();
    assert_eq!(one.len(), 6);
    let mut tiny = [0u8; 20];
    let mut p = 0;
    assert_eq!(fs.getdents64(d, &mut p, &mut tiny), Err(err(EINVAL)));
    assert_eq!(p, 0);
    assert_eq!(fs.getdents64(f, &mut p, &mut buf), Err(err(ENOTDIR)));
}

#[test]
fn getdents_stable_under_modification() {
    let mut fs = new_fs();
    let d = fs.mkdir(R, b"/d", 0o755).unwrap();
    for i in 0..200 {
        put(&mut fs, &format!("/d/f{i:03}"), b"");
    }
    let mut pos = 0;
    let mut buf = vec![0u8; 96]; // ~3 records per call
    let mut seen = Vec::new();
    let mut deleted = BTreeSet::new();
    let mut added = BTreeSet::new();
    let mut round = 0;
    loop {
        let n = fs.getdents64(d, &mut pos, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        for e in parse_dirents(&buf[..n]) {
            seen.push(std::string::String::from_utf8(e.name).unwrap());
        }
        // Between calls: delete some not-yet-seen and some seen entries,
        // add new ones, and rename one.
        round += 1;
        for k in [round * 7 % 200, (round * 13 + 5) % 200] {
            let name = format!("f{k:03}");
            if !deleted.contains(&name) && fs.unlink(R, format!("/d/{name}").as_bytes()).is_ok() {
                deleted.insert(name);
            }
        }
        if round < 50 {
            let new = format!("n{round:03}");
            put(&mut fs, &format!("/d/{new}"), b"");
            added.insert(new);
        }
    }
    let mut uniq = BTreeSet::new();
    for s in &seen {
        assert!(uniq.insert(s.clone()), "duplicate {s}");
    }
    // Every original entry that was never deleted was returned.
    for i in 0..200 {
        let name = format!("f{i:03}");
        if !deleted.contains(&name) {
            assert!(uniq.contains(&name), "missing {name}");
        }
    }
    assert!(uniq.contains("."));
    assert!(uniq.contains(".."));
    // New entries all appear (they get higher cookies than the cursor).
    for a in &added {
        assert!(uniq.contains(a), "added {a} not seen");
    }
}

#[test]
fn getdents_rename_in_progress() {
    // An entry renamed (within the directory) after the cursor passed it is
    // seen again under its new name, never lost; one renamed before the
    // cursor reached it is seen once, under the new name.
    let mut fs = new_fs();
    let d = fs.mkdir(R, b"/d", 0o755).unwrap();
    for n in ["a", "b", "c", "d"] {
        put(&mut fs, &format!("/d/{n}"), b"");
    }
    let mut pos = 0;
    let mut buf = vec![0u8; 24 * 3]; // ".", "..", "a"
    let n = fs.getdents64(d, &mut pos, &mut buf).unwrap();
    assert_eq!(parse_dirents(&buf[..n]).len(), 3);
    fs.rename(R, b"/d/c", R, b"/d/c2", 0).unwrap();
    fs.rename(R, b"/d/a", R, b"/d/a2", 0).unwrap();
    let mut rest = Vec::new();
    loop {
        let n = fs.getdents64(d, &mut pos, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        rest.extend(parse_dirents(&buf[..n]).into_iter().map(|e| e.name));
    }
    assert_eq!(
        rest,
        [b"b".to_vec(), b"d".to_vec(), b"c2".to_vec(), b"a2".to_vec()]
    );
}

#[test]
fn read_dir_order_is_creation_order() {
    let mut fs = new_fs();
    for n in ["zeta", "alpha", "mid"] {
        put(&mut fs, &format!("/{n}"), b"");
    }
    let v: Vec<_> = fs.read_dir(R).unwrap().into_iter().map(|e| e.0).collect();
    assert_eq!(v, [b"zeta".to_vec(), b"alpha".to_vec(), b"mid".to_vec()]);
    assert_eq!(names(&mut fs, "/"), ["alpha", "mid", "zeta"]);
}

#[test]
fn dir_times() {
    let mut fs = new_fs();
    let d = fs.mkdir(R, b"/d", 0o755).unwrap();
    let t0 = fs.stat(d).unwrap();
    put(&mut fs, "/d/f", b"");
    let t1 = fs.stat(d).unwrap();
    assert!(ts(t1.mtime) > ts(t0.mtime));
    fs.unlink(R, b"/d/f").unwrap();
    let t2 = fs.stat(d).unwrap();
    assert!(ts(t2.mtime) > ts(t1.mtime));
    // getdents updates atime (relatime: atime <= mtime).
    list(&mut fs, d, 1024).unwrap();
    assert!(ts(fs.stat(d).unwrap().atime) > ts(t2.mtime));
}
