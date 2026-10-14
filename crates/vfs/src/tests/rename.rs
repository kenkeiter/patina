use super::*;
use patina_abi::fs::{RENAME_EXCHANGE as EXCH, RENAME_NOREPLACE as NOREP, RENAME_WHITEOUT};

fn setup() -> Fs {
    let mut fs = new_fs();
    fs.mkdir_all(R, b"/a/b/c", 0o755).unwrap();
    fs.mkdir(R, b"/e", 0o755).unwrap(); // empty dir
    put(&mut fs, "/f", b"F");
    put(&mut fs, "/g", b"G");
    put(&mut fs, "/a/af", b"AF");
    fs
}

#[test]
fn simple_moves() {
    let mut fs = setup();
    let f = fs.resolve(R, b"/f", true).unwrap();
    fs.rename(R, b"/f", R, b"/a/b/f2", 0).unwrap();
    assert_eq!(fs.resolve(R, b"/f", true), Err(err(ENOENT)));
    assert_eq!(fs.resolve(R, b"/a/b/f2", true), Ok(f));
    assert_eq!(get(&mut fs, "/a/b/f2").unwrap(), b"F");
    // Overwrite a file: the old target is freed.
    let n0 = fs.inode_count();
    fs.rename(R, b"/g", R, b"/a/b/f2", 0).unwrap();
    assert_eq!(get(&mut fs, "/a/b/f2").unwrap(), b"G");
    assert_eq!(fs.inode_count(), n0 - 1);
    // Same file: no-op.
    fs.rename(R, b"/a/b/f2", R, b"/a/b/f2", 0).unwrap();
    assert_eq!(get(&mut fs, "/a/b/f2").unwrap(), b"G");
    // Relative to different start dirs.
    let a = fs.resolve(R, b"/a", true).unwrap();
    let b = fs.resolve(R, b"/a/b", true).unwrap();
    fs.rename(b, b"f2", a, b"moved", 0).unwrap();
    assert_eq!(get(&mut fs, "/a/moved").unwrap(), b"G");
}

#[test]
fn directory_moves_and_nlink() {
    let mut fs = setup();
    let a = fs.resolve(R, b"/a", true).unwrap();
    let e = fs.resolve(R, b"/e", true).unwrap();
    let b = fs.resolve(R, b"/a/b", true).unwrap();
    assert_eq!(fs.stat(a).unwrap().nlink, 3);
    assert_eq!(fs.stat(e).unwrap().nlink, 2);
    fs.rename(R, b"/a/b", R, b"/e/b", 0).unwrap();
    assert_eq!(fs.stat(a).unwrap().nlink, 2);
    assert_eq!(fs.stat(e).unwrap().nlink, 3);
    assert_eq!(fs.resolve(b, b"..", true), Ok(e));
    assert_eq!(
        fs.getcwd(fs.resolve(R, b"/e/b/c", true).unwrap()).unwrap(),
        b"/e/b/c"
    );
    // Replace an empty directory.
    fs.mkdir(R, b"/a/empty", 0o755).unwrap();
    assert_eq!(fs.stat(a).unwrap().nlink, 3);
    fs.rename(R, b"/e/b", R, b"/a/empty", 0).unwrap();
    assert_eq!(fs.stat(a).unwrap().nlink, 3);
    assert_eq!(fs.stat(e).unwrap().nlink, 2);
    assert_eq!(
        fs.resolve(R, b"/a/empty/c", true),
        fs.resolve(b, b"c", true)
    );
    // Trailing slashes are fine on directories.
    fs.rename(R, b"/a/empty/", R, b"/b2/", 0).unwrap();
    assert_eq!(fs.resolve(R, b"/b2", true), Ok(b));
    assert_eq!(fs.stat(R).unwrap().nlink, 5); // a, e, b2 + 2
}

#[test]
fn errors() {
    let mut fs = setup();
    let r =
        |fs: &mut Fs, a: &str, b: &str, fl: u32| fs.rename(R, a.as_bytes(), R, b.as_bytes(), fl);
    assert_eq!(r(&mut fs, "/nope", "/x", 0), Err(err(ENOENT)));
    assert_eq!(r(&mut fs, "/f", "/nodir/x", 0), Err(err(ENOENT)));
    assert_eq!(r(&mut fs, "/f", "/g/x", 0), Err(err(ENOTDIR)));
    // Type mismatches.
    assert_eq!(r(&mut fs, "/a", "/f", 0), Err(err(ENOTDIR)));
    assert_eq!(r(&mut fs, "/f", "/e", 0), Err(err(EISDIR)));
    assert_eq!(r(&mut fs, "/e", "/a", 0), Err(err(ENOTEMPTY)));
    // Trailing slash on a non-directory.
    assert_eq!(r(&mut fs, "/f/", "/x", 0), Err(err(ENOTDIR)));
    assert_eq!(r(&mut fs, "/f", "/x/", 0), Err(err(ENOTDIR)));
    assert_eq!(r(&mut fs, "/f", "/g/", 0), Err(err(ENOTDIR)));
    // Into own subtree / onto an ancestor.
    assert_eq!(r(&mut fs, "/a", "/a/b/c/x", 0), Err(err(EINVAL)));
    assert_eq!(r(&mut fs, "/a", "/a/x", 0), Err(err(EINVAL)));
    assert_eq!(r(&mut fs, "/a/b", "/a/b/c", 0), Err(err(EINVAL)));
    assert_eq!(r(&mut fs, "/a/b/c", "/a", 0), Err(err(ENOTEMPTY)));
    fs.mkdir(R, b"/a/b/c/d", 0o755).unwrap();
    assert_eq!(r(&mut fs, "/a/b/c/d", "/a/b", 0), Err(err(ENOTEMPTY)));
    // Dot entries and root.
    assert_eq!(r(&mut fs, "/a/.", "/x", 0), Err(err(EBUSY)));
    assert_eq!(r(&mut fs, "/a/..", "/x", 0), Err(err(EBUSY)));
    assert_eq!(r(&mut fs, "/", "/x", 0), Err(err(EBUSY)));
    assert_eq!(r(&mut fs, "/f", "/a/.", 0), Err(err(EBUSY)));
    assert_eq!(r(&mut fs, "/f", "/", 0), Err(err(EBUSY)));
    // Flags.
    assert_eq!(r(&mut fs, "/f", "/g", NOREP), Err(err(EEXIST)));
    assert_eq!(r(&mut fs, "/f", "/g", NOREP | EXCH), Err(err(EINVAL)));
    assert_eq!(r(&mut fs, "/f", "/g", RENAME_WHITEOUT), Err(err(EINVAL)));
    assert_eq!(r(&mut fs, "/f", "/g", 0x80), Err(err(EINVAL)));
    assert_eq!(r(&mut fs, "/f", "/nope", EXCH), Err(err(ENOENT)));
    assert_eq!(r(&mut fs, "", "/x", 0), Err(err(ENOENT)));
    // Nothing changed.
    assert_eq!(get(&mut fs, "/f").unwrap(), b"F");
    assert_eq!(get(&mut fs, "/g").unwrap(), b"G");
    r(&mut fs, "/f", "/new", NOREP).unwrap();
    assert_eq!(get(&mut fs, "/new").unwrap(), b"F");
}

#[test]
fn hardlinks_same_inode_noop() {
    let mut fs = setup();
    fs.link(R, b"/f", false, R, b"/f2").unwrap();
    fs.rename(R, b"/f", R, b"/f2", 0).unwrap();
    // Linux: both names remain.
    assert_eq!(get(&mut fs, "/f").unwrap(), b"F");
    assert_eq!(get(&mut fs, "/f2").unwrap(), b"F");
    assert_eq!(
        fs.stat(fs.resolve(R, b"/f", true).unwrap()).unwrap().nlink,
        2
    );
}

#[test]
fn exchange() {
    let mut fs = setup();
    let f = fs.resolve(R, b"/f", true).unwrap();
    let a = fs.resolve(R, b"/a", true).unwrap();
    let e = fs.resolve(R, b"/e", true).unwrap();
    // File <-> directory across parents.
    fs.rename(R, b"/f", R, b"/a/b", EXCH).unwrap();
    let b = fs.resolve(R, b"/f", true).unwrap();
    assert_eq!(fs.resolve(R, b"/a/b", true), Ok(f));
    assert_eq!(fs.file_type(b).unwrap(), FileType::Directory);
    assert_eq!(fs.resolve(b, b"..", true), Ok(R));
    assert_eq!(fs.getcwd(b).unwrap(), b"/f");
    assert_eq!(fs.stat(a).unwrap().nlink, 2);
    assert_eq!(fs.stat(R).unwrap().nlink, 5); // a, e, f(dir)
                                              // Directory <-> directory.
    fs.rename(R, b"/e", R, b"/f", EXCH).unwrap();
    assert_eq!(fs.resolve(R, b"/e", true), Ok(b));
    assert_eq!(fs.resolve(R, b"/f", true), Ok(e));
    assert_eq!(fs.getcwd(e).unwrap(), b"/f");
    // Exchange with an ancestor is EINVAL either way round.
    assert_eq!(fs.rename(R, b"/e", R, b"/e/c", EXCH), Err(err(EINVAL)));
    assert_eq!(fs.rename(R, b"/e/c", R, b"/e", EXCH), Err(err(EINVAL)));
    // Trailing slash on a non-directory target.
    assert_eq!(fs.rename(R, b"/e", R, b"/g/", EXCH), Err(err(ENOTDIR)));
    assert_eq!(fs.rename(R, b"/g/", R, b"/e", EXCH), Err(err(ENOTDIR)));
    // Same directory, files.
    put(&mut fs, "/h", b"H");
    fs.rename(R, b"/g", R, b"/h", EXCH).unwrap();
    assert_eq!(get(&mut fs, "/g").unwrap(), b"H");
    assert_eq!(get(&mut fs, "/h").unwrap(), b"G");
}

#[test]
fn rename_open_target_keeps_data() {
    let mut fs = setup();
    let o = fs.open(R, b"/g", &rd()).unwrap();
    fs.rename(R, b"/f", R, b"/g", 0).unwrap();
    assert_eq!(fs.stat(o.ino).unwrap().nlink, 0);
    assert_eq!(read_all(&mut fs, o.ino).unwrap(), b"G");
    fs.put_ref(o.ino);
    assert!(fs.stat(o.ino).is_err());
}

#[test]
fn rename_into_removed_dir() {
    let mut fs = setup();
    let e = fs.resolve(R, b"/e", true).unwrap();
    fs.get_ref(e).unwrap();
    fs.rmdir(R, b"/e").unwrap();
    assert_eq!(fs.rename(R, b"/f", e, b"x", 0), Err(err(ENOENT)));
    fs.put_ref(e);
}

#[test]
fn symlink_rename_not_followed() {
    let mut fs = setup();
    let l = fs.symlink(b"/a", R, b"/la").unwrap();
    fs.rename(R, b"/la", R, b"/lb", 0).unwrap();
    assert_eq!(fs.resolve(R, b"/lb", false), Ok(l));
    assert!(fs.resolve(R, b"/a", true).is_ok());
    // A trailing slash on a symlink to a directory: the link itself is a
    // non-directory, so ENOTDIR.
    assert_eq!(fs.rename(R, b"/lb/", R, b"/lc", 0), Err(err(ENOTDIR)));
}
