//! Pipes (`pipe2`): a 64 KiB byte ring shared by a read end and a write
//! end, each its own open file description.
//!
//! * read: `Ok(0)` at EOF (empty, no writers), `EAGAIN` if empty;
//! * write: `EPIPE` with no readers (the caller queues `SIGPIPE`); writes
//!   of at most `PIPE_BUF` (4096) bytes are atomic (all or `EAGAIN`),
//!   larger ones take what fits;
//! * readiness: read end `IN` with data, `HUP` with no writers; write end
//!   `OUT` with at least `PIPE_BUF` free, `ERR` with no readers.

use alloc::sync::Arc;

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::{EAGAIN, EPIPE, Errno};
use patina_abi::fs::S_IFIFO;
use patina_abi::misc::FIONREAD;
use patina_fsutil::ByteRing;

use super::file::{File, FileFlags, Notify, PollHook, PollList, alloc_ino, anon_stat};
use crate::sync::SpinLock;

pub const PIPE_SIZE: usize = 64 * 1024;
pub const PIPE_BUF: usize = 4096;

struct PipeState {
    ring: ByteRing,
    readers: usize,
    writers: usize,
    /// Hooks registered through the read end / the write end.
    rd_hooks: PollList,
    wr_hooks: PollList,
}

struct Pipe {
    st: SpinLock<PipeState>,
    ino: u64,
}

pub struct PipeReader(Arc<Pipe>);
pub struct PipeWriter(Arc<Pipe>);

/// A new pipe: (read end, write end).
pub fn new() -> (Arc<PipeReader>, Arc<PipeWriter>) {
    let p = Arc::new(Pipe {
        st: SpinLock::new(PipeState {
            ring: ByteRing::new(PIPE_SIZE),
            readers: 1,
            writers: 1,
            rd_hooks: PollList::new(),
            wr_hooks: PollList::new(),
        }),
        ino: alloc_ino(),
    });
    (Arc::new(PipeReader(p.clone())), Arc::new(PipeWriter(p)))
}

fn read_ready(s: &PipeState) -> EpollFlags {
    let mut ev = EpollFlags::empty();
    if !s.ring.is_empty() {
        ev |= EpollFlags::IN | EpollFlags::RDNORM;
    }
    if s.writers == 0 {
        ev |= EpollFlags::HUP;
    }
    ev
}

fn write_ready(s: &PipeState) -> EpollFlags {
    let mut ev = EpollFlags::empty();
    if s.ring.space() >= PIPE_BUF {
        ev |= EpollFlags::OUT | EpollFlags::WRNORM;
    }
    if s.readers == 0 {
        ev |= EpollFlags::ERR;
    }
    ev
}

fn fifo_stat(p: &Pipe) -> Result<Stat, Errno> {
    let mut st = anon_stat(S_IFIFO | 0o600, p.ino);
    st.st_blksize = PIPE_BUF as _;
    Ok(st)
}

impl File for PipeReader {
    fn kind(&self) -> &'static str {
        "pipe"
    }

    fn read(&self, buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        let notify;
        let n = {
            let mut s = self.0.st.lock();
            if s.ring.is_empty() {
                return if s.writers == 0 {
                    Ok(0)
                } else {
                    Err(Errno(EAGAIN))
                };
            }
            let before = s.ring.space();
            let n = s.ring.pop(buf);
            // Only a transition to "writable" matters to writers.
            notify = if before < PIPE_BUF && s.ring.space() >= PIPE_BUF {
                s.wr_hooks.collect(EpollFlags::OUT | EpollFlags::WRNORM)
            } else {
                Notify::NONE
            };
            n
        };
        notify.fire();
        Ok(n)
    }

    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        let mut s = self.0.st.lock();
        if let Some(h) = hook {
            s.rd_hooks.add(h);
        }
        read_ready(&s)
    }

    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        if cmd == FIONREAD {
            let n = self.0.st.lock().ring.len() as i32;
            crate::syscall::with_space(|s| s.write_val(arg as usize, &n))?;
            return Ok(0);
        }
        Err(Errno(patina_abi::errno::ENOTTY))
    }

    fn stat(&self) -> Result<Stat, Errno> {
        fifo_stat(&self.0)
    }

    fn release(&self) {
        let n = {
            let mut s = self.0.st.lock();
            s.readers -= 1;
            if s.readers == 0 {
                s.wr_hooks.collect(EpollFlags::ERR)
            } else {
                Notify::NONE
            }
        };
        n.fire();
    }
}

impl File for PipeWriter {
    fn kind(&self) -> &'static str {
        "pipe"
    }

    fn write(&self, buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        let notify;
        let n = {
            let mut s = self.0.st.lock();
            if s.readers == 0 {
                return Err(Errno(EPIPE));
            }
            let space = s.ring.space();
            if space == 0 || (buf.len() <= PIPE_BUF && space < buf.len()) {
                return Err(Errno(EAGAIN));
            }
            let n = s.ring.push(buf);
            // Every write notifies readers, not only empty → non-empty
            // (Linux 5.15 "pipe: make pipe writes always wake up readers":
            // some epoll ET users rely on it).
            notify = s.rd_hooks.collect(EpollFlags::IN | EpollFlags::RDNORM);
            n
        };
        notify.fire();
        Ok(n)
    }

    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        let mut s = self.0.st.lock();
        if let Some(h) = hook {
            s.wr_hooks.add(h);
        }
        write_ready(&s)
    }

    fn stat(&self) -> Result<Stat, Errno> {
        fifo_stat(&self.0)
    }

    fn release(&self) {
        let n = {
            let mut s = self.0.st.lock();
            s.writers -= 1;
            if s.writers == 0 {
                s.rd_hooks.collect(EpollFlags::HUP)
            } else {
                Notify::NONE
            }
        };
        n.fire();
    }
}
