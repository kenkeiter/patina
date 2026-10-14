use super::*;
use std::format;
use std::sync::Mutex;
use std::thread;

#[derive(Clone)]
struct Shared(Arc<Mutex<Fs>>);

impl FsRef for Shared {
    fn with<T>(&self, f: impl FnOnce(&mut Fs) -> T) -> T {
        f(&mut self.0.lock().unwrap())
    }
}

#[test]
fn handle_refcounting() {
    let sh = Shared(Arc::new(Mutex::new(new_fs())));
    let ino = sh.with(|fs| put(fs, "/f", b"abc"));
    let h = Handle::new(sh.clone(), ino).unwrap();
    assert_eq!(sh.with(|fs| fs.ref_count(ino)), 1);
    let h2 = h.clone();
    assert_eq!(sh.with(|fs| fs.ref_count(ino)), 2);
    sh.with(|fs| fs.unlink(R, b"/f")).unwrap();
    drop(h);
    assert_eq!(h2.with(read_all).unwrap(), b"abc");
    drop(h2);
    assert!(sh.with(|fs| fs.stat(ino)).is_err());
    // adopt / into_raw round trip with an open() reference.
    let o = sh.with(|fs| fs.open(R, b"/g", &ow())).unwrap();
    let h = Handle::adopt(sh.clone(), o.ino);
    assert_eq!(h.ino(), o.ino);
    let (p, i) = h.into_raw();
    assert_eq!(p.with(|fs| fs.ref_count(i)), 1);
    p.with(|fs| fs.put_ref(i));
    assert_eq!(
        Handle::new(sh.clone(), 12345).map(|h| h.ino()),
        Err(err(ENOENT))
    );
    let _ = std::format!("{:?}", Handle::new(sh, R).unwrap());
}

#[test]
fn concurrent_threads() {
    let sh = Shared(Arc::new(Mutex::new(new_fs())));
    sh.with(|fs| fs.mkdir(R, b"/t", 0o755)).unwrap();
    let mut joins = Vec::new();
    for t in 0..8 {
        let sh = sh.clone();
        joins.push(thread::spawn(move || {
            for i in 0..200 {
                let path = format!("/t/{t}-{i}");
                let o = sh.with(|fs| fs.open(R, path.as_bytes(), &ow())).unwrap();
                let h = Handle::adopt(sh.clone(), o.ino);
                h.with(|fs, ino| fs.append(ino, path.as_bytes())).unwrap();
                // Shared log file appended by everyone.
                let lo = sh.with(|fs| fs.open(R, b"/t/log", &ow())).unwrap();
                let log = Handle::adopt(sh.clone(), lo.ino);
                log.with(|fs, ino| fs.append(ino, b"0123456789")).unwrap();
                if i % 2 == 0 {
                    sh.with(|fs| fs.unlink(R, path.as_bytes())).unwrap();
                }
                let got = h.with(read_all).unwrap();
                assert_eq!(got, path.as_bytes());
                // Rename races with other threads listing the directory.
                if i % 3 == 0 {
                    let _ = sh.with(|fs| {
                        fs.rename(R, path.as_bytes(), R, format!("{path}.r").as_bytes(), 0)
                    });
                }
                sh.with(|fs| {
                    let d = fs.resolve(R, b"/t", true).unwrap();
                    list(fs, d, 512).unwrap();
                });
            }
        }));
    }
    for j in joins {
        j.join().unwrap();
    }
    sh.with(|fs| {
        let log = fs.resolve(R, b"/t/log", true).unwrap();
        assert_eq!(fs.size(log).unwrap(), 8 * 200 * 10);
        let n = fs
            .read_dir(fs.resolve(R, b"/t", true).unwrap())
            .unwrap()
            .len();
        assert_eq!(n, 8 * 100 + 1);
        // All unlinked files were freed once their handles dropped.
        assert_eq!(fs.inode_count(), 2 + n);
    });
}

#[test]
fn fs_is_send() {
    // The kernel keeps one Fs behind a SpinLock shared between CPUs.
    fn assert_send<T: Send>() {}
    assert_send::<Fs>();
}
