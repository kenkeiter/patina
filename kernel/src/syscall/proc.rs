//! Process-level syscalls: exit, ids, uname, affinity, rlimits,
//! getrandom, x86 `arch_prctl`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use patina_abi::current::nr;

use patina_abi::epoll::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM};
use patina_abi::errno::{EINVAL, EPERM, ESRCH, Errno};
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
        // Same name as /proc/sys/kernel/hostname and /etc/hostname.
        crate::fs::rootfs::hostname(),
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
///
/// Checks in Linux's order (`kernel/sys.c` `prlimit64`, `do_prlimit`): the
/// new limit is copied in first (EFAULT), then the pid (ESRCH), the
/// resource (EINVAL), `rlim_cur > rlim_max` (EINVAL).
pub fn prlimit64(pid: i32, res: u32, new: usize, old: usize) -> SysResult {
    let newv: Option<Rlimit> = if new != 0 {
        Some(with_space(|s| s.read_val(new))?)
    } else {
        None
    };
    if !valid_pid(pid) {
        return Err(Errno(ESRCH));
    }
    if res >= RLIM_NLIMITS {
        return Err(Errno(EINVAL));
    }
    if let Some(v) = &newv {
        if v.rlim_cur > v.rlim_max {
            return Err(Errno(EINVAL));
        }
        // do_prlimit: the fd table cannot grow past fs.nr_open.
        if res == RLIMIT_NOFILE && v.rlim_max > NR_OPEN {
            return Err(Errno(EPERM));
        }
    }
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

/// Linux `sysctl_nr_open` (default 1024 * 1024): the largest
/// `RLIMIT_NOFILE`.
const NR_OPEN: u64 = 1 << 20;

/// `RLIMIT_NOFILE`'s soft limit: fds must be below it (`EMFILE`).
pub fn nofile_limit() -> usize {
    RLIMITS.lock()[RLIMIT_NOFILE as usize].rlim_cur.min(NR_OPEN) as usize
}

/// `getrandom`: see `random.rs` (not cryptographic before Phase 3).
pub fn getrandom(buf: usize, len: usize, flags: u32) -> SysResult {
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0
        || flags & (GRND_RANDOM | GRND_INSECURE) == (GRND_RANDOM | GRND_INSECURE)
    {
        return Err(Errno(EINVAL));
    }
    // drivers/char/random.c: import_ubuf (count clamped to MAX_RW_COUNT,
    // access_ok → EFAULT), then get_random_bytes_user copies until a
    // fault and returns the partial count (EFAULT only if nothing was
    // copied): sized up front to the writable run (D86).
    let len = len.min(super::fs::MAX_RW);
    let len = super::fs::out_room(buf, len)?;
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

// ---- Tier-3 stubs (design §7; policy in doc/decisions.md D57) ----------

/// `prctl`: thread name, dumpable, no_new_privs, VMA names, keepcaps;
/// else `EINVAL`.
pub fn prctl(op: i32, arg2: u64) -> SysResult {
    use patina_abi::misc::{
        PR_GET_DUMPABLE, PR_GET_KEEPCAPS, PR_GET_NAME, PR_GET_NO_NEW_PRIVS, PR_SET_DUMPABLE,
        PR_SET_KEEPCAPS, PR_SET_NAME, PR_SET_NO_NEW_PRIVS, PR_SET_VMA, TASK_COMM_LEN,
    };
    match op {
        PR_SET_NAME => {
            let mut name = [0u8; TASK_COMM_LEN];
            // strncpy_from_user of at most 15 bytes: read byte-wise so a
            // short name near the end of a mapping does not fault.
            for (i, slot) in name.iter_mut().take(TASK_COMM_LEN - 1).enumerate() {
                let b: u8 = with_space(|s| s.read_val(arg2 as usize + i))?;
                if b == 0 {
                    break;
                }
                *slot = b;
            }
            *task::current().name.lock() = name;
            Ok(0)
        }
        PR_GET_NAME => {
            let name = *task::current().name.lock();
            with_space(|s| s.write_val(arg2 as usize, &name))?;
            Ok(0)
        }
        PR_SET_DUMPABLE => {
            if arg2 > 1 {
                return Err(Errno(EINVAL));
            }
            Ok(0)
        }
        PR_GET_DUMPABLE => Ok(1),
        PR_SET_NO_NEW_PRIVS => Ok(0),
        PR_GET_NO_NEW_PRIVS => Ok(0),
        PR_SET_VMA => Ok(0),
        PR_GET_KEEPCAPS => Ok(task::current().cred.lock().keepcaps as isize),
        PR_SET_KEEPCAPS => task::current().cred.lock().set_keepcaps(arg2).map(|_| 0),
        _ => Err(Errno(EINVAL)),
    }
}

/// `capget`: version negotiation, then the caller's capability sets.
pub fn capget(hdr: usize, data: usize) -> SysResult {
    use patina_abi::misc::{
        CapUserData, CapUserHeader, LINUX_CAPABILITY_VERSION_1, LINUX_CAPABILITY_VERSION_2,
        LINUX_CAPABILITY_VERSION_3,
    };
    let mut h: CapUserHeader = with_space(|s| s.read_val(hdr))?;
    let n = match h.version {
        LINUX_CAPABILITY_VERSION_1 => 1,
        LINUX_CAPABILITY_VERSION_2 | LINUX_CAPABILITY_VERSION_3 => 2,
        _ => {
            h.version = LINUX_CAPABILITY_VERSION_3;
            with_space(|s| s.write_val(hdr, &h))?;
            return if data == 0 { Ok(0) } else { Err(Errno(EINVAL)) };
        }
    };
    if h.pid < 0 {
        return Err(Errno(EINVAL));
    }
    if data != 0 {
        // The calling thread's sets (all or nothing, patina_abi::cred).
        let c = *task::current().cred.lock();
        let set = |i: u32| CapUserData {
            effective: (c.cap_eff >> (32 * i)) as u32,
            permitted: (c.cap_prm >> (32 * i)) as u32,
            inheritable: 0,
        };
        let d = [set(0), set(1)];
        for (i, v) in d.iter().take(n).enumerate() {
            with_space(|s| s.write_val(data + i * size_of::<CapUserData>(), v))?;
        }
    }
    Ok(0)
}

/// `sched_setaffinity`: accepted if the mask names an online CPU (no
/// migration is done).
pub fn sched_setaffinity(pid: i32, len: usize, mask: usize) -> SysResult {
    if !valid_pid(pid) {
        return Err(Errno(ESRCH));
    }
    if len == 0 {
        return Err(Errno(EINVAL));
    }
    let mut buf = [0u8; 8];
    let n = len.min(8);
    with_space(|s| s.copy_from_user(mask, &mut buf[..n]))?;
    let m = u64::from_le_bytes(buf);
    if m & crate::task::sched::online_mask() == 0 {
        return Err(Errno(EINVAL));
    }
    Ok(0)
}

/// `getcpu(cpu, node, cache)`.
pub fn getcpu(cpu: usize, node: usize) -> SysResult {
    let c = crate::arch::cpu::id() as u32;
    if cpu != 0 {
        with_space(|s| s.write_val(cpu, &c))?;
    }
    if node != 0 {
        with_space(|s| s.write_val(node, &0u32))?;
    }
    Ok(0)
}

/// `sysinfo`: uptime, memory from the frame allocator, one process.
pub fn sysinfo(buf: usize) -> SysResult {
    let (free, total) = crate::mm::frame::stats();
    let page = crate::mm::PAGE_SIZE as u64;
    let info = patina_abi::misc::Sysinfo {
        uptime: (crate::arch::timer::now_ns() / 1_000_000_000) as i64,
        totalram: total as u64 * page,
        freeram: free as u64 * page,
        procs: crate::task::live_threads().min(u16::MAX as usize) as u16,
        mem_unit: 1,
        ..Default::default()
    };
    with_space(|s| s.write_val(buf, &info))?;
    Ok(0)
}

/// `getresuid`/`getresgid`: everything is 0 (root).
pub fn getres(gid: bool, a: usize, b: usize, c: usize) -> SysResult {
    let cr = *task::current().cred.lock();
    let ids = if gid { cr.gid } else { cr.uid };
    for (p, v) in [(a, ids.r), (b, ids.e), (c, ids.s)] {
        with_space(|s| s.write_val(p, &v))?;
    }
    Ok(0)
}

/// `getuid` / `geteuid` / `getgid` / `getegid` of the calling thread.
pub fn get_id(n: usize) -> u32 {
    let c = *task::current().cred.lock();
    match n {
        nr::GETUID => c.uid.r,
        nr::GETEUID => c.uid.e,
        nr::GETGID => c.gid.r,
        _ => c.gid.e,
    }
}

/// The `set*uid` / `set*gid` family on the calling thread's credentials
/// (Linux rules in `patina_abi::cred`; Go applies them to every thread
/// through `AllThreadsSyscall`).
pub fn set_id(n: usize, a: u32, b: u32, c: u32) -> SysResult {
    let t = task::current();
    let mut cr = t.cred.lock();
    let r = match n {
        nr::SETUID => cr.setuid(a),
        nr::SETGID => cr.setgid(a),
        nr::SETREUID => cr.setreuid(a, b),
        nr::SETREGID => cr.setregid(a, b),
        nr::SETRESUID => cr.setresuid(a, b, c),
        nr::SETRESGID => cr.setresgid(a, b, c),
        nr::SETFSUID => return Ok(cr.setfsuid(a) as isize),
        _ => return Ok(cr.setfsgid(a) as isize),
    };
    r.map(|_| 0)
}

/// `getgroups(size, list)`.
pub fn getgroups(size: i32, list: usize) -> SysResult {
    let g = task::current().groups.lock().clone();
    let n = patina_abi::cred::getgroups_count(size, g.len())?;
    for (i, v) in g.iter().take(n).enumerate() {
        with_space(|s| s.write_val(list + i * 4, v))?;
    }
    Ok(g.len() as isize)
}

/// `setgroups(size, list)` (the calling thread; Go uses AllThreadsSyscall).
pub fn setgroups(size: usize, list: usize) -> SysResult {
    let t = task::current();
    let cr = *t.cred.lock();
    patina_abi::cred::check_setgroups(&cr, size)?;
    let mut g = Vec::with_capacity(size);
    for i in 0..size {
        let v: u32 = with_space(|s| s.read_val(list + i * 4))?;
        g.push(v);
    }
    // Linux keeps the list sorted (groups_sort).
    g.sort_unstable();
    *t.groups.lock() = Arc::new(g);
    Ok(0)
}

/// `sched_getparam`: `sched_priority` 0.
pub fn sched_getparam(pid: i32, param: usize) -> SysResult {
    if !valid_pid(pid) {
        return Err(Errno(ESRCH));
    }
    with_space(|s| s.write_val(param, &0i32))?;
    Ok(0)
}

/// `getpgid`/`getsid` of ourselves: 1.
pub fn own_id(pid: i32) -> SysResult {
    if !valid_pid(pid) {
        return Err(Errno(ESRCH));
    }
    Ok(1)
}
