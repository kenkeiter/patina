use super::*;

#[test]
fn hard_links() {
    let mut fs = new_fs();
    let f = put(&mut fs, "/f", b"data");
    fs.mkdir(R, b"/d", 0o755).unwrap();
    let c0 = fs.stat(f).unwrap().ctime;
    fs.link(R, b"/f", false, R, b"/d/f2").unwrap();
    let m = fs.stat(f).unwrap();
    assert_eq!(m.nlink, 2);
    assert!(ts(m.ctime) > ts(c0));
    assert_eq!(fs.resolve(R, b"/d/f2", true), Ok(f));
    fs.write_at(f, 4, b"!").unwrap();
    assert_eq!(get(&mut fs, "/d/f2").unwrap(), b"data!");
    fs.unlink(R, b"/f").unwrap();
    assert_eq!(fs.stat(f).unwrap().nlink, 1);
    assert_eq!(get(&mut fs, "/d/f2").unwrap(), b"data!");
    fs.unlink(R, b"/d/f2").unwrap();
    assert!(fs.stat(f).is_err());
}

#[test]
fn link_errors() {
    let mut fs = new_fs();
    put(&mut fs, "/f", b"");
    fs.mkdir(R, b"/d", 0o755).unwrap();
    assert_eq!(fs.link(R, b"/d", false, R, b"/d2"), Err(err(EPERM)));
    assert_eq!(fs.link(R, b"/f", false, R, b"/d"), Err(err(EEXIST)));
    assert_eq!(
        fs.link(R, b"/d", false, R, b"/f"),
        Err(err(EEXIST)),
        "EEXIST before EPERM"
    );
    assert_eq!(fs.link(R, b"/nope", false, R, b"/x"), Err(err(ENOENT)));
    assert_eq!(fs.link(R, b"/f", false, R, b"/nope/x"), Err(err(ENOENT)));
    assert_eq!(fs.link(R, b"/f", false, R, b"/x/"), Err(err(ENOENT)));
    assert_eq!(fs.link(R, b"/f", false, R, b"/d/."), Err(err(EEXIST)));
    assert_eq!(fs.link(R, b"/f/", false, R, b"/x"), Err(err(ENOTDIR)));
}

#[test]
fn link_symlink_follow_flag() {
    let mut fs = new_fs();
    let f = put(&mut fs, "/f", b"");
    let l = fs.symlink(b"/f", R, b"/l").unwrap();
    fs.link(R, b"/l", false, R, b"/l2").unwrap();
    assert_eq!(fs.resolve(R, b"/l2", false), Ok(l));
    assert_eq!(fs.stat(l).unwrap().nlink, 2);
    fs.link(R, b"/l", true, R, b"/f2").unwrap();
    assert_eq!(fs.resolve(R, b"/f2", false), Ok(f));
}

#[test]
fn link_unlinked_open_file_is_enoent() {
    let mut fs = new_fs();
    let o = fs.open(R, b"/f", &ow()).unwrap();
    fs.unlink(R, b"/f").unwrap();
    assert_eq!(fs.link_ino(o.ino, R, b"/back"), Err(err(ENOENT)));
    fs.put_ref(o.ino);
}

#[test]
fn unlink_while_open() {
    let mut fs = new_fs();
    let o = fs.open(R, b"/f", &ow()).unwrap();
    fs.write_at(o.ino, 0, &[7u8; 10000]).unwrap();
    assert_eq!(fs.pages_used(), 3);
    fs.unlink(R, b"/f").unwrap();
    assert_eq!(fs.resolve(R, b"/f", true), Err(err(ENOENT)));
    // Still readable, writable and stat-able through the open reference.
    assert_eq!(fs.stat(o.ino).unwrap().nlink, 0);
    fs.write_at(o.ino, 10000, b"tail").unwrap();
    let data = read_all(&mut fs, o.ino).unwrap();
    assert_eq!(data.len(), 10004);
    assert_eq!(&data[10000..], b"tail");
    fs.truncate(o.ino, 5).unwrap();
    // A new file of the same name is independent.
    let n = put(&mut fs, "/f", b"new");
    assert_ne!(n, o.ino);
    // Second reference (dup / fork of the description).
    fs.get_ref(o.ino).unwrap();
    fs.put_ref(o.ino);
    assert!(fs.stat(o.ino).is_ok());
    fs.put_ref(o.ino);
    assert!(fs.stat(o.ino).is_err());
    assert_eq!(fs.pages_used(), 1, "only /f's page remains");
}

#[test]
fn unlinked_dir_stays_open() {
    let mut fs = new_fs();
    let d = fs.mkdir(R, b"/d", 0o755).unwrap();
    let o = fs
        .open(
            R,
            b"/d",
            &OpenOptions {
                directory: true,
                ..rd()
            },
        )
        .unwrap();
    assert_eq!(o.ino, d);
    fs.rmdir(R, b"/d").unwrap();
    assert_eq!(fs.stat(d).unwrap().nlink, 0);
    assert_eq!(fs.stat(R).unwrap().nlink, 2);
    let mut pos = 0;
    let mut buf = [0u8; 128];
    assert_eq!(fs.getdents64(d, &mut pos, &mut buf), Err(err(ENOENT)));
    fs.put_ref(d);
    assert_eq!(fs.inode_count(), 1);
}

#[test]
fn ref_errors() {
    let mut fs = new_fs();
    assert_eq!(fs.get_ref(999), Err(err(ENOENT)));
    // Releasing an unknown inode is harmless.
    fs.put_ref(999);
    // The root is never freed.
    fs.put_ref(R);
    assert!(fs.stat(R).is_ok());
}
