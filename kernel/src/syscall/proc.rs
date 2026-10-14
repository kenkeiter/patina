//! Process-level syscalls: exit, ids, uname, affinity, rlimits,
//! getrandom, x86 `arch_prctl`.

use core::sync::atomic::Ordering;

use patina_abi::epoll::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM};
use patina_abi::errno::{EINVAL, ESRCH, Errno};
use patina_abi::misc::{
    RLIM_INFINITY, RLIM_NLIMITS, RLIMIT_CORE, RLIMIT_NOFILE, RLIMIT_STACK, Rlimit, Utsname,
};

use super::{SysResult, with_space};
use crate::sync::SpinLock;
use crate::task;

/// `exit_group(code)`: stop the other CPUs (so no other thread runs user
/// code any more), then end the VM with `code`.
pub fn exit_group(code: u64) -> ! {
    crate::task::sched::stop_other_cpus();
    let (free, total) = crate::mm::frame::stats();
    crate::kprintln!("[patina] frames at exit: {} free / {} total", free, total);
    crate::exit((code & 0xff) as i32)
}

pub fn set_tid_address(p: u64) -> SysResult {
    let t = task::current();
    t.clear_child_tid.store(p, Ordering::Relaxed);
    Ok(t.tid as isize)
}

pub fn uname(buf: usize) -> SysResult {
    let u = Utsname::new(
        "Linux",
        "patina",
        "6.6.0-patina",
        "#1 SMP",
        patina_abi::current::UTS_MACHINE,
        "(none)",
    );
    with_space(|s| s.write_val(buf, &u))?;
    Ok(0)
}

fn valid_pid(pid: i32) -> bool {
    pid == 0 || pid == task::process().pid as i32 || pid == task::current().tid as i32
}

/// Every CPU running the scheduler (so Go's `NumCPU`/GOMAXPROCS = vCPUs).
/// Returns the bytes written, as Linux does (its cpumask size: 8 on 64-bit
/// with NR_CPUS <= 64).
pub fn sched_getaffinity(pid: i32, len: usize, mask: usize) -> SysResult {
    if !valid_pid(pid) {
        return Err(Errno(ESRCH));
    }
    if len < 8 || !len.is_multiple_of(8) {
        return Err(Errno(EINVAL));
    }
    let m = crate::task::sched::online_mask();
    with_space(|s| s.write_val(mask, &m))?;
    Ok(8)
}

const fn lim(cur: u64, max: u64) -> Rlimit {
    Rlimit {
        rlim_cur: cur,
        rlim_max: max,
    }
}

static RLIMITS: SpinLock<[Rlimit; RLIM_NLIMITS as usize]> = SpinLock::new({
    let mut l = [lim(RLIM_INFINITY, RLIM_INFINITY); RLIM_NLIMITS as usize];
    l[RLIMIT_STACK as usize] = lim(8 << 20, RLIM_INFINITY);
    l[RLIMIT_CORE as usize] = lim(0, RLIM_INFINITY);
    l[RLIMIT_NOFILE as usize] = lim(1024, 1 << 20);
    l[patina_abi::misc::RLIMIT_MEMLOCK as usize] = lim(8 << 20, 8 << 20);
    l[patina_abi::misc::RLIMIT_SIGPENDING as usize] = lim(63_000, 63_000);
    l[patina_abi::misc::RLIMIT_MSGQUEUE as usize] = lim(819_200, 819_200);
    l[patina_abi::misc::RLIMIT_NICE as usize] = lim(0, 0);
    l[patina_abi::misc::RLIMIT_RTPRIO as usize] = lim(0, 0);
    l
});

/// `prlimit64(pid, resource, new, old)` (also getrlimit/setrlimit).
/// Limits are stored and reported, not enforced.
pub fn prlimit64(pid: i32, res: u32, new: usize, old: usize) -> SysResult {
    if !valid_pid(pid) {
        return Err(Errno(ESRCH));
    }
    if res >= RLIM_NLIMITS {
        return Err(Errno(EINVAL));
    }
    let newv: Option<Rlimit> = if new != 0 {
        let v: Rlimit = with_space(|s| s.read_val(new))?;
        if v.rlim_cur > v.rlim_max {
            return Err(Errno(EINVAL));
        }
        Some(v)
    } else {
        None
    };
    let prev = {
        let mut l = RLIMITS.lock();
        let prev = l[res as usize];
        if let Some(v) = newv {
            l[res as usize] = v;
        }
        prev
    };
    if old != 0 {
        with_space(|s| s.write_val(old, &prev))?;
    }
    Ok(0)
}

/// `RLIMIT_NOFILE`'s soft limit: fds must be below it (`EMFILE`).
pub fn nofile_limit() -> usize {
    RLIMITS.lock()[RLIMIT_NOFILE as usize].rlim_cur.min(1 << 20) as usize
}

/// `getrandom`: see `random.rs` (not cryptographic before Phase 3).
pub fn getrandom(buf: usize, len: usize, flags: u32) -> SysResult {
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0
        || flags & (GRND_RANDOM | GRND_INSECURE) == (GRND_RANDOM | GRND_INSECURE)
    {
        return Err(Errno(EINVAL));
    }
    let len = len.min(i32::MAX as usize);
    let mut chunk = [0u8; 256];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(chunk.len());
        crate::random::fill(&mut chunk[..n]);
        if let Err(e) = with_space(|s| s.copy_to_user(buf + done, &chunk[..n])) {
            return if done > 0 { Ok(done as isize) } else { Err(e) };
        }
        done += n;
    }
    Ok(len as isize)
}

/// x86_64 `arch_prctl`: FS base only.
#[cfg(target_arch = "x86_64")]
pub fn arch_prctl(frame: &mut crate::arch::ctx::TrapFrame, code: i32, addr: u64) -> SysResult {
    use patina_abi::errno::EPERM;
    use patina_abi::x86_64::{ARCH_GET_FS, ARCH_SET_FS};
    match code {
        ARCH_SET_FS => {
            if addr >= crate::arch::uspace::USER_VA_END as u64 {
                return Err(Errno(EPERM));
            }
            frame.set_tls(addr);
            Ok(0)
        }
        ARCH_GET_FS => {
            let v = frame.tls();
            with_space(|s| s.write_val(addr as usize, &v))?;
            Ok(0)
        }
        _ => Err(Errno(EINVAL)),
    }
}
