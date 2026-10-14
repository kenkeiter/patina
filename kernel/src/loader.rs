//! Load `/app` from the initrd and start it (design §4 "ELF loading",
//! "Stacks", §7 auxv).
//!
//! 1. Unpack the newc initrd into the root filesystem (`fs::rootfs`;
//!    file pages stay borrowed from the initrd) and look `/app` up there.
//! 2. Validate it with `patina_loader::elf::parse` (static, non-PIE, this
//!    arch).
//! 3. Each `PT_LOAD` becomes an ELF VMA, populated eagerly: a zeroed frame
//!    per page, file bytes copied in (the rest is .bss), then mapped with
//!    the segment's permissions (copy before map: aarch64 syncs the I-cache
//!    at map time).
//! 4. An 8 MiB lazily filled stack VMA ending at [`STACK_TOP`], with the
//!    Linux-exact argv/envp/auxv image from `patina_loader::stack::build`
//!    copied to its top through uaccess.
//! 5. The vDSO image and the vvar page are mapped (`vdso::map`, D75) and
//!    `AT_SYSINFO_EHDR` leads the auxv, unless `patina.novdso=1`.
//! 6. The `Process` and thread 0 are created and thread 0 enters user mode.
//!
//! Failures print `[patina] fatal: ...` and exit 127 (no/bad `/app` or
//! initrd) or 126 (not a loadable executable), like a shell would.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use patina_abi::auxv::*;
use patina_abi::mm::ProtFlags;
use patina_abi::signal::SigAction;
use patina_cmdline::Cmdline;
use patina_loader::elf::{self, Machine};
use patina_loader::stack;

use crate::arch::{self, ctx};
use crate::bootinfo::BootInfo;
use crate::mm::vm::{AddressSpace, VmaKind};
use crate::mm::{PAGE_SIZE, frame, phys_to_virt};
use crate::sync::SpinLock;
use crate::task::{self, NSIG_SLOTS, Process};

/// Top of the initial thread's stack (exclusive).
pub const STACK_TOP: usize = 0x7fff_f000_0000;
/// Size of the initial stack VMA (Go's main thread stack; RLIMIT_STACK).
pub const STACK_SIZE: usize = 8 << 20;

const EXE_PATH: &[u8] = b"/app";

#[cfg(target_arch = "aarch64")]
const MACHINE: Machine = Machine::Aarch64;
#[cfg(target_arch = "x86_64")]
const MACHINE: Machine = Machine::X86_64;

fn fatal(code: i32, args: core::fmt::Arguments<'_>) -> ! {
    crate::kprintln!("[patina] fatal: {}", args);
    crate::exit(code)
}

/// Find `/app` in the root filesystem (populated from the initrd). Its
/// pages are borrowed from the initrd, so this does not copy.
fn find_app() -> &'static [u8] {
    use patina_vfs::FileType as T;
    let r = crate::fs::vfs::with(|fs| {
        let ino = fs.resolve(fs.root(), EXE_PATH, true).ok()?;
        if fs.file_type(ino) != Ok(T::Regular) || fs.special(ino).is_some() {
            return Some(Err(()));
        }
        if let Some(d) = fs.static_contents(ino) {
            return Some(Ok(d));
        }
        let mut v = vec![0u8; fs.size(ino).ok()? as usize];
        let n = fs.read_at(ino, 0, &mut v).ok()?;
        v.truncate(n);
        Some(Ok(&*v.leak()))
    });
    match r {
        Some(Ok(d)) => d,
        Some(Err(())) => fatal(
            126,
            format_args!("/app in the initrd is not a regular file"),
        ),
        None => fatal(127, format_args!("/app not found in the initrd")),
    }
}

/// argv (`/app` + args), envp (`K=V`, last duplicate wins, first-seen
/// order) from the parsed command line.
fn argv_envp(c: &Cmdline) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut argv = vec![EXE_PATH.to_vec()];
    argv.extend(c.argv.iter().map(|a| a.as_bytes().to_vec()));
    let mut env: Vec<(&String, &String)> = Vec::new();
    for (k, v) in &c.env {
        match env.iter_mut().find(|(ek, _)| *ek == k) {
            Some(slot) => slot.1 = v,
            None => env.push((k, v)),
        }
    }
    let envp = env
        .into_iter()
        .map(|(k, v)| {
            let mut s = k.as_bytes().to_vec();
            s.push(b'=');
            s.extend_from_slice(v.as_bytes());
            s
        })
        .collect();
    (argv, envp)
}

fn prot_of(f: elf::Flags) -> ProtFlags {
    let mut p = ProtFlags::empty();
    if f.read {
        p |= ProtFlags::READ;
    }
    if f.write {
        p |= ProtFlags::WRITE;
    }
    if f.exec {
        p |= ProtFlags::EXEC;
    }
    p
}

/// Load `/app` and run it as thread 0. Never returns.
pub fn run(bi: &'static BootInfo, c: &Cmdline) -> ! {
    let Some(rd) = bi.initrd else {
        fatal(127, format_args!("no initrd"));
    };
    // SAFETY: the initrd is RAM reserved from the frame allocator
    // (MemKind::Initrd) and covered by the direct map; it is never written.
    let data: &'static [u8] =
        unsafe { core::slice::from_raw_parts(phys_to_virt(rd.start).as_ptr::<u8>(), rd.len()) };
    if let Err(err) = crate::fs::rootfs::init(Some(data), c) {
        fatal(
            127,
            format_args!("initrd is not a valid newc cpio archive ({})", err),
        );
    }
    let file = find_app();
    let img = match elf::parse(file, MACHINE) {
        Ok(i) => i,
        Err(e) => fatal(126, format_args!("/app: {}", e)),
    };

    let strace = matches!(c.get("patina.strace"), Some(None) | Some(Some("1")));
    let novdso = matches!(c.get("patina.novdso"), Some(None) | Some(Some("1")));
    if matches!(c.get("patina.stats"), Some(None) | Some(Some("1"))) {
        crate::STATS.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    let epoch_ns = c.epoch_ns.unwrap_or(0);
    crate::random::seed_boot(arch::timer::now_ns() ^ epoch_ns.rotate_left(32));

    let mut space =
        AddressSpace::new().unwrap_or_else(|| fatal(127, format_args!("out of memory")));

    // PT_LOAD segments, eager.
    let mut nseg = 0;
    for seg in img.segments() {
        let (s, e) = seg.page_range();
        let (s, e) = (s as usize, e as usize);
        if let Err(err) = space.add_vma(s, e, prot_of(seg.flags), VmaKind::Elf) {
            fatal(
                126,
                format_args!("/app: cannot map segment {:#x}-{:#x}: {}", s, e, err),
            );
        }
        let bytes = img.data(&seg);
        let fstart = seg.vaddr as usize;
        let fend = fstart + bytes.len();
        for page in (s..e).step_by(PAGE_SIZE) {
            let pa = frame::alloc_frames_zeroed(0)
                .unwrap_or_else(|| fatal(127, format_args!("out of memory loading /app")));
            let lo = page.max(fstart);
            let hi = (page + PAGE_SIZE).min(fend);
            if lo < hi {
                // SAFETY: a fresh frame via the direct map; [lo-page, hi-page)
                // lies within it.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(lo - fstart),
                        phys_to_virt(pa).as_mut_ptr::<u8>().add(lo - page),
                        hi - lo,
                    )
                };
            }
            if let Err(err) = space.map_filled(page, pa) {
                fatal(
                    127,
                    format_args!("cannot map /app page {:#x}: {}", page, err),
                );
            }
        }
        nseg += 1;
    }
    space.set_brk(img.highest_mapped_vaddr() as usize);

    // Stack.
    let stack_lo = STACK_TOP - STACK_SIZE;
    space
        .add_vma(
            stack_lo,
            STACK_TOP,
            ProtFlags::READ | ProtFlags::WRITE,
            VmaKind::Stack,
        )
        .unwrap_or_else(|e| fatal(127, format_args!("cannot create the stack: {}", e)));

    // vDSO + vvar (D75), unless patina.novdso=1.
    let vdso_base = if novdso {
        None
    } else {
        let b = crate::vdso::init(epoch_ns)
            .then(|| crate::vdso::map(&mut space))
            .flatten();
        if b.is_none() {
            crate::kprintln!("[patina] loader: cannot map the vDSO, running without it");
        }
        b
    };

    let (argv, envp) = argv_envp(c);
    let (hwcap, hwcap2) = arch::cpu::hwcap();
    let mut auxv: Vec<(u64, u64)> = Vec::with_capacity(16);
    if let Some(b) = vdso_base {
        auxv.push((AT_SYSINFO_EHDR, b as u64));
    }
    auxv.extend_from_slice(&[
        (AT_PAGESZ, PAGE_SIZE as u64),
        (AT_PHDR, img.phdr_vaddr),
        (AT_PHENT, img.phent as u64),
        (AT_PHNUM, img.phnum as u64),
        (AT_ENTRY, img.entry),
        (AT_HWCAP, hwcap),
        (AT_HWCAP2, hwcap2),
        (AT_CLKTCK, crate::trap::TICK_HZ),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
    ]);
    // AT_RANDOM: see random.rs (no entropy source before Phase 3).
    let mut random = [0u8; 16];
    crate::random::fill(&mut random);
    let argv_refs: Vec<&[u8]> = argv.iter().map(|a| a.as_slice()).collect();
    let envp_refs: Vec<&[u8]> = envp.iter().map(|a| a.as_slice()).collect();
    let image = stack::build(
        STACK_TOP as u64,
        &argv_refs,
        &envp_refs,
        &auxv,
        &random,
        patina_abi::current::PLATFORM.as_bytes(),
        EXE_PATH,
    )
    .unwrap_or_else(|e| fatal(127, format_args!("cannot build the initial stack: {}", e)));
    if (image.sp as usize) < stack_lo + PAGE_SIZE {
        fatal(
            127,
            format_args!("argv/envp do not fit on the initial stack"),
        );
    }
    space
        .copy_to_user(image.sp as usize, &image.bytes)
        .unwrap_or_else(|e| fatal(127, format_args!("cannot write the initial stack: {}", e)));

    let argv_show: Vec<&str> = argv
        .iter()
        .map(|a| core::str::from_utf8(a).unwrap_or("<non-utf8>"))
        .collect();
    crate::kprintln!(
        "[patina] loader: /app {} entry {:#x} phdr {:#x} {} segments, stack {:#x}-{:#x} sp {:#x}, {}, argv {:?}{}",
        arch::NAME,
        img.entry,
        img.phdr_vaddr,
        nseg,
        stack_lo,
        STACK_TOP,
        image.sp,
        match vdso_base {
            Some(_) => "vdso on",
            None => "vdso off",
        },
        argv_show,
        if strace { ", strace on" } else { "" }
    );

    // Commit limit (D78): the frames the program can still get (free now
    // plus the image pages it already holds) minus a kernel reserve of
    // 1/16 of RAM, at least 4 MiB and at most 64 MiB (page tables, kernel
    // stacks, pipe/socket buffers, heap growth).
    let (free, total) = frame::stats();
    let reserve = (total / 16).clamp(1024, 16384);
    let limit = (free + space.resident_pages()).saturating_sub(reserve);
    space.set_commit_limit(limit);
    crate::kprintln!(
        "[patina] memory: commit limit {} MiB ({} MiB kernel reserve)",
        limit * PAGE_SIZE >> 20,
        reserve * PAGE_SIZE >> 20
    );

    let frame = ctx::TrapFrame::new_user(img.entry, image.sp);
    task::set_process(Process {
        pid: 1,
        space: SpinLock::new(space),
        sigactions: SpinLock::new([SigAction::default(); NSIG_SLOTS]),
        argv,
        envp,
        exe_path: EXE_PATH,
        strace,
        epoch_ns,
        files: SpinLock::new(crate::fs::FdTable::with_console()),
        cwd: SpinLock::new(crate::fs::vfs::root_ref()),
        umask: core::sync::atomic::AtomicU32::new(0o022),
    });
    let t0 = task::create_thread0(frame);
    crate::ctl::ready(patina_loader::buildinfo::go_version(file).unwrap_or(""));
    task::start_thread0(t0)
}
