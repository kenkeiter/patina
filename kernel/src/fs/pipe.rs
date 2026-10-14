//! Pipes (`pipe2`): a 64 KiB byte ring shared by a read end and a write
//! end, each its own open file description.
//!
//! * read: `Ok(0)` at EOF (empty, no writers), `EAGAIN` if empty;
//! * write: `EPIPE` with no readers (the caller queues `SIGPIPE`); writes
//!   of at most `PIPE_BUF` (4096) bytes are atomic (all or `EAGAIN`),
//!   larger ones take what fits;
//! * readiness: read end `IN` with data, `HUP` with no writers; write end
//!   `OUT` with at least `PIPE_BUF` free, `ERR` with no readers.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::{EAGAIN, ENXIO, EPIPE, Errno};
use patina_abi::fs::S_IFIFO;
use patina_abi::misc::FIONREAD;
use patina_fsutil::ByteRing;

use super::file::{File, FileFlags, Notify, PollHook, PollList, PollWaiter, alloc_ino, anon_stat};
use crate::sync::SpinLock;
use crate::task::WakeReason;
use crate::task::signal::ERESTARTSYS;

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

fn new_pipe(readers: usize, writers: usize) -> Arc<Pipe> {
    Arc::new(Pipe {
        st: SpinLock::new(PipeState {
            ring: ByteRing::new(PIPE_SIZE),
            readers,
            writers,
            rd_hooks: PollList::new(),
            wr_hooks: PollList::new(),
        }),
        ino: alloc_ino(),
    })
}

/// A new pipe: (read end, write end).
pub fn new() -> (Arc<PipeReader>, Arc<PipeWriter>) {
    let p = new_pipe(1, 1);
    (Arc::new(PipeReader(p.clone())), Arc::new(PipeWriter(p)))
}

/// A named pipe (FIFO, `mknod S_IFIFO`): the pipe its open ends share.
/// Opening follows Linux `fifo_open`: `O_RDONLY` waits for a writer and
/// `O_WRONLY` for a reader (unless `O_NONBLOCK`: then `O_WRONLY` without a
/// reader is `ENXIO`), `O_RDWR` never waits. When every end is closed the
/// next open starts a fresh, empty pipe.
pub struct Fifo {
    st: SpinLock<FifoState>,
}

struct FifoState {
    pipe: Option<Arc<Pipe>>,
    /// Opens ever made for reading / writing (Linux `r_counter`/`w_counter`):
    /// a blocked opener waits for the other counter to move.
    r_counter: u64,
    w_counter: u64,
    waiters: PollList,
}

/// A FIFO opened `O_RDWR`: one description that is both ends.
struct FifoRdWr {
    r: PipeReader,
    w: PipeWriter,
}

impl Fifo {
    pub fn new() -> Fifo {
        Fifo {
            st: SpinLock::new(FifoState {
                pipe: None,
                r_counter: 0,
                w_counter: 0,
                waiters: PollList::new(),
            }),
        }
    }

    /// Open an end (`read`/`write` from the access mode). May block;
    /// returns `ERESTARTSYS` if a signal arrives while waiting.
    pub fn open(&self, read: bool, write: bool, nonblock: bool) -> Result<Arc<dyn File>, Errno> {
        let (pipe, notify, wait_for_writer, wait_for_reader, seen) = {
            let mut st = self.st.lock();
            let live = st.pipe.as_ref().is_some_and(|p| {
                let s = p.st.lock();
                s.readers + s.writers > 0
            });
            let pipe = match &st.pipe {
                Some(p) if live => p.clone(),
                _ => {
                    let p = new_pipe(0, 0);
                    st.pipe = Some(p.clone());
                    p
                }
            };
            let (readers, writers) = {
                let mut s = pipe.st.lock();
                if write && !read && nonblock && s.readers == 0 {
                    return Err(Errno(ENXIO));
                }
                if read {
                    s.readers += 1;
                }
                if write {
                    s.writers += 1;
                }
                (s.readers, s.writers)
            };
            if read {
                st.r_counter += 1;
            }
            if write {
                st.w_counter += 1;
            }
            let notify = st.waiters.collect(EpollFlags::empty());
            let seen = (st.r_counter, st.w_counter);
            (
                pipe,
                notify,
                read && !write && !nonblock && writers == 0,
                write && !read && readers == 0,
                seen,
            )
        };
        notify.fire();
        // The new end (its count is already taken).
        let file: Arc<dyn File> = match (read, write) {
            (true, true) => Arc::new(FifoRdWr {
                r: PipeReader(pipe.clone()),
                w: PipeWriter(pipe),
            }),
            (true, false) => Arc::new(PipeReader(pipe)),
            _ => Arc::new(PipeWriter(pipe)),
        };
        if wait_for_writer || wait_for_reader {
            loop {
                let w = PollWaiter::new();
                {
                    let mut st = self.st.lock();
                    let moved = if wait_for_writer {
                        st.w_counter != seen.1
                    } else {
                        st.r_counter != seen.0
                    };
                    if moved {
                        break;
                    }
                    let hook: Arc<dyn PollHook> = w.clone();
                    st.waiters.add(&hook);
                }
                if w.wait(None) == WakeReason::Interrupted {
                    // Undo the count taken above.
                    file.release();
                    return Err(Errno(ERESTARTSYS));
                }
            }
        }
        Ok(file)
    }
}

impl Default for Fifo {
    fn default() -> Self {
        Self::new()
    }
}

impl File for FifoRdWr {
    fn kind(&self) -> &'static str {
        "pipe"
    }
    fn read(&self, buf: &mut [u8], flags: FileFlags) -> Result<usize, Errno> {
        self.r.read(buf, flags)
    }
    fn write(&self, buf: &[u8], flags: FileFlags) -> Result<usize, Errno> {
        self.w.write(buf, flags)
    }
    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        // A description that is both ends never sees HUP or ERR.
        (self.r.poll(hook) | self.w.poll(hook)) - EpollFlags::HUP - EpollFlags::ERR
    }
    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        self.r.ioctl(cmd, arg)
    }
    fn stat(&self) -> Result<Stat, Errno> {
        fifo_stat(&self.r.0)
    }
    fn proc_name(&self) -> Vec<u8> {
        self.r.proc_name()
    }
    fn release(&self) {
        self.r.release();
        self.w.release();
    }
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

    fn proc_name(&self) -> Vec<u8> {
        format!("pipe:[{}]", self.0.ino).into_bytes()
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

    fn proc_name(&self) -> Vec<u8> {
        format!("pipe:[{}]", self.0.ino).into_bytes()
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
