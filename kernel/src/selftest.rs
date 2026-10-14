//! Boot-time self-tests selected on the kernel command line, so the host can
//! exercise the panic, fault and allocator paths:
//!
//! * `patina.selftest=panic` — `panic!("selftest")` (expect exit code 255).
//! * `patina.selftest=fault` — read an unmapped kernel address (expect a
//!   register dump with ESR/FAR, then exit code 255).
//! * `patina.selftest=alloc` — frame and heap allocate/free round trips.
//! * `patina.selftest=vm` — VMA operations and uaccess (`mm::vm::selftest`).
//! * `patina.selftest=smp` — secondary CPU bring-up, IPIs, per-CPU ticks and
//!   one-shot deadlines (`arch::smp::selftest`).
//! * `patina.selftest=net` — virtio-net ARP round trips against QEMU's
//!   user-mode network, IRQ path included (`drivers::virtio::net::selftest`;
//!   prints `no device` and exits 0 without a NIC).
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
                "smp" => crate::arch::smp::selftest(),
                "net" => crate::drivers::virtio::net::selftest(),
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

/// Every size class and alignment, the page path, in-place and moving
/// reallocs, frees out of order; then the frames must be back.
fn layouts_test(f0: usize) {
    use core::alloc::Layout;
    let mut blocks: Vec<(*mut u8, Layout, u8)> = Vec::new();
    let mut tag = 0u8;
    for round in 0..4 {
        for size in (1..=9000usize).step_by(37) {
            for align in [1usize, 8, 16, 32, 64, 256, 4096, 16384] {
                if (size + align + round) % 3 != 0 {
                    continue;
                }
                let l = Layout::from_size_align(size, align).unwrap();
                // SAFETY: non-zero size.
                let p = unsafe { alloc::alloc::alloc(l) };
                assert!(!p.is_null(), "selftest alloc: {:?} failed", l);
                assert!((p as usize).is_multiple_of(align), "{:?} misaligned: {:p}", l, p);
                tag = tag.wrapping_add(1);
                // SAFETY: freshly allocated, `size` bytes.
                unsafe { core::ptr::write_bytes(p, tag, size) };
                blocks.push((p, l, tag));
            }
        }
        // Grow or shrink every third block, free every other one.
        for (i, b) in blocks.iter_mut().enumerate() {
            let (p, l, t) = *b;
            if i % 3 != round % 3 {
                continue;
            }
            let new = if i % 2 == 0 { l.size() * 2 + 100 } else { l.size().div_ceil(3) };
            // SAFETY: `p`/`l` are live; `new` is non-zero.
            let q = unsafe { alloc::alloc::realloc(p, l, new) };
            assert!(!q.is_null());
            assert!((q as usize).is_multiple_of(l.align()));
            // SAFETY: the realloc kept the first min(old, new) bytes.
            let ok = (0..l.size().min(new)).all(|k| unsafe { *q.add(k) } == t);
            assert!(ok, "selftest alloc: realloc lost data ({:?} -> {})", l, new);
            // SAFETY: `new` bytes are ours now.
            unsafe { core::ptr::write_bytes(q, t, new) };
            *b = (q, Layout::from_size_align(new, l.align()).unwrap(), t);
        }
        let mut keep = Vec::new();
        for (j, (p, l, t)) in blocks.drain(..).enumerate() {
            // SAFETY: live block of `l.size()` bytes.
            let ok = (0..l.size()).all(|k| unsafe { *p.add(k) } == t);
            assert!(ok, "selftest alloc: block {:?} corrupted", l);
            if j % 2 == round % 2 {
                // SAFETY: allocated with `l`.
                unsafe { alloc::alloc::dealloc(p, l) };
            } else {
                keep.push((p, l, t));
            }
        }
        blocks = keep;
    }
    let n = blocks.len();
    for (p, l, _) in blocks.drain(..) {
        // SAFETY: allocated with `l`.
        unsafe { alloc::alloc::dealloc(p, l) };
    }
    drop(blocks);
    let (f, _) = frame::stats();
    kprintln!(
        "[patina] selftest alloc: layouts: {} blocks left after 4 rounds, all freed; frames free {} (start {})",
        n,
        f,
        f0
    );
    assert!(f0.saturating_sub(f) <= 64, "heap kept {} frames", f0.saturating_sub(f));
}

fn alloc_test() {
    let h0 = heap::stats();
    // Reserved before counting: growing it later would take heap frames.
    let mut blocks: Vec<(PhysAddr, usize)> = Vec::with_capacity(16);
    let (f0, total) = frame::stats();
    kprintln!(
        "[patina] selftest alloc: before: frames {} free / {} total; heap user {} B, total {} B",
        f0,
        total,
        h0.0,
        h0.2
    );

    // Frames: a spread of orders, write a pattern, free in reverse.
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

    // Heap: many small objects, a few large ones (page path), strings and
    // boxes; afterwards the frames must be back in the frame allocator.
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
    // The heap gave the frames back (D87): only kept empty slabs remain.
    assert!(
        f0.saturating_sub(f3) <= 64,
        "heap kept {} frames after freeing everything",
        f0.saturating_sub(f3)
    );
    layouts_test(f0);
    kprintln!("[patina] selftest alloc: ok");
}
