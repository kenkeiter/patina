extern crate std;
use std::vec;
use std::vec::Vec;

use super::*;

fn meta(ino: u32, mode: u32) -> EntryMeta {
    EntryMeta {
        ino,
        mode,
        nlink: 1,
        mtime: 1_700_000_000,
        uid: 0,
        gid: 0,
        ..Default::default()
    }
}

fn build(format: Format, entries: &[(EntryMeta, &[u8], &[u8])]) -> Vec<u8> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut w = Writer::new(&mut buf, format);
    for (m, n, d) in entries {
        w.entry(m, n, d).unwrap();
    }
    let n = w.finish().unwrap();
    buf.truncate(n);
    buf
}

fn sample(format: Format) -> Vec<u8> {
    build(
        format,
        &[
            (meta(1, S_IFDIR | 0o755), b".", b""),
            (meta(2, S_IFREG | 0o755), b"app", b"\x7fELF fake"),
            (meta(3, S_IFDIR | 0o755), b"etc", b""),
            (
                meta(4, S_IFREG | 0o644),
                b"etc/hosts",
                b"127.0.0.1 localhost\n",
            ),
            (meta(5, S_IFLNK | 0o777), b"bin", b"/app"),
            (
                EntryMeta {
                    rdev_major: 1,
                    rdev_minor: 3,
                    ..meta(6, S_IFCHR | 0o666)
                },
                b"dev/null",
                b"",
            ),
            (meta(7, S_IFIFO | 0o600), b"fifo", b""),
            (meta(8, S_IFREG | 0o600), b"abcd", b"1"),
            (meta(9, S_IFREG | 0o600), b"abcde", b"12"),
            (meta(10, S_IFREG | 0o600), b"abcdef", b"123"),
            (meta(11, S_IFREG | 0o600), b"empty", b""),
        ],
    )
}

fn collect(r: Reader<'_>) -> Result<Vec<Entry<'_>>, Error> {
    r.collect()
}

#[test]
fn reads_newc() {
    let a = sample(Format::Newc);
    assert_eq!(a.len() % 4, 0);
    let es = collect(Reader::new(&a)).unwrap();
    assert_eq!(es.len(), 11);
    assert_eq!(es[0].file_type(), FileType::Directory);
    assert_eq!(es[0].path(), b"");
    assert_eq!(es[1].name, b"app");
    assert_eq!(es[1].data, b"\x7fELF fake");
    assert_eq!(es[1].perm(), 0o755);
    assert_eq!(es[1].mtime, 1_700_000_000);
    assert_eq!(es[1].format, Format::Newc);
    assert_eq!(es[3].name_str(), Some("etc/hosts"));
    assert_eq!(es[3].data, b"127.0.0.1 localhost\n");
    assert_eq!(es[4].file_type(), FileType::Symlink);
    assert_eq!(es[4].symlink_target(), Some(&b"/app"[..]));
    assert_eq!(es[3].symlink_target(), None);
    assert_eq!(es[5].file_type(), FileType::CharDevice);
    assert_eq!((es[5].rdev_major, es[5].rdev_minor), (1, 3));
    assert_eq!(es[6].file_type(), FileType::Fifo);
    // Name/data padding variants.
    assert_eq!(es[7].data, b"1");
    assert_eq!(es[8].data, b"12");
    assert_eq!(es[9].data, b"123");
    assert_eq!(es[10].data, b"");
    assert!(es.iter().all(|e| e.offset % 4 == 0));
}

#[test]
fn reads_crc_and_verifies() {
    let a = sample(Format::Crc);
    let es = collect(Reader::new(&a)).unwrap();
    assert_eq!(es.len(), 11);
    assert_eq!(es[1].format, Format::Crc);
    let sum: u32 = b"\x7fELF fake".iter().map(|&b| b as u32).sum();
    assert_eq!(es[1].check, sum);

    // Corrupt one data byte of "app".
    let mut bad = a.clone();
    // Data starts after the header and "app\0", aligned to 4.
    let off = es[1].offset + HEADER_LEN + 4;
    let data_off = align4(off).unwrap();
    bad[data_off] ^= 1;
    let r: Vec<_> = Reader::new(&bad).collect();
    assert_eq!(r.len(), 2);
    assert_eq!(
        r[1],
        Err(Error {
            kind: ErrorKind::ChecksumMismatch,
            offset: es[1].offset
        })
    );
    // With verification disabled it reads through.
    assert_eq!(
        collect(Reader::new(&bad).verify_crc(false)).unwrap().len(),
        11
    );
}

#[test]
fn path_normalisation() {
    let a = build(
        Format::Newc,
        &[
            (meta(1, S_IFDIR), b"./", b""),
            (meta(2, S_IFREG), b"./app", b""),
            (meta(3, S_IFREG), b"/etc/x", b""),
            (meta(4, S_IFREG), b"././a/./b", b""),
            (meta(5, S_IFREG), b"..", b""),
        ],
    );
    let es = collect(Reader::new(&a)).unwrap();
    let paths: Vec<&[u8]> = es.iter().map(|e| e.path()).collect();
    assert_eq!(paths, vec![&b""[..], b"app", b"etc/x", b"a/./b", b".."]);
}

#[test]
fn empty_and_trailer_only() {
    let a = build(Format::Newc, &[]);
    assert_eq!(collect(Reader::new(&a)).unwrap().len(), 0);
    assert_eq!(collect(Reader::initramfs(&[])).unwrap().len(), 0);
    assert_eq!(collect(Reader::initramfs(&[0; 512])).unwrap().len(), 0);
    assert_eq!(
        collect(Reader::new(&[])).unwrap_err(),
        Error {
            kind: ErrorKind::MissingTrailer,
            offset: 0
        }
    );
}

#[test]
fn stops_at_trailer_and_concatenates() {
    let a = build(Format::Newc, &[(meta(1, S_IFREG), b"one", b"1")]);
    let b = build(Format::Crc, &[(meta(2, S_IFREG), b"two", b"22")]);
    let mut cat = a.clone();
    cat.extend_from_slice(&[0u8; 512]);
    cat.extend_from_slice(&b);
    cat.extend_from_slice(&[0u8; 3]);

    let single = collect(Reader::new(&cat)).unwrap();
    assert_eq!(single.len(), 1);
    let all = collect(Reader::initramfs(&cat)).unwrap();
    assert_eq!(
        all.iter().map(|e| e.name).collect::<Vec<_>>(),
        vec![&b"one"[..], b"two"]
    );

    // Second archive missing its trailer.
    let mut cut = a.clone();
    cut.extend_from_slice(&b[..b.len() - 124]);
    let r: Vec<_> = Reader::initramfs(&cut).collect();
    assert_eq!(r.len(), 3);
    assert_eq!(r[2].unwrap_err().kind, ErrorKind::MissingTrailer);
}

#[test]
fn malformed() {
    let a = sample(Format::Newc);
    // Bad magic (e.g. gzip-compressed initramfs).
    let r: Vec<_> = Reader::new(b"\x1f\x8b\x08\x00garbage").collect();
    assert_eq!(
        r,
        vec![Err(Error {
            kind: ErrorKind::BadMagic,
            offset: 0
        })]
    );
    // Old odc format is not supported.
    let mut odc = a.clone();
    odc[..6].copy_from_slice(b"070707");
    assert_eq!(
        Reader::new(&odc).next().unwrap().unwrap_err().kind,
        ErrorKind::BadMagic
    );
    // Bad hex digit in mode.
    let mut bad = a.clone();
    bad[14] = b'g';
    assert_eq!(
        Reader::new(&bad).next().unwrap(),
        Err(Error {
            kind: ErrorKind::BadHex,
            offset: 14
        })
    );
    // namesize 0.
    let mut bad = a.clone();
    bad[94..102].copy_from_slice(b"00000000");
    assert_eq!(
        Reader::new(&bad).next().unwrap().unwrap_err().kind,
        ErrorKind::BadName
    );
    // Name not NUL-terminated: namesize 1 shorter than real.
    let mut bad = a.clone();
    bad[94..102].copy_from_slice(b"00000001");
    assert_eq!(
        Reader::new(&bad).next().unwrap().unwrap_err().kind,
        ErrorKind::BadName
    );
    // Huge filesize.
    let mut bad = a.clone();
    bad[54..62].copy_from_slice(b"ffffffff");
    assert_eq!(
        Reader::new(&bad).next().unwrap().unwrap_err().kind,
        ErrorKind::Truncated
    );
    // Huge namesize.
    let mut bad = a.clone();
    bad[94..102].copy_from_slice(b"FFFFFFFF");
    assert_eq!(
        Reader::new(&bad).next().unwrap().unwrap_err().kind,
        ErrorKind::Truncated
    );
    // Errors end iteration.
    let mut r = Reader::new(&bad);
    assert!(r.next().unwrap().is_err());
    assert!(r.next().is_none());
}

#[test]
fn every_truncation() {
    let a = sample(Format::Crc);
    for cut in 0..a.len() {
        let mut n = 0;
        let mut err = false;
        for r in Reader::new(&a[..cut]) {
            match r {
                Ok(_) => n += 1,
                Err(_) => err = true,
            }
        }
        // Either a clean prefix ending in an error, or (only when the cut
        // just drops trailing trailer padding) a complete read.
        assert!(err || n == 11, "cut {cut}: n={n}");
        let _ = collect(Reader::initramfs(&a[..cut]));
    }
}

#[test]
fn random_corruption_never_panics() {
    let a = sample(Format::Crc);
    let mut state = 0xdead_beef_cafe_f00du64;
    let mut rnd = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let mut b = a.clone();
        for _ in 0..1 + rnd() % 4 {
            let i = (rnd() as usize) % b.len();
            b[i] = match rnd() % 3 {
                0 => rnd() as u8,
                1 => b"0123456789abcdefF"[(rnd() % 17) as usize],
                _ => 0,
            };
        }
        let cut = (rnd() as usize) % (b.len() + 1);
        for e in Reader::initramfs(&b[..cut])
            .chain(Reader::new(&b))
            .flatten()
        {
            let _ = (e.path(), e.file_type(), e.symlink_target(), e.name_str());
        }
    }
}

#[test]
fn writer_overflow() {
    let mut small = [0u8; 100];
    let mut w = Writer::new(&mut small, Format::Newc);
    assert!(w.entry(&meta(1, S_IFREG), b"x", b"").is_none());
}
