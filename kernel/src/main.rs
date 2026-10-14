//! patina kernel: arch-neutral core.
//!
//! Boot flow: `arch::<arch>::boot` (asm entry -> `early_main`) discovers the
//! machine, sets up paging, the console, the frame allocator and the heap,
//! then calls [`kernel_main`] with a [`BootInfo`].

#![no_std]
#![no_main]
// Phase 0: much of the arch interface is defined ahead of its first user.
#![allow(dead_code)]

extern crate alloc;

mod arch;
mod bootinfo;
mod cmdline;
mod console;
mod loader;
mod mm;
mod panic;
mod random;
mod selftest;
mod selftest_user;
mod sync;
mod syscall;
mod task;
mod trap;

use alloc::format;

use bootinfo::{BootInfo, PlatformInfo};

/// Exit sentinel printed before poweroff; the host scripts / CLI parse it.
pub fn exit(code: i32) -> ! {
    kprint!("\n[patina] exit code {}\n", code);
    arch::power::poweroff(code)
}

/// The process was terminated by signal `sig` (`SIG_DFL` terminate/core):
/// print `[patina] exit signal <NAME>` (the future control-channel `exit`
/// event's `signal` field, design §10) and exit with `128 + sig` through
/// the normal exit path.
pub fn exit_signal(sig: i32) -> ! {
    match patina_abi::signal::name(sig) {
        Some(n) => kprint!("\n[patina] exit signal {}\n", n),
        None => kprint!("\n[patina] exit signal {}\n", sig),
    }
    exit(128 + sig)
}

fn human(bytes: usize) -> alloc::string::String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    const GIB: usize = 1024 * MIB;
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{} MiB", bytes / MIB) + if !bytes.is_multiple_of(MIB) { "+" } else { "" }
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB) + if !bytes.is_multiple_of(KIB) { "+" } else { "" }
    } else {
        format!("{} B", bytes)
    }
}

pub fn kernel_main(bi: &'static BootInfo) -> ! {
    let banner = format!(
        "[patina] patina-kernel {} ({})",
        env!("CARGO_PKG_VERSION"),
        arch::NAME
    );
    kprintln!("{}", banner);

    kprintln!("[patina] memory map:");
    for r in bi.memory.iter() {
        kprintln!(
            "[patina]   {:#014x}-{:#014x} {:>10} {}",
            r.start.0,
            r.end.0,
            human(r.len()),
            r.kind.name()
        );
    }
    kprintln!(
        "[patina] ram: total {} ({} bytes), usable {} ({} bytes)",
        human(bi.memory.total_bytes()),
        bi.memory.total_bytes(),
        human(bi.memory.usable_bytes()),
        bi.memory.usable_bytes()
    );
    kprintln!(
        "[patina] kernel: {:#x}-{:#x} ({})",
        bi.kernel.start.0,
        bi.kernel.end.0,
        human(bi.kernel.len())
    );
    kprintln!("[patina] cmdline: \"{}\"", bi.cmdline);
    let cl = patina_cmdline::Cmdline::parse(bi.cmdline);
    let params: alloc::vec::Vec<&str> = cmdline::params(bi.cmdline).collect();
    kprintln!("[patina] params: {:?}", params);
    kprintln!("[patina] argv: {:?}", {
        let mut v = alloc::vec![alloc::string::String::from("/app")];
        v.extend(cl.argv.iter().cloned());
        v
    });
    for e in &cl.errors {
        kprintln!(
            "[patina] cmdline: ignoring {}={:?}: {}",
            e.key,
            e.value,
            e.reason
        );
    }
    match bi.initrd {
        Some(r) => kprintln!(
            "[patina] initrd: {:#x}-{:#x} ({} bytes)",
            r.start.0,
            r.end.0,
            r.len()
        ),
        None => kprintln!("[patina] initrd: none"),
    }
    match bi.platform {
        PlatformInfo::Dtb(pa) => kprintln!("[patina] platform: dtb @ {:#x}", pa.0),
        PlatformInfo::StartInfo(pa) => kprintln!("[patina] platform: pvh start_info @ {:#x}", pa.0),
    }
    let (free, total) = mm::frame::stats();
    let (_, heap_used, heap_total) = mm::heap::stats();
    kprintln!(
        "[patina] frames: {} free / {} total ({} free); heap: {} used / {}",
        free,
        total,
        human(free * mm::PAGE_SIZE),
        human(heap_used),
        human(heap_total)
    );

    selftest::run(bi.cmdline);

    if bi.initrd.is_some() {
        loader::run(bi, &cl);
    }
    exit(0)
}
