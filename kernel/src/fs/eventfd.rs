//! `eventfd2`: a 64-bit counter (`patina_fsutil::EventCounter`). Reads and
//! writes move exactly 8 bytes (`EINVAL` for shorter buffers); a read
//! returns the counter and zeroes it (or returns 1 and decrements it with
//! `EFD_SEMAPHORE`); `EAGAIN` when the counter is 0 (read) or would pass
//! `u64::MAX - 1` (write). Readable while the counter is non-zero,
//! writable while it is below the maximum.

use alloc::sync::Arc;

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::{EAGAIN, EINVAL, Errno};
use patina_abi::fs::S_IFREG;
use patina_fsutil::{EventCounter, EventError};

use super::file::{File, FileFlags, PollHook, PollList, alloc_ino, anon_stat};
use crate::sync::SpinLock;

struct State {
    ctr: EventCounter,
    hooks: PollList,
}

pub struct EventFd {
    st: SpinLock<State>,
    ino: u64,
}

impl EventFd {
    pub fn new(init: u32, semaphore: bool) -> Arc<EventFd> {
        Arc::new(EventFd {
            st: SpinLock::new(State {
                ctr: EventCounter::new(init as u64, semaphore),
                hooks: PollList::new(),
            }),
            ino: alloc_ino(),
        })
    }
}

fn ready(c: &EventCounter) -> EpollFlags {
    let mut ev = EpollFlags::empty();
    if c.readable() {
        ev |= EpollFlags::IN | EpollFlags::RDNORM;
    }
    if c.writable() {
        ev |= EpollFlags::OUT | EpollFlags::WRNORM;
    }
    ev
}

fn err(e: EventError) -> Errno {
    match e {
        EventError::WouldBlock => Errno(EAGAIN),
        EventError::Invalid => Errno(EINVAL),
    }
}

impl File for EventFd {
    fn kind(&self) -> &'static str {
        "eventfd"
    }

    fn read(&self, buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        if buf.len() < 8 {
            return Err(Errno(EINVAL));
        }
        let (v, n) = {
            let mut s = self.st.lock();
            let v = s.ctr.read().map_err(err)?;
            (v, s.hooks.collect(EpollFlags::OUT | EpollFlags::WRNORM))
        };
        n.fire();
        buf[..8].copy_from_slice(&v.to_ne_bytes());
        Ok(8)
    }

    fn write(&self, buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        let Some(b) = buf.get(..8) else {
            return Err(Errno(EINVAL));
        };
        let v = u64::from_ne_bytes(b.try_into().expect("8 bytes"));
        let n = {
            let mut s = self.st.lock();
            s.ctr.write(v).map_err(err)?;
            s.hooks.collect(EpollFlags::IN | EpollFlags::RDNORM)
        };
        n.fire();
        Ok(8)
    }

    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        let mut s = self.st.lock();
        if let Some(h) = hook {
            s.hooks.add(h);
        }
        ready(&s.ctr)
    }

    // Anonymous inode (Linux: anon_inodefs, mode 0600).
    fn stat(&self) -> Result<Stat, Errno> {
        Ok(anon_stat(S_IFREG | 0o600, self.ino))
    }
}
