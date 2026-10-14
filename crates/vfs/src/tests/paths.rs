use super::*;
use alloc::vec;

fn setup() -> Fs {
    let mut fs = new_fs();
    fs.mkdir_all(R, b"/a/b/c", 0o755).unwrap();
    put(&mut fs, "/a/b/f", b"F");
    put(&mut fs, "/top", b"T");
    fs
}

#[test]
fn dots_and_slashes() {
    let fs = setup();
    let b = fs.resolve(R, b"/a/b", true).unwrap();
    let a = fs.resolve(R, b"/a", true).unwrap();
    assert_eq!(fs.resolve(R, b"//a///b//", true), Ok(b));
    assert_eq!(fs.resolve(R, b"/a/./b/.", true), Ok(b));
    assert_eq!(fs.resolve(R, b"/a/b/c/..", true), Ok(b));
    assert_eq!(fs.resolve(R, b"/a/b/..", true), Ok(a));
    assert_eq!(fs.resolve(R, b"/..", true), Ok(R));
    assert_eq!(fs.resolve(R, b"/../../a", true), Ok(a));
    assert_eq!(fs.resolve(R, b"/", true), Ok(R));
    assert_eq!(fs.resolve(R, b"///", true), Ok(R));
    assert_eq!(fs.resolve(R, b".", true), Ok(R));
    // Relative to a start directory.
    assert_eq!(fs.resolve(b, b"f", true), fs.resolve(R, b"/a/b/f", true));
    assert_eq!(fs.resolve(b, b"..", true), Ok(a));
    assert_eq!(
        fs.resolve(b, b"../../top", true),
        fs.resolve(R, b"/top", true)
    );
    assert_eq!(fs.resolve(b, b"/top", true), fs.resolve(R, b"/top", true));
    assert_eq!(fs.resolve(b, b".", true), Ok(b));
    // A relative path from a non-directory start.
    let f = fs.resolve(R, b"/top", true).unwrap();
    assert_eq!(fs.resolve(f, b"x", true), Err(err(ENOTDIR)));
    assert_eq!(fs.resolve(f, b"/a", true), Ok(a));
}

#[test]
fn errors() {
    let fs = setup();
    assert_eq!(fs.resolve(R, b"", true), Err(err(ENOENT)));
    assert_eq!(fs.resolve(R, b"/nope", true), Err(err(ENOENT)));
    assert_eq!(fs.resolve(R, b"/nope/x", true), Err(err(ENOENT)));
    assert_eq!(fs.resolve(R, b"/top/x", true), Err(err(ENOTDIR)));
    assert_eq!(fs.resolve(R, b"/top/", true), Err(err(ENOTDIR)));
    assert_eq!(fs.resolve(R, b"/top/.", true), Err(err(ENOTDIR)));
    assert_eq!(fs.resolve(R, b"/top/..", true), Err(err(ENOTDIR)));
    assert_eq!(fs.resolve(R, b"/a/b/f/", true), Err(err(ENOTDIR)));
    // ENOENT for a missing intermediate beats ENOTDIR later.
    assert_eq!(fs.resolve(R, b"/nope/top/x", true), Err(err(ENOENT)));
}

#[test]
fn name_too_long() {
    let mut fs = setup();
    let long = vec![b'x'; 256];
    let ok = vec![b'y'; 255];
    assert_eq!(fs.resolve(R, &long, true), Err(err(ENAMETOOLONG)));
    let mut p = b"/a/".to_vec();
    p.extend_from_slice(&long);
    p.extend_from_slice(b"/x");
    assert_eq!(fs.resolve(R, &p, true), Err(err(ENAMETOOLONG)));
    assert_eq!(fs.open(R, &long, &ow()), Err(err(ENAMETOOLONG)));
    assert_eq!(fs.mkdir(R, &long, 0o755), Err(err(ENAMETOOLONG)));
    let o = fs.open(R, &ok, &ow()).unwrap();
    fs.put_ref(o.ino);
    assert_eq!(fs.resolve(R, &ok, true), Ok(o.ino));
    // Whole path length: 4095 bytes is OK, 4096 is not.
    let mut p = Vec::new();
    while p.len() < 4095 {
        p.extend_from_slice(b"./");
    }
    p.truncate(4094);
    p.push(b'.');
    assert_eq!(p.len(), 4095);
    assert_eq!(fs.resolve(R, &p, true), Ok(R));
    p.push(b'/');
    assert_eq!(fs.resolve(R, &p, true), Err(err(ENAMETOOLONG)));
    let long_target = vec![b'a'; 4096];
    assert_eq!(fs.symlink(&long_target, R, b"/l"), Err(err(ENAMETOOLONG)));
}

#[test]
fn symlinks() {
    let mut fs = setup();
    let b = fs.resolve(R, b"/a/b", true).unwrap();
    let f = fs.resolve(R, b"/a/b/f", true).unwrap();
    let la = fs.symlink(b"/a/b", R, b"/abs").unwrap();
    fs.symlink(b"b/f", R, b"/a/rel").unwrap();
    fs.symlink(b"../top", R, b"/a/b/up").unwrap();
    fs.symlink(b"abs", R, b"/chain").unwrap();
    fs.symlink(b"/", R, b"/rootlink").unwrap();
    assert_eq!(fs.resolve(R, b"/abs", true), Ok(b));
    assert_eq!(fs.resolve(R, b"/abs", false), Ok(la));
    assert_eq!(
        fs.resolve(R, b"/abs/", false),
        Ok(b),
        "trailing slash follows"
    );
    assert_eq!(fs.resolve(R, b"/abs/f", false), Ok(f));
    assert_eq!(fs.resolve(R, b"/a/rel", true), Ok(f));
    assert_eq!(
        fs.resolve(R, b"/a/b/up", true),
        fs.resolve(R, b"/a/top", true).or(Err(err(ENOENT)))
    );
    assert_eq!(fs.resolve(R, b"/a/b/up", true), Err(err(ENOENT)));
    fs.symlink(b"../../top", R, b"/a/b/up2").unwrap();
    assert_eq!(
        fs.resolve(R, b"/a/b/up2", true),
        fs.resolve(R, b"/top", true)
    );
    assert_eq!(fs.resolve(R, b"/chain/f", true), Ok(f));
    assert_eq!(fs.resolve(R, b"/chain/c/..", true), Ok(b));
    assert_eq!(fs.resolve(R, b"/rootlink/a/b/f", true), Ok(f));
    assert_eq!(fs.resolve(R, b"/rootlink", true), Ok(R));
    // A link to a file with a trailing slash.
    assert_eq!(fs.resolve(R, b"/a/rel/", true), Err(err(ENOTDIR)));
    assert_eq!(fs.resolve(R, b"/a/rel/", false), Err(err(ENOTDIR)));
    // Dangling.
    fs.symlink(b"/missing", R, b"/dangle").unwrap();
    assert_eq!(fs.resolve(R, b"/dangle", true), Err(err(ENOENT)));
    assert!(fs.resolve(R, b"/dangle", false).is_ok());
    assert_eq!(fs.resolve(R, b"/dangle/x", false), Err(err(ENOENT)));
    // readlink.
    assert_eq!(fs.readlink(R, b"/a/rel").unwrap(), b"b/f");
    assert_eq!(fs.readlink(R, b"/top"), Err(err(EINVAL)));
    assert_eq!(fs.readlink(R, b"/abs/"), Err(err(EINVAL)));
    assert_eq!(fs.readlink(R, b"/nope"), Err(err(ENOENT)));
    assert_eq!(fs.stat(la).unwrap().size, 4);
    assert_eq!(fs.stat(la).unwrap().mode, patina_abi::fs::S_IFLNK | 0o777);
    assert_eq!(fs.symlink(b"", R, b"/empty"), Err(err(ENOENT)));
    assert_eq!(fs.symlink(b"x", R, b"/abs"), Err(err(EEXIST)));
    assert_eq!(fs.symlink(b"x", R, b"/top/"), Err(err(EEXIST)));
    assert_eq!(fs.symlink(b"x", R, b"/newl/"), Err(err(ENOENT)));
    assert_eq!(fs.symlink(b"x", R, b"/a/."), Err(err(EEXIST)));
}

#[test]
fn symlink_loops() {
    let mut fs = setup();
    fs.symlink(b"self", R, b"/self").unwrap();
    assert_eq!(fs.resolve(R, b"/self", true), Err(err(ELOOP)));
    assert_eq!(fs.resolve(R, b"/self/x", true), Err(err(ELOOP)));
    assert!(fs.resolve(R, b"/self", false).is_ok());
    fs.symlink(b"/p2", R, b"/p1").unwrap();
    fs.symlink(b"/p1", R, b"/p2").unwrap();
    assert_eq!(fs.resolve(R, b"/p1", true), Err(err(ELOOP)));
    // A chain of exactly 40 links resolves; 41 is ELOOP.
    fs.mkdir(R, b"/ch", 0o755).unwrap();
    put(&mut fs, "/ch/l0", b"end");
    for i in 1..=41 {
        let t = std::format!("l{}", i - 1);
        let n = std::format!("/ch/l{i}");
        fs.symlink(t.as_bytes(), R, n.as_bytes()).unwrap();
    }
    assert_eq!(get(&mut fs, "/ch/l40").unwrap(), b"end");
    assert_eq!(get(&mut fs, "/ch/l41"), Err(err(ELOOP)));
    // The budget is shared between intermediate and final links.
    fs.symlink(b"ch", R, b"/chl").unwrap();
    assert_eq!(get(&mut fs, "/chl/l39").unwrap(), b"end");
    assert_eq!(get(&mut fs, "/chl/l40"), Err(err(ELOOP)));
}

#[test]
fn open_symlink_flags() {
    let mut fs = setup();
    let f = fs.resolve(R, b"/top", true).unwrap();
    let l = fs.symlink(b"/top", R, b"/lt").unwrap();
    let nofollow = OpenOptions {
        nofollow: true,
        ..rd()
    };
    assert_eq!(fs.open(R, b"/lt", &nofollow), Err(err(ELOOP)));
    // O_NOFOLLOW only affects the last component.
    fs.symlink(b"/a", R, b"/la").unwrap();
    let o = fs.open(R, b"/la/b/f", &nofollow).unwrap();
    fs.put_ref(o.ino);
    // O_PATH|O_NOFOLLOW opens the link itself.
    let o = fs
        .open(
            R,
            b"/lt",
            &OpenOptions {
                path: true,
                ..nofollow
            },
        )
        .unwrap();
    assert_eq!((o.ino, o.ftype), (l, FileType::Symlink));
    fs.put_ref(o.ino);
    // O_NOFOLLOW|O_DIRECTORY on a symlink: ENOTDIR.
    assert_eq!(
        fs.open(
            R,
            b"/la",
            &OpenOptions {
                directory: true,
                ..nofollow
            }
        ),
        Err(err(ENOTDIR))
    );
    // O_CREAT through an existing link opens the target.
    let o = fs.open(R, b"/lt", &ow()).unwrap();
    assert_eq!(o.ino, f);
    assert!(!o.created);
    fs.put_ref(o.ino);
    // O_CREAT|O_NOFOLLOW on a link: ELOOP; O_CREAT|O_EXCL: EEXIST.
    assert_eq!(
        fs.open(
            R,
            b"/lt",
            &OpenOptions {
                nofollow: true,
                ..ow()
            }
        ),
        Err(err(ELOOP))
    );
    assert_eq!(
        fs.open(R, b"/lt", &OpenOptions { excl: true, ..ow() }),
        Err(err(EEXIST))
    );
    // O_CREAT through a dangling link creates the target.
    fs.symlink(b"a/b/created", R, b"/dl").unwrap();
    let o = fs.open(R, b"/dl", &ow()).unwrap();
    assert!(o.created);
    assert_eq!(fs.resolve(R, b"/a/b/created", true), Ok(o.ino));
    fs.put_ref(o.ino);
    // ... unless the dangling target's directory is missing.
    fs.symlink(b"/nodir/x", R, b"/dl2").unwrap();
    assert_eq!(fs.open(R, b"/dl2", &ow()), Err(err(ENOENT)));
    // A dangling link with O_EXCL is EEXIST, not a create.
    fs.symlink(b"/a/never", R, b"/dl3").unwrap();
    assert_eq!(
        fs.open(R, b"/dl3", &OpenOptions { excl: true, ..ow() }),
        Err(err(EEXIST))
    );
    assert_eq!(fs.resolve(R, b"/a/never", true), Err(err(ENOENT)));
    // Loop with O_CREAT.
    fs.symlink(b"loop", R, b"/loop").unwrap();
    assert_eq!(fs.open(R, b"/loop", &ow()), Err(err(ELOOP)));
    assert_eq!(fs.open(R, b"/loop", &rd()), Err(err(ELOOP)));
}

#[test]
fn getcwd_paths() {
    let mut fs = setup();
    let c = fs.resolve(R, b"/a/b/c", true).unwrap();
    assert_eq!(fs.getcwd(c).unwrap(), b"/a/b/c");
    fs.rename(R, b"/a/b", R, b"/moved", 0).unwrap();
    assert_eq!(fs.getcwd(c).unwrap(), b"/moved/c");
    let f = fs.resolve(R, b"/top", true).unwrap();
    assert_eq!(fs.getcwd(f), Err(err(ENOTDIR)));
}

#[test]
fn removed_cwd() {
    let mut fs = setup();
    let c = fs.resolve(R, b"/a/b/c", true).unwrap();
    fs.get_ref(c).unwrap(); // cwd
    fs.rmdir(R, b"/a/b/c").unwrap();
    assert!(fs.is_removed_dir(c));
    assert_eq!(fs.getcwd(c), Err(err(ENOENT)));
    // stat(".") works with nlink 0; lookups and creates fail.
    assert_eq!(fs.resolve(c, b".", true), Ok(c));
    assert_eq!(fs.stat(c).unwrap().nlink, 0);
    assert_eq!(fs.open(c, b"x", &ow()), Err(err(ENOENT)));
    assert_eq!(fs.mkdir(c, b"x", 0o755), Err(err(ENOENT)));
    assert_eq!(fs.symlink(b"t", c, b"x"), Err(err(ENOENT)));
    let mut pos = 0;
    let mut buf = [0u8; 256];
    assert_eq!(fs.getdents64(c, &mut pos, &mut buf), Err(err(ENOENT)));
    // `..` still leads back to the old parent.
    assert_eq!(fs.resolve(c, b"..", true), fs.resolve(R, b"/a/b", true));
    let before = fs.inode_count();
    fs.put_ref(c);
    assert_eq!(fs.inode_count(), before - 1);
}

#[test]
fn lookup_parent_follows_intermediate_links() {
    let mut fs = setup();
    fs.symlink(b"/a/b", R, b"/lb").unwrap();
    fs.symlink(b"lb", R, b"/lb2").unwrap();
    let b = fs.resolve(R, b"/a/b", true).unwrap();
    let p = fs.lookup_parent(R, b"/lb2/f").unwrap();
    assert_eq!(p.dir, b);
    assert_eq!(p.name.as_deref(), Some(&b"f"[..]));
    assert!(!p.trailing);
    // The final component is not followed.
    let p = fs.lookup_parent(R, b"/lb2").unwrap();
    assert_eq!((p.dir, p.name.as_deref()), (R, Some(&b"lb2"[..])));
    let p = fs.lookup_parent(R, b"/a/b/c/").unwrap();
    assert_eq!((p.dir, p.name.as_deref(), p.trailing), (b, Some(&b"c"[..]), true));
    let p = fs.lookup_parent(R, b"/a/b/..").unwrap();
    assert_eq!((p.dir, p.name), (b, None));
    assert_eq!(fs.lookup_parent(R, b""), Err(err(ENOENT)));
    assert_eq!(fs.lookup_parent(R, b"/nope/x"), Err(err(ENOENT)));
    assert_eq!(fs.lookup_parent(R, b"/top/x"), Err(err(ENOTDIR)));
    // Nothing is created.
    assert!(fs.resolve(R, b"/a/b/x", false).is_err());
    let _ = vec![0u8; 0];
}
