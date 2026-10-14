//! `/dev` (design §8): `null zero full random urandom console tty`, and
//! the `fd stdin stdout stderr` symlinks into `/proc/self/fd`.
//!
//! The devices are registered by `rdev` (Linux numbers: mem 1:3 null,
//! 1:5 zero, 1:7 full, 1:8 random, 1:9 urandom; tty 5:0 tty, 5:1
//! console), so device nodes the initrd brings (`mknod` entries) work too.
//! Every device is always ready and accepts `lseek` (to 0, like the mem
//! devices).

use alloc::sync::Arc;

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::{ENOSPC, ENXIO, Errno};
use patina_abi::fs::{S_IFCHR, makedev};
use patina_vfs::{Fs, Ino};

use super::console::Console;
use super::file::{File, FileFlags, PollHook, alloc_ino, anon_stat};
use super::vfs::{self, DevNode};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mem {
    Null,
    Zero,
    Full,
    Random,
}

impl File for Mem {
    fn kind(&self) -> &'static str {
        "chr"
    }

    fn read(&self, buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        match self {
            Mem::Null => Ok(0),
            Mem::Zero | Mem::Full => {
                buf.fill(0);
                Ok(buf.len())
            }
            Mem::Random => {
                crate::random::fill(buf);
                Ok(buf.len())
            }
        }
    }

    fn write(&self, buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        match self {
            Mem::Full => Err(Errno(ENOSPC)),
            // Writing to /dev/random is accepted and discarded (Linux mixes
            // it into the pool without crediting entropy).
            _ => Ok(buf.len()),
        }
    }

    fn poll(&self, _hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        EpollFlags::IN | EpollFlags::RDNORM | EpollFlags::OUT | EpollFlags::WRNORM
    }

    fn stat(&self) -> Result<Stat, Errno> {
        Ok(anon_stat(S_IFCHR | 0o666, alloc_ino()))
    }

    fn pread(&self, buf: &mut [u8], _off: u64) -> Result<usize, Errno> {
        self.read(buf, FileFlags(0))
    }

    fn pwrite(&self, buf: &[u8], _off: u64) -> Result<usize, Errno> {
        self.write(buf, FileFlags(0))
    }

    fn lseek(&self, _off: i64, _whence: i32) -> Result<u64, Errno> {
        Ok(0)
    }
}

impl DevNode for Mem {
    fn open(&self, _ino: Ino, _flags: u32) -> Result<Arc<dyn File>, Errno> {
        Ok(Arc::new(*self))
    }
}

struct ConsoleDev;

impl DevNode for ConsoleDev {
    fn open(&self, _ino: Ino, _flags: u32) -> Result<Arc<dyn File>, Errno> {
        Ok(Arc::new(Console))
    }
}

/// `/dev/tty`: there is no controlling terminal.
struct NoTty;

impl DevNode for NoTty {
    fn open(&self, _ino: Ino, _flags: u32) -> Result<Arc<dyn File>, Errno> {
        Err(Errno(ENXIO))
    }
}

pub const NULL: u64 = makedev(1, 3);
pub const ZERO: u64 = makedev(1, 5);
pub const FULL: u64 = makedev(1, 7);
pub const RANDOM: u64 = makedev(1, 8);
pub const URANDOM: u64 = makedev(1, 9);
pub const TTY: u64 = makedev(5, 0);
pub const CONSOLE: u64 = makedev(5, 1);

/// (name, rdev, mode).
const NODES: &[(&[u8], u64, u32)] = &[
    (b"null", NULL, 0o666),
    (b"zero", ZERO, 0o666),
    (b"full", FULL, 0o666),
    (b"random", RANDOM, 0o666),
    (b"urandom", URANDOM, 0o666),
    (b"tty", TTY, 0o666),
    (b"console", CONSOLE, 0o600),
];

const LINKS: &[(&[u8], &[u8])] = &[
    (b"fd", b"/proc/self/fd"),
    (b"stdin", b"/proc/self/fd/0"),
    (b"stdout", b"/proc/self/fd/1"),
    (b"stderr", b"/proc/self/fd/2"),
];

/// Register the drivers (before any device node is opened).
pub fn register() {
    vfs::register_chrdev(NULL, Arc::new(Mem::Null));
    vfs::register_chrdev(ZERO, Arc::new(Mem::Zero));
    vfs::register_chrdev(FULL, Arc::new(Mem::Full));
    vfs::register_chrdev(RANDOM, Arc::new(Mem::Random));
    vfs::register_chrdev(URANDOM, Arc::new(Mem::Random));
    vfs::register_chrdev(TTY, Arc::new(NoTty));
    vfs::register_chrdev(CONSOLE, Arc::new(ConsoleDev));
}

/// Create the `/dev` nodes the initrd did not supply (called with the
/// filesystem locked, after population). Nodes are bound to their driver
/// by `rdev` when opened.
pub fn populate(fs: &mut Fs) {
    let root = fs.root();
    let Ok(dev) = fs.mkdir_all(root, b"/dev", 0o755) else {
        return;
    };
    for (name, rdev, mode) in NODES {
        let _ = fs.mknod(dev, name, S_IFCHR | mode, *rdev, None);
    }
    for (name, target) in LINKS {
        let _ = fs.symlink(target, dev, name);
    }
    let _ = fs.mkdir(dev, b"shm", 0o1777);
    let _ = fs.mkdir(dev, b"pts", 0o755);
}
