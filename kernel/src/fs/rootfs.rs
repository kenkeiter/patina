//! Boot-time construction of the root filesystem: unpack the initrd
//! (zero-copy: file pages borrow the initrd until written), then add what
//! the initrd did not supply — the base directories (`/tmp` 1777, `/root`,
//! `/etc`, `/dev`, `/proc`, `/sys`), `/dev` nodes, procfs/sysfs, and the
//! `/etc` files Go's resolver and `os/user` read. The initrd wins: nothing
//! here replaces an existing node.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use patina_cmdline::Cmdline;
use patina_cpio::Reader;
use patina_vfs::{Fs, OpenOptions, PopulateOptions, PopulateReport, populate_from_cpio_static};

use super::{devfs, procfs, vfs};
use crate::sync::OnceCell;

static HOSTNAME: OnceCell<String> = OnceCell::new();

/// The host name (`ip=` host name field, else `patina`).
pub fn hostname() -> &'static str {
    HOSTNAME.get().map_or("patina", |s| s.as_str())
}

/// Create the filesystem and populate it from `initrd` (if any). Errors
/// are a malformed archive (members before the error are kept).
pub fn init(
    initrd: Option<&'static [u8]>,
    c: &Cmdline,
) -> Result<PopulateReport, patina_cpio::Error> {
    vfs::init(c.epoch_ns.unwrap_or(0));
    devfs::register();
    let host = c
        .ip
        .as_ref()
        .and_then(|ip| ip.hostname.clone())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| String::from("patina"));
    let _ = HOSTNAME.set(host);
    vfs::with(|fs| {
        let rep = match initrd {
            Some(d) => {
                populate_from_cpio_static(fs, Reader::initramfs(d), &PopulateOptions::default())?
            }
            None => PopulateReport::default(),
        };
        let root = fs.root();
        for (dir, mode) in [
            (&b"/tmp"[..], 0o1777),
            (b"/root", 0o700),
            (b"/etc", 0o755),
            (b"/dev", 0o755),
            (b"/proc", 0o555),
            (b"/sys", 0o555),
        ] {
            let _ = fs.mkdir_all(root, dir, mode);
        }
        devfs::populate(fs);
        procfs::populate(fs);
        etc(fs, c);
        Ok(rep)
    })
}

/// Create `path` with `data` unless something is already there.
fn put(fs: &mut Fs, path: &[u8], data: &[u8], mode: u32) {
    let opts = OpenOptions {
        write: true,
        create: true,
        excl: true,
        mode,
        ..Default::default()
    };
    if let Ok(o) = fs.open(fs.root(), path, &opts) {
        let _ = fs.write_at(o.ino, 0, data);
        fs.put_ref(o.ino);
    }
}

fn etc(fs: &mut Fs, c: &Cmdline) {
    let ip = c.ip.as_ref();
    let mut resolv = String::new();
    let dns: Vec<_> = ip.map_or(Vec::new(), |i| i.dns.iter().flatten().collect());
    if dns.is_empty() {
        resolv.push_str("nameserver 10.0.2.3\n");
    }
    for d in dns {
        resolv.push_str(&format!("nameserver {}\n", d));
    }
    put(fs, b"/etc/resolv.conf", resolv.as_bytes(), 0o644);

    let mut hosts = String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    if let Some(addr) = ip.and_then(|i| i.client) {
        hosts.push_str(&format!("{}\t{}\n", addr, hostname()));
    }
    put(fs, b"/etc/hosts", hosts.as_bytes(), 0o644);
    put(fs, b"/etc/hostname", format!("{}\n", hostname()).as_bytes(), 0o644);
    put(fs, b"/etc/nsswitch.conf", b"hosts: files dns\n", 0o644);
    put(fs, b"/etc/passwd", b"root:x:0:0:root:/root:/bin/sh\n", 0o644);
    put(fs, b"/etc/group", b"root:x:0:\n", 0o644);
}
