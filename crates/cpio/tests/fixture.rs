//! Reads an archive produced by a real cpio implementation (bsdcpio 3.5.3 /
//! libarchive 3.7.4, `--format newc`; regenerate with
//! `tests/fixtures/mkfixture.sh`).

use patina_cpio::{FileType, Format, Reader};

const SAMPLE: &[u8] = include_bytes!("fixtures/sample.newc.cpio");

/// 2025-01-02 03:04:05 UTC, set by mkfixture.sh.
const MTIME: u32 = 1_735_787_045;

#[test]
fn bsdcpio_newc_fixture() {
    let entries: Vec<_> = Reader::new(SAMPLE).collect::<Result<_, _>>().unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            ".",
            "./app",
            "./app.hard",
            "./dir",
            "./dir/five",
            "./dir/sub",
            "./dir/sub/empty",
            "./empty-dir",
            "./etc",
            "./etc/hosts",
            "./link-to-app",
            "./odd-name-len",
        ]
    );
    let get = |p: &str| *entries.iter().find(|e| e.path() == p.as_bytes()).unwrap();

    for e in &entries {
        assert_eq!(e.format, Format::Newc);
        assert_eq!(e.mtime, MTIME, "{:?}", e.name_str());
    }

    let root = get("");
    assert_eq!(root.file_type(), FileType::Directory);

    // Hard link pair: same inode, data only on the last link.
    let app = get("app");
    let hard = get("app.hard");
    assert_eq!(app.file_type(), FileType::Regular);
    assert_eq!(app.perm(), 0o755);
    assert_eq!((app.nlink, hard.nlink), (2, 2));
    assert_eq!((app.dev(), app.ino), (hard.dev(), hard.ino));
    assert_eq!(app.data, b"");
    assert_eq!(hard.data, b"hello from patina\n");

    let five = get("dir/five");
    assert_eq!(five.data, b"abcde");
    assert_eq!(five.perm(), 0o600);

    assert_eq!(get("dir/sub/empty").data, b"");
    assert_eq!(get("dir/sub").file_type(), FileType::Directory);
    assert_eq!(get("empty-dir").perm(), 0o700);
    assert_eq!(get("etc/hosts").data, b"127.0.0.1 localhost\n");
    assert_eq!(get("odd-name-len").data, b"x");

    let link = get("link-to-app");
    assert_eq!(link.file_type(), FileType::Symlink);
    assert_eq!(link.symlink_target(), Some(&b"/app"[..]));

    // bsdcpio pads the archive to a 512-byte block; initramfs mode handles
    // the trailing NULs too.
    assert_eq!(SAMPLE.len() % 512, 0);
    assert_eq!(Reader::initramfs(SAMPLE).count(), entries.len());
}
