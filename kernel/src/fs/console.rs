//! The console as fds 0/1/2. stdin is always at end of file (there is no
//! console input path yet); writes go to the kernel console. Always ready.

use alloc::sync::Arc;

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::Errno;
use patina_abi::fs::S_IFCHR;

use super::file::{File, FileFlags, PollHook};

pub struct Console;

impl File for Console {
    fn kind(&self) -> &'static str {
        "console"
    }

    fn proc_name(&self) -> alloc::vec::Vec<u8> {
        b"/dev/console".to_vec()
    }

    fn read(&self, _buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(&self, buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        crate::console::write_bytes(buf);
        Ok(buf.len())
    }

    fn poll(&self, _hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        EpollFlags::IN | EpollFlags::RDNORM | EpollFlags::OUT | EpollFlags::WRNORM
    }

    // Like /dev/console: a character device, makedev(5, 1).
    fn stat(&self) -> Result<Stat, Errno> {
        Ok(Stat {
            st_mode: S_IFCHR | 0o620,
            st_nlink: 1,
            st_rdev: (5 << 8) | 1,
            st_blksize: 1024,
            ..Default::default()
        })
    }
}
