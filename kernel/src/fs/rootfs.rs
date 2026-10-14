//! Boot-time construction of the root filesystem: unpack the initrd
//! (zero-copy: file pages borrow the initrd until written), then add what
//! the initrd did not supply — the base directories (`/tmp` 1777, `/root`,
//! `/etc`, `/dev`, `/proc`, `/sys`), `/dev` nodes, procfs/sysfs, and the
//! `/etc` files Go's resolver and `os/user` read. The initrd wins: nothing
//! here replaces an existing node.

use alloc::format;
use alloc::string::String;

use patina_cmdline::Cmdline;
use patina_cpio::Reader;
use patina_vfs::{Fs, OpenOptions, PopulateOptions, PopulateReport, populate_from_cpio_static};

use super::{devfs, procfs, vfs};
use crate::sync::OnceCell;

static HOSTNAME: OnceCell<String> = OnceCell::new();

/// The host name (`ip=` host name field, else the DHCP server's, else
/// `patina`).
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
    let host =
        c.ip.as_ref()
            .and_then(|ip| ip.hostname.clone())
            .filter(|h| !h.is_empty())
            .or_else(|| crate::net::effective_config().hostname)
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
        etc(fs);
        initial_cwd(fs, c);
        Ok(rep)
    })
}

/// `patina.cwd=<dir>` (percent-decoded): the program's initial working
/// directory, like a container's WORKDIR. A path that is not an existing
/// directory is reported and ignored (the cwd stays `/`).
fn initial_cwd(fs: &mut Fs, c: &Cmdline) {
    let Some(v) = c
        .unknown
        .iter()
        .rev()
        .find(|p| p.key == "patina.cwd")
        .and_then(|p| p.value.as_deref())
    else {
        return;
    };
    let path = patina_cmdline::percent_decode_bytes(v);
    let root = fs.root();
    match fs.resolve(root, &path, true) {
        Ok(ino) if fs.file_type(ino) == Ok(patina_vfs::FileType::Directory) => {
            vfs::set_initial_cwd(fs, ino);
            if let Ok(p) = fs.getcwd(ino) {
                procfs::set_cwd_link(fs, &p);
            }
        }
        _ => crate::kprintln!("[patina] patina.cwd={}: not a directory, using /", v),
    }
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

/// Generated `/etc` files (D54, D72). `/etc/resolv.conf` and `/etc/hosts`
/// come from the stack's effective configuration (`net::effective_config`:
/// a DHCP lease, else `ip=`), which is final by now (`net::wait_dhcp` ran
/// before the loader). DNS falls back to 10.0.2.3 (QEMU slirp's resolver).
/// Later lease changes do not rewrite them.
fn etc(fs: &mut Fs) {
    let cfg = crate::net::effective_config();
    let mut resolv = String::new();
    if let Some(d) = &cfg.domain {
        resolv.push_str(&format!("search {}\n", d));
    }
    if cfg.dns.is_empty() {
        resolv.push_str("nameserver 10.0.2.3\n");
    }
    for d in &cfg.dns {
        resolv.push_str(&format!("nameserver {}\n", d));
    }
    put(fs, b"/etc/resolv.conf", resolv.as_bytes(), 0o644);

    // Without an address the host name resolves to 127.0.0.1 (the only
    // 127/8 address the stack has, D68).
    let mut hosts = match cfg.addr {
        Some(_) => String::from("127.0.0.1\tlocalhost\n"),
        None => format!("127.0.0.1\tlocalhost {}\n", hostname()),
    };
    hosts.push_str("::1\tlocalhost ip6-localhost ip6-loopback\n");
    if let Some((addr, _)) = cfg.addr {
        match &cfg.domain {
            Some(d) => hosts.push_str(&format!("{}\t{}.{} {}\n", addr, hostname(), d, hostname())),
            None => hosts.push_str(&format!("{}\t{}\n", addr, hostname())),
        }
    }
    put(fs, b"/etc/hosts", hosts.as_bytes(), 0o644);
    put(
        fs,
        b"/etc/hostname",
        format!("{}\n", hostname()).as_bytes(),
        0o644,
    );
    put(fs, b"/etc/nsswitch.conf", b"hosts: files dns\n", 0o644);
    put(
        fs,
        b"/etc/passwd",
        b"root:x:0:0:root:/root:/bin/sh\n",
        0o644,
    );
    put(fs, b"/etc/group", b"root:x:0:\n", 0o644);
}
