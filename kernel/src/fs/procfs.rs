//! procfs-lite and `/sys` (design §8): real directories and symlinks in
//! the root filesystem, generated files (contents produced at open time
//! into a read-only [`MemFile`]), and two "magic" places the path layer
//! (`namei.rs`) intercepts before normal resolution:
//!
//! * `/proc/1/fd/N` (also via `/proc/self/fd`, `/dev/fd`, `/dev/stdin`...):
//!   a symlink to whatever fd N is; following it reaches the open file
//!   itself. The directory lists the open fds ([`FdDirFile`]).
//! * `/proc/1/cwd`: the current directory. A real symlink placeholder
//!   (retargeted by `chdir`) keeps `/proc/self/cwd/x` working through the
//!   VFS; the final component is generated at lookup time.
//!
//! The process is always pid 1 (`/proc/self` → `1`).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::{EACCES, EBADF, EINVAL, EISDIR, Errno};
use patina_abi::fs::{DT_DIR, DT_LNK, O_ACCMODE, O_RDONLY, S_IFREG, SEEK_SET, write_dirent64};
use patina_abi::mm::ProtFlags;
use patina_abi::signal::{SIG_DFL, SIG_IGN};
use patina_vfs::{Fs, Ino};

use super::file::{File, FileFlags, PollHook};
use super::vfile::{MemFile, stat_ino};
use super::vfs::{self, DevNode, VHandle};
use crate::mm::vm::VmaKind;
use crate::sync::SpinLock;
use crate::task;

/// Inode of `/proc/1/fd` (0 before population).
static FD_DIR: AtomicU64 = AtomicU64::new(0);
/// Inode of `/proc/1`.
static PID_DIR: AtomicU64 = AtomicU64::new(0);

pub fn fd_dir() -> Ino {
    FD_DIR.load(Ordering::Relaxed)
}

pub fn pid_dir() -> Ino {
    PID_DIR.load(Ordering::Relaxed)
}

/// A generated read-only file.
struct Gen(Box<dyn Fn() -> Vec<u8> + Send + Sync>);

impl DevNode for Gen {
    fn open(&self, _ino: Ino, flags: u32) -> Result<Arc<dyn File>, Errno> {
        if flags & O_ACCMODE != O_RDONLY {
            return Err(Errno(EACCES));
        }
        Ok(Arc::new(MemFile::new((self.0)())))
    }
}

fn gen_file(fs: &mut Fs, dir: Ino, name: &[u8], f: impl Fn() -> Vec<u8> + Send + Sync + 'static) {
    let sp = vfs::special(Arc::new(Gen(Box::new(f))));
    let _ = fs.mknod(dir, name, S_IFREG | 0o444, 0, Some(sp));
}

fn text(s: &str) -> impl Fn() -> Vec<u8> + Send + Sync + 'static {
    let v = s.as_bytes().to_vec();
    move || v.clone()
}

/// Online CPUs as a list (`0-3`).
fn cpu_list() -> String {
    let n = task::sched::online_cpus();
    if n <= 1 {
        String::from("0")
    } else {
        format!("0-{}", n - 1)
    }
}

/// A random RFC 4122 v4 UUID.
fn uuid() -> String {
    let mut b = [0u8; 16];
    crate::random::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut s = String::new();
    for (i, x) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{:02x}", x);
    }
    s
}

fn kb(frames: usize) -> usize {
    frames * crate::mm::PAGE_SIZE / 1024
}

fn meminfo() -> Vec<u8> {
    // Free whatever deferred TLB-shootdown batches are already safe, so
    // MemFree does not count frames merely waiting for a flush round.
    crate::mm::tlb::reclaim();
    let (free, total) = crate::mm::frame::stats();
    let mut s = String::new();
    let _ = writeln!(s, "MemTotal:       {:>8} kB", kb(total));
    let _ = writeln!(s, "MemFree:        {:>8} kB", kb(free));
    let _ = writeln!(s, "MemAvailable:   {:>8} kB", kb(free));
    let _ = writeln!(s, "Buffers:        {:>8} kB", 0);
    let _ = writeln!(s, "Cached:         {:>8} kB", 0);
    let _ = writeln!(s, "SwapCached:     {:>8} kB", 0);
    let _ = writeln!(s, "SwapTotal:      {:>8} kB", 0);
    let _ = writeln!(s, "SwapFree:       {:>8} kB", 0);
    let _ = writeln!(s, "Shmem:          {:>8} kB", 0);
    let _ = writeln!(s, "Hugepagesize:   {:>8} kB", 2048);
    s.into_bytes()
}

fn cpuinfo() -> Vec<u8> {
    let mut s = String::new();
    for cpu in 0..task::sched::online_cpus() {
        #[cfg(target_arch = "aarch64")]
        {
            let _ = writeln!(s, "processor\t: {}", cpu);
            let _ = writeln!(s, "BogoMIPS\t: 100.00");
            let _ = writeln!(s, "Features\t: fp asimd evtstrm cpuid");
            let _ = writeln!(s, "CPU implementer\t: 0x00");
            let _ = writeln!(s, "CPU architecture: 8");
            let _ = writeln!(s, "CPU variant\t: 0x0");
            let _ = writeln!(s, "CPU part\t: 0x000");
            let _ = writeln!(s, "CPU revision\t: 0");
            let _ = writeln!(s);
        }
        #[cfg(target_arch = "x86_64")]
        {
            let n = task::sched::online_cpus();
            let _ = writeln!(s, "processor\t: {}", cpu);
            let _ = writeln!(s, "vendor_id\t: patina");
            let _ = writeln!(s, "cpu family\t: 6");
            let _ = writeln!(s, "model\t\t: 0");
            let _ = writeln!(s, "model name\t: patina virtual CPU");
            let _ = writeln!(s, "physical id\t: 0");
            let _ = writeln!(s, "siblings\t: {}", n);
            let _ = writeln!(s, "core id\t\t: {}", cpu);
            let _ = writeln!(s, "cpu cores\t: {}", n);
            let _ = writeln!(
                s,
                "flags\t\t: fpu tsc cx8 cmov sse sse2 sse3 ssse3 sse4_1 sse4_2 popcnt"
            );
            let _ = writeln!(s, "bogomips\t: 2000.00");
            let _ = writeln!(s);
        }
    }
    s.into_bytes()
}

fn uptime_ns() -> u64 {
    crate::arch::timer::now_ns()
}

fn uptime() -> Vec<u8> {
    let ns = uptime_ns();
    let cs = ns / 10_000_000;
    let idle = cs * task::sched::online_cpus() as u64 / 2;
    format!(
        "{}.{:02} {}.{:02}\n",
        cs / 100,
        cs % 100,
        idle / 100,
        idle % 100
    )
    .into_bytes()
}

fn joined_nul(v: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for a in v {
        out.extend_from_slice(a);
        out.push(0);
    }
    out
}

/// `/proc/1/maps`: `start-end perms offset dev inode path`.
fn maps() -> Vec<u8> {
    let app_ino = vfs::with(|fs| fs.resolve(fs.root(), b"/app", true).unwrap_or(0));
    let vmas: Vec<crate::mm::vm::Vma> = task::process().space.lock().vmas().copied().collect();
    let mut s = String::new();
    for v in vmas {
        let p = |f: ProtFlags, c: char| if v.prot.contains(f) { c } else { '-' };
        let (ino, path) = match v.kind {
            VmaKind::Elf => (app_ino, "/app"),
            VmaKind::Stack => (0, "[stack]"),
            VmaKind::Vdso => (0, "[vdso]"),
            VmaKind::Vvar => (0, "[vvar]"),
            VmaKind::Anon => (0, ""),
        };
        let line = format!(
            "{:08x}-{:08x} {}{}{}p 00000000 {} {}",
            v.start,
            v.end,
            p(ProtFlags::READ, 'r'),
            p(ProtFlags::WRITE, 'w'),
            p(ProtFlags::EXEC, 'x'),
            if ino != 0 { "00:01" } else { "00:00" },
            ino
        );
        if path.is_empty() {
            let _ = writeln!(s, "{}", line);
        } else {
            let _ = writeln!(s, "{:<72} {}", line, path);
        }
    }
    s.into_bytes()
}

/// (VmSize, VmRSS) in kB: mapped VMAs, and user pages with a frame.
fn vm_kb() -> (usize, usize) {
    let space = task::process().space.lock();
    let size: usize = space.vmas().map(|v| v.end - v.start).sum();
    (size / 1024, kb(space.resident_pages()))
}

/// (pending, blocked, ignored, caught) signal masks.
fn sig_masks() -> (u64, u64, u64, u64) {
    let blocked = task::current().blocked();
    let (mut ign, mut cgt) = (0u64, 0u64);
    let acts = *task::process().sigactions.lock();
    for (sig, a) in acts.iter().enumerate().skip(1) {
        let bit = 1u64 << (sig - 1);
        match a.sa_handler {
            SIG_DFL => {}
            SIG_IGN => ign |= bit,
            _ => cgt |= bit,
        }
    }
    (0, blocked, ign, cgt)
}

/// `/proc/1/status` (`tid` None: the process, shown with the main
/// thread's credentials) or `/proc/1/task/<tid>/status`.
fn status(tid: Option<u32>) -> Vec<u8> {
    let p = task::process();
    let th = tid.and_then(task::thread_by_tid);
    let (cred, groups) = match &th {
        Some(t) => (*t.cred.lock(), t.groups.lock().clone()),
        None => task::process_cred(),
    };
    let (u, g) = (cred.uid, cred.gid);
    let mut gl = String::new();
    for (i, x) in groups.iter().enumerate() {
        let _ = write!(gl, "{}{}", if i > 0 { " " } else { "" }, x);
    }
    let threads = task::live_threads();
    let (vsz, rss) = vm_kb();
    let (pnd, blk, ign, cgt) = sig_masks();
    let umask = p.umask.load(Ordering::Relaxed);
    let mut s = String::new();
    let _ = writeln!(s, "Name:\tapp");
    let _ = writeln!(s, "Umask:\t{:04o}", umask);
    let _ = writeln!(s, "State:\tR (running)");
    let _ = writeln!(s, "Tgid:\t{}", p.pid);
    let _ = writeln!(s, "Ngid:\t0");
    let _ = writeln!(s, "Pid:\t{}", tid.unwrap_or(p.pid));
    let _ = writeln!(s, "PPid:\t0");
    let _ = writeln!(s, "TracerPid:\t0");
    let _ = writeln!(s, "Uid:\t{}\t{}\t{}\t{}", u.r, u.e, u.s, u.fs);
    let _ = writeln!(s, "Gid:\t{}\t{}\t{}\t{}", g.r, g.e, g.s, g.fs);
    let _ = writeln!(s, "FDSize:\t{}", 64);
    let _ = writeln!(s, "Groups:\t{}", gl);
    let _ = writeln!(s, "VmPeak:\t{:>8} kB", vsz);
    let _ = writeln!(s, "VmSize:\t{:>8} kB", vsz);
    let _ = writeln!(s, "VmHWM:\t{:>8} kB", rss);
    let _ = writeln!(s, "VmRSS:\t{:>8} kB", rss);
    let _ = writeln!(s, "Threads:\t{}", threads);
    let _ = writeln!(s, "SigQ:\t0/63000");
    let _ = writeln!(s, "SigPnd:\t{:016x}", pnd);
    let _ = writeln!(s, "ShdPnd:\t{:016x}", 0);
    let _ = writeln!(s, "SigBlk:\t{:016x}", blk);
    let _ = writeln!(s, "SigIgn:\t{:016x}", ign);
    let _ = writeln!(s, "SigCgt:\t{:016x}", cgt);
    let _ = writeln!(s, "CapInh:\t0000000000000000");
    let _ = writeln!(s, "CapPrm:\t{:016x}", cred.cap_prm);
    let _ = writeln!(s, "CapEff:\t{:016x}", cred.cap_eff);
    let _ = writeln!(s, "CapBnd:\t000001ffffffffff");
    let _ = writeln!(s, "CapAmb:\t0000000000000000");
    let _ = writeln!(s, "NoNewPrivs:\t0");
    let _ = writeln!(s, "Seccomp:\t0");
    let _ = writeln!(s, "Cpus_allowed_list:\t{}", cpu_list());
    let _ = writeln!(s, "voluntary_ctxt_switches:\t0");
    let _ = writeln!(s, "nonvoluntary_ctxt_switches:\t0");
    s.into_bytes()
}

/// `/proc/1/stat`: the 52 fields of `fs/proc/array.c`.
fn stat() -> Vec<u8> {
    let p = task::process();
    let ticks = |ns: u64| ns / 10_000_000; // USER_HZ = 100
    let cpu: u64 = task::threads()
        .iter()
        .map(|t| task::sched::cpu_time_ns(t))
        .sum();
    let (vsz, rss) = vm_kb();
    let (pnd, blk, ign, cgt) = sig_masks();
    let threads = task::live_threads();
    let f = [
        format!(
            "{} (app) R 0 {} {} 0 -1 4194560 0 0 0 0",
            p.pid, p.pid, p.pid
        ),
        format!("{} 0 0 0 20 0 {} 0 0", ticks(cpu), threads),
        format!("{} {} 18446744073709551615 0 0 0 0 0", vsz * 1024, rss / 4),
        format!(
            "{} {} {} {} 0 0 0 17 {} 0 0 0 0 0",
            pnd,
            blk,
            ign,
            cgt,
            crate::arch::cpu::id()
        ),
        String::from("0 0 0 0 0 0 0 0"),
    ];
    let mut s = f.join(" ");
    s.push('\n');
    s.into_bytes()
}

fn mounts() -> Vec<u8> {
    b"rootfs / tmpfs rw,relatime 0 0\nproc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n"
        .to_vec()
}

fn hostname() -> Vec<u8> {
    format!("{}\n", super::rootfs::hostname()).into_bytes()
}

/// Inode of `/proc/1/task`.
static TASK_DIR: AtomicU64 = AtomicU64::new(0);

pub fn task_dir() -> Ino {
    TASK_DIR.load(Ordering::Relaxed)
}

/// Make `/proc/1/task` list the live threads (filesystem locked): add a
/// `<tid>` directory (`status`, `stat`, `comm`) for each new thread and
/// remove those of exited ones. Called when the directory is opened, so
/// a listing is current; a lookup of `/proc/1/task/<tid>` for a thread
/// created since the last listing fails.
pub fn sync_tasks(fs: &mut Fs) {
    let dir = task_dir();
    if dir == 0 {
        return;
    }
    let mut live: Vec<u32> = task::threads()
        .iter()
        .filter(|t| matches!(t.kind, task::ThreadKind::User))
        .map(|t| t.tid)
        .collect();
    live.sort_unstable();
    let have = fs.read_dir(dir).unwrap_or_default();
    for (name, ino, _) in &have {
        let tid = core::str::from_utf8(name)
            .ok()
            .and_then(|n| n.parse::<u32>().ok());
        if tid.is_some_and(|t| live.binary_search(&t).is_ok()) {
            continue;
        }
        for f in [&b"status"[..], b"stat", b"comm"] {
            let _ = fs.unlink(*ino, f);
        }
        let _ = fs.rmdir(dir, name);
    }
    for tid in live {
        let name = format!("{}", tid);
        if have.iter().any(|(n, _, _)| n.as_slice() == name.as_bytes()) {
            continue;
        }
        let Ok(d) = fs.mkdir(dir, name.as_bytes(), 0o555) else {
            continue;
        };
        gen_file(fs, d, b"status", move || status(Some(tid)));
        gen_file(fs, d, b"stat", stat);
        gen_file(fs, d, b"comm", move || {
            let n = task::thread_by_tid(tid)
                .map(|t| *t.name.lock())
                .unwrap_or_default();
            let len = n.iter().position(|&b| b == 0).unwrap_or(n.len());
            let mut v = n[..len].to_vec();
            v.push(b'\n');
            v
        });
    }
}

/// Create `/proc` and `/sys` (filesystem locked, after population).
pub fn populate(fs: &mut Fs) {
    let root = fs.root();
    let Ok(proc) = fs.mkdir_all(root, b"/proc", 0o555) else {
        return;
    };
    let _ = fs.symlink(b"1", proc, b"self");
    let _ = fs.symlink(b"1", proc, b"thread-self");
    if let Ok(pid) = fs.mkdir_all(proc, b"1", 0o555) {
        PID_DIR.store(pid, Ordering::Relaxed);
        let _ = fs.symlink(b"/app", pid, b"exe");
        let _ = fs.symlink(b"/", pid, b"cwd");
        let _ = fs.symlink(b"/", pid, b"root");
        if let Ok(fd) = fs.mkdir_all(pid, b"fd", 0o500) {
            FD_DIR.store(fd, Ordering::Relaxed);
        }
        gen_file(fs, pid, b"maps", maps);
        gen_file(fs, pid, b"status", || status(None));
        if let Ok(t) = fs.mkdir_all(pid, b"task", 0o555) {
            TASK_DIR.store(t, Ordering::Relaxed);
        }
        gen_file(fs, pid, b"stat", stat);
        gen_file(fs, pid, b"comm", text("app\n"));
        gen_file(fs, pid, b"cmdline", || joined_nul(&task::process().argv));
        gen_file(fs, pid, b"environ", || joined_nul(&task::process().envp));
        gen_file(fs, pid, b"mounts", mounts);
    }
    gen_file(fs, proc, b"cpuinfo", cpuinfo);
    gen_file(fs, proc, b"meminfo", meminfo);
    gen_file(fs, proc, b"uptime", uptime);
    gen_file(fs, proc, b"mounts", mounts);
    gen_file(fs, proc, b"loadavg", || {
        format!("0.00 0.00 0.00 1/{} 1\n", task::live_threads()).into_bytes()
    });
    gen_file(
        fs,
        proc,
        b"version",
        text("Linux version 6.6.0-patina (patina) #1 SMP\n"),
    );
    if let Ok(k) = fs.mkdir_all(proc, b"sys/kernel", 0o555) {
        gen_file(fs, k, b"hostname", hostname);
        gen_file(fs, k, b"osrelease", text("6.6.0-patina\n"));
        gen_file(fs, k, b"ostype", text("Linux\n"));
        gen_file(fs, k, b"version", text("#1 SMP\n"));
        gen_file(fs, k, b"pid_max", text("4194304\n"));
        gen_file(
            fs,
            k,
            b"threads-max",
            text(&format!("{}\n", task::MAX_THREADS)),
        );
        if let Ok(r) = fs.mkdir_all(k, b"random", 0o555) {
            let boot_id = format!("{}\n", uuid());
            gen_file(fs, r, b"boot_id", text(&boot_id));
            gen_file(fs, r, b"uuid", || format!("{}\n", uuid()).into_bytes());
        }
    }
    if let Ok(n) = fs.mkdir_all(proc, b"net", 0o555) {
        // Interface multicast memberships (Go's Interface.MulticastAddrs).
        gen_file(fs, n, b"igmp", crate::net::netlink::proc_igmp);
        gen_file(fs, n, b"igmp6", crate::net::netlink::proc_igmp6);
    }
    if let Ok(n) = fs.mkdir_all(proc, b"sys/net/core", 0o555) {
        gen_file(fs, n, b"somaxconn", text("4096\n"));
    }
    if let Ok(f) = fs.mkdir_all(proc, b"sys/fs", 0o555) {
        // Pipes have a fixed 64 KiB ring (no F_SETPIPE_SZ), so that is
        // also the most an unprivileged F_SETPIPE_SZ could ask for.
        gen_file(fs, f, b"pipe-max-size", text("65536\n"));
    }
    if let Ok(v) = fs.mkdir_all(proc, b"sys/vm", 0o555) {
        gen_file(fs, v, b"overcommit_memory", text("0\n"));
    }
    let Ok(sys) = fs.mkdir_all(root, b"/sys", 0o555) else {
        return;
    };
    if let Ok(t) = fs.mkdir_all(sys, b"kernel/mm/transparent_hugepage", 0o755) {
        gen_file(fs, t, b"hpage_pmd_size", text("2097152\n"));
        gen_file(fs, t, b"enabled", text("always [madvise] never\n"));
    }
    if let Ok(c) = fs.mkdir_all(sys, b"devices/system/cpu", 0o755) {
        gen_file(fs, c, b"online", || {
            format!("{}\n", cpu_list()).into_bytes()
        });
        gen_file(fs, c, b"possible", || {
            format!("{}\n", cpu_list()).into_bytes()
        });
    }
}

/// Point the `/proc/1/cwd` placeholder at `target` (filesystem locked).
pub fn set_cwd_link(fs: &mut Fs, target: &[u8]) {
    let pid = pid_dir();
    if pid == 0 {
        return;
    }
    let _ = fs.unlink(pid, b"cwd");
    let _ = fs.symlink(target, pid, b"cwd");
}

/// `/proc/1/fd` opened as a directory: lists the open fds as symlinks.
/// The stream position is `fd + 2` (0 and 1 are `.` and `..`).
pub struct FdDirFile {
    h: VHandle,
    pos: SpinLock<u64>,
}

impl FdDirFile {
    pub fn new(h: VHandle) -> FdDirFile {
        FdDirFile {
            h,
            pos: SpinLock::new(0),
        }
    }
}

/// Inode numbers reported for the `/proc/1/fd/N` links.
const FD_LINK_INO_BASE: u64 = 0x7000_0000;

impl File for FdDirFile {
    fn kind(&self) -> &'static str {
        "dir"
    }
    fn read(&self, _buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EISDIR))
    }
    fn write(&self, _buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        Err(Errno(EBADF))
    }
    fn poll(&self, _hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        EpollFlags::IN | EpollFlags::RDNORM
    }
    fn pollable(&self) -> bool {
        false
    }
    fn stat(&self) -> Result<Stat, Errno> {
        stat_ino(self.h.ino())
    }
    fn lseek(&self, off: i64, whence: i32) -> Result<u64, Errno> {
        if whence != SEEK_SET || off < 0 {
            return Err(Errno(EINVAL));
        }
        *self.pos.lock() = off as u64;
        Ok(off as u64)
    }
    fn getdents64(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut fds = task::process().files.lock().fds();
        fds.sort_unstable();
        let mut pos = self.pos.lock();
        let mut used = 0;
        let mut emit = |pos: &mut u64, ino: u64, next: u64, t: u8, name: &[u8]| -> bool {
            match write_dirent64(&mut buf[used..], ino, next as i64, t, name) {
                Some(n) => {
                    used += n;
                    *pos = next;
                    true
                }
                None => false,
            }
        };
        let mut full = false;
        if *pos == 0 && !emit(&mut pos, fd_dir(), 1, DT_DIR, b".") {
            full = true;
        }
        if !full && *pos == 1 && !emit(&mut pos, pid_dir(), 2, DT_DIR, b"..") {
            full = true;
        }
        if !full {
            for fd in fds {
                let p = fd as u64 + 2;
                if p < *pos {
                    continue;
                }
                let name = format!("{}", fd);
                if !emit(
                    &mut pos,
                    FD_LINK_INO_BASE + fd as u64,
                    p + 1,
                    DT_LNK,
                    name.as_bytes(),
                ) {
                    full = true;
                    break;
                }
            }
        }
        if full && used == 0 {
            return Err(Errno(EINVAL));
        }
        Ok(used)
    }
    fn vnode(&self) -> Option<Ino> {
        Some(self.h.ino())
    }
    fn proc_name(&self) -> Vec<u8> {
        b"/proc/1/fd".to_vec()
    }
}

/// `lstat` of a magic link (`/proc/1/fd/N`, `/proc/1/cwd`).
pub fn link_stat(fd: Option<i32>, target_len: usize, write: bool) -> Stat {
    let mode = match fd {
        Some(_) => patina_abi::fs::S_IFLNK | 0o500 | if write { 0o200 } else { 0 },
        None => patina_abi::fs::S_IFLNK | 0o777,
    };
    let mut st = super::file::anon_stat(mode, FD_LINK_INO_BASE + fd.map_or(0xffff, |f| f as u64));
    st.st_size = target_len as _;
    st.st_dev = patina_abi::fs::makedev(0, 1);
    st
}
