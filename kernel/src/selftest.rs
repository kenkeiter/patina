//! Boot-time self-tests selected on the kernel command line, so the host can
//! exercise the panic, fault and allocator paths:
//!
//! * `patina.selftest=panic` — `panic!("selftest")` (expect exit code 255).
//! * `patina.selftest=fault` — read an unmapped kernel address (expect a
//!   register dump with ESR/FAR, then exit code 255).
//! * `patina.selftest=alloc` — frame and heap allocate/free round trips.
//! * `patina.selftest=vm` — VMA operations and uaccess (`mm::vm::selftest`).
//!
//! Only kernel parameters are considered (see `cmdline::params`): tokens
//! before the first `--`, or raw `key=value` tokens after it.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::kprintln;
use crate::mm::{PhysAddr, frame, heap};

/// Canonical kernel-half address that is never mapped on any arch (inside the
/// direct-map window, ~94 TiB above PHYS_OFFSET).
const UNMAPPED_KERNEL_VA: usize = 0xffff_dead_beef_0000;

pub fn run(cmdline: &str) {
    if let Some(which) = crate::cmdline::param(cmdline, "patina.selftest")
        && crate::selftest_user::run(which)
    {
        return;
    }
    for tok in crate::cmdline::params(cmdline) {
        if let Some(which) = tok.strip_prefix("patina.selftest=") {
            match which {
                "panic" => panic!("selftest"),
                "fault" => fault(),
                "alloc" => alloc_test(),
                "vm" => crate::mm::vm::selftest(),
                other => kprintln!("[patina] selftest: unknown test {:?}", other),
            }
        }
    }
}

fn fault() {
    kprintln!(
        "[patina] selftest: reading unmapped address {:#x}",
        UNMAPPED_KERNEL_VA
    );
    // SAFETY: deliberately faulting; the exception handler never returns here.
    let v = unsafe { core::ptr::read_volatile(UNMAPPED_KERNEL_VA as *const u64) };
    kprintln!("[patina] selftest: fault did NOT fault (read {:#x})", v);
}

fn alloc_test() {
    let (f0, total) = frame::stats();
    let h0 = heap::stats();
    kprintln!(
        "[patina] selftest alloc: before: frames {} free / {} total; heap user {} B, total {} B",
        f0,
        total,
        h0.0,
        h0.2
    );

    // Frames: a spread of orders, write a pattern, free in reverse.
    let mut blocks: Vec<(PhysAddr, usize)> = Vec::new();
    for order in [0usize, 1, 3, 5, 8, 9, 0, 2, 9] {
        let pa = frame::alloc_frames(order).expect("alloc_frames failed");
        assert!(
            pa.0.is_multiple_of(crate::mm::PAGE_SIZE << order),
            "misaligned block"
        );
        let va = crate::mm::phys_to_virt(pa).as_mut_ptr::<u64>();
        // SAFETY: freshly allocated, direct-mapped.
        unsafe {
            va.write_volatile(pa.0 as u64);
            va.add(((crate::mm::PAGE_SIZE << order) / 8) - 1)
                .write_volatile(!(pa.0 as u64));
        }
        blocks.push((pa, order));
    }
    let used: usize = blocks.iter().map(|&(_, o)| 1usize << o).sum();
    let (f1, _) = frame::stats();
    kprintln!(
        "[patina] selftest alloc: {} frame blocks ({} frames, {} KiB) allocated; free now {}",
        blocks.len(),
        used,
        used * 4,
        f1
    );
    assert_eq!(f0 - f1, used, "frame accounting mismatch");
    for &(pa, order) in blocks.iter().rev() {
        let va = crate::mm::phys_to_virt(pa).as_ptr::<u64>();
        // SAFETY: still allocated.
        let ok = unsafe { va.read_volatile() == pa.0 as u64 };
        assert!(ok, "frame contents corrupted");
        frame::free_frames(pa, order);
    }
    drop(blocks);

    // Heap: many small objects, a few large ones (forces growth past the
    // initial chunk), strings and boxes.
    {
        let mut small: Vec<Box<[u64; 4]>> = Vec::new();
        for i in 0..10_000u64 {
            small.push(Box::new([i, i + 1, i + 2, i + 3]));
        }
        let mut big: Vec<Vec<u8>> = Vec::new();
        for i in 0..6 {
            big.push(alloc::vec![i as u8; 3 << 20]);
        }
        let mut s = String::new();
        for i in 0..1000 {
            s.push_str(&alloc::format!("{},", i));
        }
        let h1 = heap::stats();
        let (f2, _) = frame::stats();
        kprintln!(
            "[patina] selftest alloc: during: heap user {} B, total {} B; frames free {}; checks {} {} {}",
            h1.0,
            h1.2,
            f2,
            small[9999][3],
            big[5][(3 << 20) - 1],
            s.len()
        );
        assert_eq!(small[1234][2], 1236);
        assert!(
            big.iter()
                .enumerate()
                .all(|(i, v)| v.iter().all(|&b| b == i as u8))
        );
    }
    let h2 = heap::stats();
    let (f3, _) = frame::stats();
    kprintln!(
        "[patina] selftest alloc: after: frames {} free / {} total; heap user {} B, total {} B",
        f3,
        total,
        h2.0,
        h2.2
    );
    assert_eq!(h2.0, h0.0, "heap leak");
    kprintln!("[patina] selftest alloc: ok");
}
