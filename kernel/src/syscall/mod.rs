//! Syscall dispatch (design §7).
//!
//! `trap::syscall` → [`dispatch`]: decode number and arguments from the
//! frame, run the handler, write the result back. Handlers return
//! [`SysResult`] (`Ok(value)` or `Err(errno)`); dispatch turns errors into
//! `-errno`. Unknown numbers return `ENOSYS` and are logged once per number.
//! `patina.strace` prints every call with its six raw arguments.
//!
//! Handlers are grouped by area: [`fs`] (fds), [`path`] (paths, cwd,
//! metadata), [`poll`] (epoll, ppoll), [`mm`] (memory),
//! [`net`] (sockets), [`signal`], [`proc`] (ids, limits, exit_group, randomness, Tier-3
//! stubs), [`thread`] (clone, exit, futex, sched_yield), [`time`] (clocks,
//! sleeps).

mod fs;
mod mm;
mod net;
mod path;
mod poll;
mod proc;
mod signal;
mod thread;
mod time;

use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::current::{self, nr};
use patina_abi::errno::{ECHILD, EINVAL, ENOSYS, EOPNOTSUPP, EPERM, Errno};
#[cfg(target_arch = "x86_64")]
use patina_abi::fs::AT_FDCWD;

use crate::arch::ctx::TrapFrame;
use crate::mm::vm::AddressSpace;
use crate::task;

/// A handler's result: the non-negative return value, or an errno.
pub type SysResult = Result<isize, Errno>;

/// fd-table access for kernel code outside `syscall` (AF_UNIX SCM_RIGHTS).
pub use fs::{get as fd_get, install as fd_install};

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
        crate::ctl::log_warn(format_args!(
            "ENOSYS {} ({})",
            n,
            current::syscall_name(n).unwrap_or("?")
        ));
    }
    Err(Errno(ENOSYS))
}

/// `<tid>` in strace lines, `<tid>@<cpu>` with more than one CPU.
struct TraceTid(u32);

impl core::fmt::Display for TraceTid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if task::sched::online_cpus() > 1 {
            write!(f, "{}@{}", self.0, crate::arch::cpu::id())
        } else {
            write!(f, "{}", self.0)
        }
    }
}

fn trace(n: usize, a: &[u64; 6], ret: Option<isize>) {
    let tid = TraceTid(task::current().tid);
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

/// Index of the path argument of a path syscall (for strace).
fn path_arg(n: usize) -> Option<usize> {
    match n {
        nr::OPENAT
        | nr::MKDIRAT
        | nr::MKNODAT
        | nr::UNLINKAT
        | nr::READLINKAT
        | nr::NEWFSTATAT
        | nr::STATX
        | nr::FACCESSAT
        | nr::FACCESSAT2
        | nr::FCHMODAT
        | nr::FCHOWNAT
        | nr::UTIMENSAT
        | nr::RENAMEAT
        | nr::RENAMEAT2
        | nr::LINKAT => Some(1),
        nr::SYMLINKAT => Some(2),
        nr::CHDIR | nr::TRUNCATE | nr::STATFS | nr::UMOUNT2 => Some(0),
        #[cfg(target_arch = "x86_64")]
        nr::OPEN
        | nr::STAT
        | nr::LSTAT
        | nr::ACCESS
        | nr::READLINK
        | nr::MKDIR
        | nr::RMDIR
        | nr::UNLINK
        | nr::RENAME
        | nr::CREAT
        | nr::CHMOD
        | nr::UTIME
        | nr::UTIMES => Some(0),
        _ => None,
    }
}

/// strace: show the path argument of a path syscall before the call.
fn trace_path(n: usize, a: &[u64; 6]) {
    if let Some(i) = path_arg(n) {
        if a[i] == 0 {
            return;
        }
        if let Ok(p) = with_space(|s| s.read_cstr(a[i] as usize, 4095)) {
            crate::kprintln!(
                "[strace] {} {} path {:?}",
                TraceTid(task::current().tid),
                current::syscall_name(n).unwrap_or("?"),
                core::str::from_utf8(&p).unwrap_or("<non-utf8>")
            );
        }
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

    if strace {
        trace_path(n, &a);
        if let Some(addr) = net::trace_addr(n, &a) {
            crate::kprintln!(
                "[strace] {} {} addr {}",
                TraceTid(task::current().tid),
                current::syscall_name(n).unwrap_or("?"),
                addr
            );
        }
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
        nr::FSTAT => fs::fstat(i(0), u(1)),
        nr::GETDENTS64 => fs::getdents64(i(0), u(1), u(2)),
        nr::FTRUNCATE => fs::ftruncate(i(0), a[1] as i64),
        nr::FALLOCATE => fs::fallocate(i(0), i(1), a[2] as i64, a[3] as i64),
        nr::FLOCK => fs::flock(i(0), i(1)),
        nr::FADVISE64 => fs::fadvise64(i(0), i(3)),
        nr::CLOSE_RANGE => fs::close_range(a[0] as u32, a[1] as u32, a[2] as u32),
        nr::FSYNC | nr::FDATASYNC | nr::SYNCFS => path::fd_noop(i(0)),
        nr::SYNC_FILE_RANGE => path::fd_noop(i(0)),
        nr::SYNC => Ok(0),
        nr::READAHEAD => fs::fadvise64(i(0), 0),
        nr::SENDFILE => fs::sendfile(i(0), i(1), u(2), u(3)),
        // Go falls back to read/write on ENOSYS; not worth a log line.
        nr::COPY_FILE_RANGE | nr::OPENAT2 => Err(Errno(ENOSYS)),
        // Go's splice path (io.Copy between TCP conns, poll.Splice) falls
        // back only on EINVAL ("splice unsupported for this fd type");
        // ENOSYS would fail the copy.
        nr::SPLICE | nr::TEE => Err(Errno(EINVAL)),
        #[cfg(target_arch = "x86_64")]
        nr::DUP2 => fs::dup2(i(0), i(1)),
        #[cfg(target_arch = "x86_64")]
        nr::PIPE => fs::pipe2(u(0), 0),
        #[cfg(target_arch = "x86_64")]
        nr::EVENTFD => fs::eventfd2(a[0] as u32, 0),

        // path.rs
        nr::OPENAT => path::openat(i(0), u(1), a[2] as u32, a[3] as u32),
        nr::MKDIRAT => path::mkdirat(i(0), u(1), a[2] as u32),
        nr::MKNODAT => path::mknodat(i(0), u(1), a[2] as u32, a[3]),
        nr::UNLINKAT => path::unlinkat(i(0), u(1), a[2] as u32),
        nr::RENAMEAT => path::renameat2(i(0), u(1), i(2), u(3), 0),
        nr::RENAMEAT2 => path::renameat2(i(0), u(1), i(2), u(3), a[4] as u32),
        nr::SYMLINKAT => path::symlinkat(u(0), i(1), u(2)),
        nr::LINKAT => path::linkat(i(0), u(1), i(2), u(3), a[4] as u32),
        nr::READLINKAT => path::readlinkat(i(0), u(1), u(2), u(3)),
        nr::NEWFSTATAT => path::newfstatat(i(0), u(1), u(2), a[3] as u32),
        nr::STATX => path::statx(i(0), u(1), a[2] as u32, a[3] as u32, u(4)),
        nr::FACCESSAT => path::faccessat2(i(0), u(1), a[2] as u32, 0),
        nr::FACCESSAT2 => path::faccessat2(i(0), u(1), a[2] as u32, a[3] as u32),
        nr::FCHMODAT => path::fchmodat2(i(0), u(1), a[2] as u32, 0),
        nr::FCHMODAT2 => path::fchmodat2(i(0), u(1), a[2] as u32, a[3] as u32),
        nr::FCHOWNAT => path::fchownat(i(0), u(1), a[2] as u32, a[3] as u32, a[4] as u32),
        nr::UTIMENSAT => path::utimensat(i(0), u(1), u(2), a[3] as u32),
        nr::FCHMOD => path::fchmod(i(0), a[1] as u32),
        nr::FCHOWN => path::fchown(i(0), a[1] as u32, a[2] as u32),
        nr::TRUNCATE => path::truncate(u(0), a[1] as i64),
        nr::GETCWD => path::getcwd(u(0), u(1)),
        nr::CHDIR => path::chdir(u(0)),
        nr::FCHDIR => path::fchdir(i(0)),
        nr::UMASK => path::umask_call(a[0] as u32),
        nr::STATFS => path::statfs(u(0), u(1)),
        nr::FSTATFS => path::fstatfs(i(0), u(1)),
        nr::UMOUNT2 => path::umount2(u(0), a[1] as u32),
        nr::CHROOT => Err(Errno(EPERM)),
        nr::SETXATTR
        | nr::LSETXATTR
        | nr::FSETXATTR
        | nr::GETXATTR
        | nr::LGETXATTR
        | nr::FGETXATTR
        | nr::LISTXATTR
        | nr::LLISTXATTR
        | nr::FLISTXATTR
        | nr::REMOVEXATTR
        | nr::LREMOVEXATTR
        | nr::FREMOVEXATTR => Err(Errno(EOPNOTSUPP)),
        #[cfg(target_arch = "x86_64")]
        nr::OPEN => path::openat(AT_FDCWD, u(0), a[1] as u32, a[2] as u32),
        #[cfg(target_arch = "x86_64")]
        nr::CREAT => path::openat(
            AT_FDCWD,
            u(0),
            patina_abi::fs::O_CREAT | patina_abi::fs::O_WRONLY | patina_abi::fs::O_TRUNC,
            a[1] as u32,
        ),
        #[cfg(target_arch = "x86_64")]
        nr::STAT => path::stat(u(0), u(1), true),
        #[cfg(target_arch = "x86_64")]
        nr::LSTAT => path::stat(u(0), u(1), false),
        #[cfg(target_arch = "x86_64")]
        nr::ACCESS => path::faccessat2(AT_FDCWD, u(0), a[1] as u32, 0),
        #[cfg(target_arch = "x86_64")]
        nr::READLINK => path::readlinkat(AT_FDCWD, u(0), u(1), u(2)),
        #[cfg(target_arch = "x86_64")]
        nr::MKDIR => path::mkdirat(AT_FDCWD, u(0), a[1] as u32),
        #[cfg(target_arch = "x86_64")]
        nr::MKNOD => path::mknodat(AT_FDCWD, u(0), a[1] as u32, a[2]),
        #[cfg(target_arch = "x86_64")]
        nr::RMDIR => path::unlinkat(AT_FDCWD, u(0), patina_abi::fs::AT_REMOVEDIR),
        #[cfg(target_arch = "x86_64")]
        nr::UNLINK => path::unlinkat(AT_FDCWD, u(0), 0),
        #[cfg(target_arch = "x86_64")]
        nr::RENAME => path::renameat2(AT_FDCWD, u(0), AT_FDCWD, u(1), 0),
        #[cfg(target_arch = "x86_64")]
        nr::SYMLINK => path::symlinkat(u(0), AT_FDCWD, u(1)),
        #[cfg(target_arch = "x86_64")]
        nr::LINK => path::linkat(AT_FDCWD, u(0), AT_FDCWD, u(1), 0),
        #[cfg(target_arch = "x86_64")]
        nr::CHMOD => path::fchmodat2(AT_FDCWD, u(0), a[1] as u32, 0),
        #[cfg(target_arch = "x86_64")]
        nr::CHOWN => path::fchownat(AT_FDCWD, u(0), a[1] as u32, a[2] as u32, 0),
        #[cfg(target_arch = "x86_64")]
        nr::LCHOWN => path::lchown(u(0), a[1] as u32, a[2] as u32),
        #[cfg(target_arch = "x86_64")]
        nr::UTIME => path::utime(u(0), u(1)),
        #[cfg(target_arch = "x86_64")]
        nr::UTIMES => path::futimesat(AT_FDCWD, u(0), u(1)),
        #[cfg(target_arch = "x86_64")]
        nr::FUTIMESAT => path::futimesat(i(0), u(1), u(2)),
        // The old getdents record format is not implemented; Go uses
        // getdents64.
        #[cfg(target_arch = "x86_64")]
        nr::GETDENTS => Err(Errno(ENOSYS)),

        // poll.rs
        nr::EPOLL_CREATE1 => poll::epoll_create1(a[0] as u32),
        nr::EPOLL_CTL => poll::epoll_ctl(i(0), i(1), i(2), u(3)),
        nr::EPOLL_PWAIT => poll::epoll_pwait(i(0), u(1), i(2), i(3), u(4), u(5)),
        nr::EPOLL_PWAIT2 => poll::epoll_pwait2(i(0), u(1), i(2), u(3), u(4), u(5)),
        nr::PPOLL => poll::ppoll(u(0), u(1), u(2), u(3), u(4)),
        nr::PSELECT6 => poll::pselect6(i(0), u(1), u(2), u(3), u(4), u(5)),
        #[cfg(target_arch = "x86_64")]
        nr::SELECT => poll::select(i(0), u(1), u(2), u(3), u(4)),
        #[cfg(target_arch = "x86_64")]
        nr::EPOLL_CREATE => poll::epoll_create(i(0)),
        #[cfg(target_arch = "x86_64")]
        nr::EPOLL_WAIT => poll::epoll_pwait(i(0), u(1), i(2), i(3), 0, 0),
        #[cfg(target_arch = "x86_64")]
        nr::POLL => poll::poll(u(0), u(1), i(2)),

        // net.rs
        nr::SOCKET => net::socket(i(0), i(1), i(2)),
        nr::SOCKETPAIR => net::socketpair(i(0), i(1), i(2), u(3)),
        nr::BIND => net::bind(i(0), u(1), u(2)),
        nr::LISTEN => net::listen(i(0), i(1)),
        nr::ACCEPT => net::accept4(i(0), u(1), u(2), 0),
        nr::ACCEPT4 => net::accept4(i(0), u(1), u(2), a[3] as u32),
        nr::CONNECT => net::connect(i(0), u(1), u(2)),
        nr::GETSOCKNAME => net::getsockname(i(0), u(1), u(2)),
        nr::GETPEERNAME => net::getpeername(i(0), u(1), u(2)),
        nr::SETSOCKOPT => net::setsockopt(i(0), i(1), i(2), u(3), i(4)),
        nr::GETSOCKOPT => net::getsockopt(i(0), i(1), i(2), u(3), u(4)),
        nr::SENDTO => net::sendto(i(0), u(1), u(2), a[3] as u32, u(4), a[5] as u32 as usize),
        nr::RECVFROM => net::recvfrom(i(0), u(1), u(2), a[3] as u32, u(4), u(5)),
        nr::SENDMSG => net::sendmsg(i(0), u(1), a[2] as u32),
        nr::RECVMSG => net::recvmsg(i(0), u(1), a[2] as u32),
        nr::SHUTDOWN => net::shutdown(i(0), i(1)),

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
        nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => Ok(proc::get_id(n) as isize),
        nr::SET_TID_ADDRESS => proc::set_tid_address(a[0]),
        nr::UNAME => proc::uname(u(0)),
        nr::SCHED_GETAFFINITY => proc::sched_getaffinity(i(0), u(1), u(2)),
        nr::SCHED_SETAFFINITY => proc::sched_setaffinity(i(0), u(1), u(2)),
        nr::SCHED_YIELD => thread::sched_yield(),
        nr::PRLIMIT64 => proc::prlimit64(i(0), a[1] as u32, u(2), u(3)),
        nr::GETRLIMIT => proc::prlimit64(0, a[0] as u32, 0, u(1)),
        nr::SETRLIMIT => proc::prlimit64(0, a[0] as u32, u(1), 0),
        nr::GETRANDOM => proc::getrandom(u(0), u(1), a[2] as u32),
        #[cfg(target_arch = "x86_64")]
        nr::ARCH_PRCTL => proc::arch_prctl(frame, i(0), a[1]),
        // Tier-3 stubs (D57).
        nr::PRCTL => proc::prctl(i(0), a[1]),
        nr::GETPGID | nr::GETSID => proc::own_id(i(0)),
        #[cfg(target_arch = "x86_64")]
        nr::GETPGRP => Ok(1),
        nr::SETPGID => Ok(0),
        nr::SETSID => Err(Errno(EPERM)),
        nr::GETGROUPS => proc::getgroups(i(0), u(1)),
        nr::SETGROUPS => proc::setgroups(u(0), u(1)),
        nr::SETUID
        | nr::SETGID
        | nr::SETREUID
        | nr::SETREGID
        | nr::SETRESUID
        | nr::SETRESGID
        | nr::SETFSUID
        | nr::SETFSGID => proc::set_id(n, a[0] as u32, a[1] as u32, a[2] as u32),
        nr::GETRESUID | nr::GETRESGID => proc::getres(n == nr::GETRESGID, u(0), u(1), u(2)),
        nr::CAPGET => proc::capget(u(0), u(1)),
        nr::CAPSET => Err(Errno(EPERM)),
        nr::SCHED_GETSCHEDULER => proc::own_id(i(0)).map(|_| 0),
        nr::SCHED_SETSCHEDULER | nr::SCHED_SETPARAM => Err(Errno(EPERM)),
        nr::SCHED_GETPARAM => proc::sched_getparam(i(0), u(1)),
        nr::SCHED_GET_PRIORITY_MAX | nr::SCHED_GET_PRIORITY_MIN => Ok(0),
        // The raw syscall returns 20 - nice.
        nr::GETPRIORITY => Ok(20),
        nr::SETPRIORITY => Ok(0),
        nr::PERSONALITY => Ok(0),
        nr::SYSINFO => proc::sysinfo(u(0)),
        nr::GETCPU => proc::getcpu(u(0), u(1)),
        nr::MLOCK | nr::MUNLOCK | nr::MLOCKALL | nr::MUNLOCKALL | nr::MSYNC | nr::MLOCK2 => Ok(0),
        nr::WAIT4 | nr::WAITID => Err(Errno(ECHILD)),
        // Logged once (enosys): a program that needs these cannot work.
        nr::EXECVE | nr::EXECVEAT => enosys(n),
        #[cfg(target_arch = "x86_64")]
        nr::FORK | nr::VFORK => enosys(n),

        // time.rs
        nr::CLOCK_GETTIME => time::clock_gettime(i(0), u(1)),
        nr::CLOCK_GETRES => time::clock_getres(i(0), u(1)),
        nr::GETTIMEOFDAY => time::gettimeofday(u(0)),
        nr::NANOSLEEP => time::nanosleep(u(0), u(1)),
        nr::CLOCK_NANOSLEEP => time::clock_nanosleep(i(0), i(1), u(2), u(3)),
        nr::TIMER_CREATE => time::timer_create(i(0), u(1), u(2)),
        nr::TIMER_SETTIME => time::timer_settime(i(0), i(1), u(2), u(3)),
        nr::TIMER_GETTIME => time::timer_gettime(i(0), u(1)),
        nr::TIMER_GETOVERRUN => time::timer_getoverrun(i(0)),
        nr::TIMER_DELETE => time::timer_delete(i(0)),
        nr::GETITIMER => time::getitimer(i(0), u(1)),
        nr::SETITIMER => time::setitimer(i(0), u(1), u(2)),
        #[cfg(target_arch = "x86_64")]
        nr::ALARM => time::alarm(a[0] as u32),
        #[cfg(target_arch = "x86_64")]
        nr::TIME => time::time(u(0)),
        nr::GETRUSAGE => time::getrusage(i(0), u(1)),
        nr::TIMES => time::times(u(0)),

        // thread.rs
        nr::CLONE => thread::clone(frame, a),
        nr::FUTEX => thread::futex(u(0), i(1), a[2] as u32, u(3), a[5] as u32),
        // clone3 → ENOSYS makes glibc/Go fall back to clone.
        // Not implemented, expected, and handled by Go: no log line.
        nr::MEMBARRIER | nr::SET_ROBUST_LIST | nr::RSEQ => Err(Errno(ENOSYS)),
        _ => enosys(n),
    }
}
