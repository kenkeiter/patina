//! Syscall dispatch (design §7).
//!
//! `trap::syscall` → [`dispatch`]: decode number and arguments from the
//! frame, run the handler, write the result back. Handlers return
//! [`SysResult`] (`Ok(value)` or `Err(errno)`); dispatch turns errors into
//! `-errno`. Unknown numbers return `ENOSYS` and are logged once per number.
//! `patina.strace` prints every call with its six raw arguments.
//!
//! Handlers are grouped by area: [`fs`] (fds, paths), [`poll`] (epoll,
//! ppoll), [`mm`] (memory),
//! [`signal`], [`proc`] (ids, limits, exit_group, randomness), [`thread`]
//! (clone, exit, futex, sched_yield), [`time`] (clocks, sleeps).

mod fs;
mod mm;
mod poll;
mod proc;
mod signal;
mod thread;
mod time;

use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::current::{self, nr};
use patina_abi::errno::{ENOSYS, Errno};

use crate::arch::ctx::TrapFrame;
use crate::mm::vm::AddressSpace;
use crate::task;

/// A handler's result: the non-negative return value, or an errno.
pub type SysResult = Result<isize, Errno>;

/// Run `f` on the current process's address space (locked).
pub fn with_space<R>(f: impl FnOnce(&mut AddressSpace) -> R) -> R {
    f(&mut task::process().space.lock())
}

/// `true` the first time it is called for `key` (< 1024).
fn first_time(set: &[AtomicU64; 16], key: usize) -> bool {
    let (w, b) = (key / 64 % 16, key % 64);
    set[w].fetch_or(1 << b, Ordering::Relaxed) & (1 << b) == 0
}

static ENOSYS_SEEN: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

fn enosys(n: usize) -> SysResult {
    if first_time(&ENOSYS_SEEN, n) {
        crate::kprintln!(
            "[patina] ENOSYS {} ({})",
            n,
            current::syscall_name(n).unwrap_or("?")
        );
    }
    Err(Errno(ENOSYS))
}

fn trace(n: usize, a: &[u64; 6], ret: Option<isize>) {
    let tid = task::current().tid;
    let name = current::syscall_name(n).unwrap_or("?");
    match ret {
        None => crate::kprintln!(
            "[strace] {} {}({:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}) = ?",
            tid,
            name,
            a[0],
            a[1],
            a[2],
            a[3],
            a[4],
            a[5]
        ),
        Some(r) => match Errno::from_ret(r) {
            Some(e) => crate::kprintln!(
                "[strace] {} {}({:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}) = -{}",
                tid,
                name,
                a[0],
                a[1],
                a[2],
                a[3],
                a[4],
                a[5],
                e
            ),
            None if !(0..=0xffff).contains(&r) => crate::kprintln!(
                "[strace] {} {}({:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}) = {:#x}",
                tid,
                name,
                a[0],
                a[1],
                a[2],
                a[3],
                a[4],
                a[5],
                r
            ),
            None => crate::kprintln!(
                "[strace] {} {}({:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}) = {}",
                tid,
                name,
                a[0],
                a[1],
                a[2],
                a[3],
                a[4],
                a[5],
                r
            ),
        },
    }
}

/// Handle the syscall in `frame` (called from `trap::syscall`).
pub fn dispatch(frame: &mut TrapFrame) {
    let n = frame.syscall_nr();
    let a = frame.syscall_args();
    let strace = task::process().strace;

    // Calls that do not return to the caller: trace first.
    if n == nr::EXIT || n == nr::EXIT_GROUP {
        if strace {
            trace(n, &a, None);
        }
        if n == nr::EXIT {
            thread::exit(a[0]);
        }
        proc::exit_group(a[0]);
    }
    // rt_sigreturn rebuilds the whole frame (return register included).
    if n == nr::RT_SIGRETURN {
        if strace {
            trace(n, &a, None);
        }
        signal::rt_sigreturn(frame);
        return;
    }

    let r = call(n, &a, frame);
    let ret = match r {
        Ok(v) => v,
        Err(e) => {
            // Internal restart codes are resolved by signal delivery on
            // the way out (`task::signal::deliver`).
            if task::signal::is_restart(e.0) {
                task::current().restart.store(e.0, Ordering::Relaxed);
            }
            e.to_ret()
        }
    };
    if strace {
        trace(n, &a, Some(ret));
    }
    frame.set_syscall_ret(ret as u64);
}

fn call(n: usize, a: &[u64; 6], frame: &mut TrapFrame) -> SysResult {
    let i = |k: usize| a[k] as i32;
    let u = |k: usize| a[k] as usize;
    match n {
        // fs.rs
        nr::READ => fs::read(i(0), u(1), u(2)),
        nr::WRITE => fs::write(i(0), u(1), u(2)),
        nr::OPENAT => fs::openat(i(0), u(1), a[2] as u32),
        nr::CLOSE => fs::close(i(0)),
        nr::FCNTL => fs::fcntl(i(0), i(1), a[2]),
        nr::IOCTL => fs::ioctl(i(0), a[1] as u32, a[2]),
        nr::READV => fs::readv(i(0), u(1), u(2)),
        nr::WRITEV => fs::writev(i(0), u(1), u(2)),
        nr::PREAD64 => fs::pread64(i(0), u(1), u(2), a[3] as i64),
        nr::PWRITE64 => fs::pwrite64(i(0), u(1), u(2), a[3] as i64),
        nr::LSEEK => fs::lseek(i(0), a[1] as i64, i(2)),
        nr::DUP => fs::dup(i(0)),
        nr::DUP3 => fs::dup3(i(0), i(1), a[2] as u32),
        nr::PIPE2 => fs::pipe2(u(0), a[1] as u32),
        nr::EVENTFD2 => fs::eventfd2(a[0] as u32, a[1] as u32),
        #[cfg(target_arch = "x86_64")]
        nr::DUP2 => fs::dup2(i(0), i(1)),
        #[cfg(target_arch = "x86_64")]
        nr::PIPE => fs::pipe2(u(0), 0),
        #[cfg(target_arch = "x86_64")]
        nr::EVENTFD => fs::eventfd2(a[0] as u32, 0),
        nr::READLINKAT => fs::path_enoent(u(1)),
        nr::FACCESSAT | nr::FACCESSAT2 => fs::path_enoent(u(1)),
        nr::NEWFSTATAT => fs::newfstatat(i(0), u(1), u(2), a[3] as u32),
        nr::FSTAT => fs::fstat(i(0), u(1)),
        #[cfg(target_arch = "x86_64")]
        nr::OPEN => fs::openat(patina_abi::fs::AT_FDCWD, u(0), a[1] as u32),
        #[cfg(target_arch = "x86_64")]
        nr::ACCESS | nr::READLINK | nr::STAT | nr::LSTAT => fs::path_enoent(u(0)),

        // poll.rs
        nr::EPOLL_CREATE1 => poll::epoll_create1(a[0] as u32),
        nr::EPOLL_CTL => poll::epoll_ctl(i(0), i(1), i(2), u(3)),
        nr::EPOLL_PWAIT => poll::epoll_pwait(i(0), u(1), i(2), i(3), u(4), u(5)),
        nr::EPOLL_PWAIT2 => poll::epoll_pwait2(i(0), u(1), i(2), u(3), u(4), u(5)),
        nr::PPOLL => poll::ppoll(u(0), u(1), u(2), u(3), u(4)),
        #[cfg(target_arch = "x86_64")]
        nr::EPOLL_CREATE => poll::epoll_create(i(0)),
        #[cfg(target_arch = "x86_64")]
        nr::EPOLL_WAIT => poll::epoll_pwait(i(0), u(1), i(2), i(3), 0, 0),
        #[cfg(target_arch = "x86_64")]
        nr::POLL => poll::poll(u(0), u(1), i(2)),

        // mm.rs
        nr::MMAP => mm::mmap(u(0), u(1), a[2] as u32, a[3] as u32, i(4), a[5]),
        nr::MUNMAP => mm::munmap(u(0), u(1)),
        nr::MPROTECT => mm::mprotect(u(0), u(1), a[2] as u32),
        nr::MADVISE => mm::madvise(u(0), u(1), i(2)),
        nr::MREMAP => mm::mremap(u(0), u(1), u(2), a[3] as u32),
        nr::BRK => mm::brk(u(0)),
        nr::MINCORE => mm::mincore(u(0), u(1), u(2)),

        // signal.rs
        nr::RT_SIGACTION => signal::rt_sigaction(i(0), u(1), u(2), u(3)),
        nr::RT_SIGPROCMASK => signal::rt_sigprocmask(i(0), u(1), u(2), u(3)),
        nr::SIGALTSTACK => signal::sigaltstack(u(0), u(1), frame.sp()),
        nr::KILL => signal::kill(i(0), i(1)),
        nr::TGKILL => signal::tgkill(i(0), i(1), i(2)),
        nr::TKILL => signal::tkill(i(0), i(1)),
        nr::RT_SIGPENDING => signal::rt_sigpending(u(0), u(1)),
        nr::RT_SIGSUSPEND => signal::rt_sigsuspend(u(0), u(1)),
        nr::RESTART_SYSCALL => signal::restart_syscall(),
        #[cfg(target_arch = "x86_64")]
        nr::PAUSE => signal::pause(),
        // Go does not use these; queued-info signals are not supported.
        nr::RT_SIGTIMEDWAIT | nr::RT_SIGQUEUEINFO | nr::RT_TGSIGQUEUEINFO => enosys(n),

        // proc.rs
        nr::GETPID => Ok(task::process().pid as isize),
        nr::GETTID => Ok(task::current().tid as isize),
        nr::GETPPID => Ok(0),
        nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => Ok(0),
        nr::SET_TID_ADDRESS => proc::set_tid_address(a[0]),
        nr::UNAME => proc::uname(u(0)),
        nr::SCHED_GETAFFINITY => proc::sched_getaffinity(i(0), u(1), u(2)),
        nr::SCHED_YIELD => thread::sched_yield(),
        nr::PRLIMIT64 => proc::prlimit64(i(0), a[1] as u32, u(2), u(3)),
        nr::GETRLIMIT => proc::prlimit64(0, a[0] as u32, 0, u(1)),
        nr::SETRLIMIT => proc::prlimit64(0, a[0] as u32, u(1), 0),
        nr::GETRANDOM => proc::getrandom(u(0), u(1), a[2] as u32),
        #[cfg(target_arch = "x86_64")]
        nr::ARCH_PRCTL => proc::arch_prctl(frame, i(0), a[1]),

        // time.rs
        nr::CLOCK_GETTIME => time::clock_gettime(i(0), u(1)),
        nr::CLOCK_GETRES => time::clock_getres(i(0), u(1)),
        nr::GETTIMEOFDAY => time::gettimeofday(u(0)),
        nr::NANOSLEEP => time::nanosleep(u(0), u(1)),
        nr::CLOCK_NANOSLEEP => time::clock_nanosleep(i(0), i(1), u(2), u(3)),

        // thread.rs
        nr::CLONE => thread::clone(frame, a),
        nr::FUTEX => thread::futex(u(0), i(1), a[2] as u32, u(3), a[5] as u32),
        // clone3 → ENOSYS makes glibc/Go fall back to clone.
        // Not implemented, expected, and handled by Go: no log line.
        nr::MEMBARRIER | nr::SET_ROBUST_LIST | nr::RSEQ => Err(Errno(ENOSYS)),
        _ => enosys(n),
    }
}
