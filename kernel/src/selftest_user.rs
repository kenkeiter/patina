//! Self-tests for the Phase 1 arch contract (`arch/mod.rs`, `trap.rs`):
//! `patina.selftest=user|userfault|timer|switch`.
//!
//! These run before the loader/scheduler exist and exercise the arch layer
//! alone: user page tables, entry to user mode, the syscall and fault paths,
//! the periodic timer, and kernel context switching.
//!
//! * `user`: maps `arch::ctx::selftest_user_code()` at [`CODE_VA`] (R-X), a
//!   data page at [`DATA_VA`] (RW) and a stack page, enters EL0/ring 3. The
//!   user code must `write(1, "selftest user: hello\n", 21)`, store to
//!   `DATA_VA`, then `exit_group(7)`. Expected: the line and exit code 7.
//! * `userfault`: same, but `DATA_VA` is left unmapped, so the store faults.
//!   Expected: the line, then `fatal: user Write fault (NotPresent) at
//!   0x401000`, a register dump, exit code 139.
//! * `timer`: starts the tick and waits for five ticks (bounded by
//!   `timer::now_ns`). Expected: `selftest timer: ok (...)`, exit 0.
//! * `switch`: ping-pongs between the boot context and a new kernel context
//!   on a fresh stack. Expected: `selftest switch: ok`, exit 0.

use core::sync::atomic::Ordering;

use crate::arch::{self, cpu, ctx, paging::MapFlags, timer, uspace};
use crate::mm::{PAGE_SIZE, PhysAddr, frame, phys_to_virt};
use crate::sync::SpinLock;
use crate::trap;

pub const CODE_VA: usize = 0x40_0000;
pub const DATA_VA: usize = 0x40_1000;
pub const STACK_VA: usize = 0x7ff0_0000_0000;

static TABLES: SpinLock<Option<uspace::UserTables>> = SpinLock::new(None);

/// Run the self-test named by `which`; returns false if it is not one of ours.
pub fn run(which: &str) -> bool {
    match which {
        "user" => user(true),
        "userfault" => user(false),
        "timer" => timer_test(),
        "switch" => switch_test(),
        _ => false,
    }
}

fn alloc_page() -> PhysAddr {
    frame::alloc_frames_zeroed(0).expect("selftest: out of frames")
}

fn user(map_data: bool) -> ! {
    let mut tables = uspace::UserTables::new().expect("selftest: UserTables::new");

    let code = ctx::selftest_user_code();
    assert!(code.len() <= PAGE_SIZE, "selftest user code does not fit a page");
    let code_pa = alloc_page();
    // SAFETY: freshly allocated frame, reached through the direct map.
    unsafe {
        core::ptr::copy_nonoverlapping(
            code.as_ptr(),
            phys_to_virt(code_pa).as_mut_ptr::<u8>(),
            code.len(),
        );
    }
    tables
        .map(CODE_VA, code_pa, MapFlags::READ | MapFlags::EXEC)
        .expect("selftest: map code");
    if map_data {
        tables
            .map(DATA_VA, alloc_page(), MapFlags::READ | MapFlags::WRITE)
            .expect("selftest: map data");
    }
    tables
        .map(STACK_VA, alloc_page(), MapFlags::READ | MapFlags::WRITE)
        .expect("selftest: map stack");

    // SAFETY: the tables map only the pages above; the kernel half is
    // untouched (TTBR1 / mirrored PML4 upper half).
    unsafe { tables.activate() };
    *TABLES.lock() = Some(tables);

    let frame = ctx::TrapFrame::new_user(CODE_VA as u64, (STACK_VA + PAGE_SIZE) as u64);
    crate::kprintln!(
        "[patina] selftest {}: entering user mode at {:#x} (data {})",
        if map_data { "user" } else { "userfault" },
        CODE_VA,
        if map_data { "mapped" } else { "unmapped" }
    );
    cpu::set_kernel_stack(arch::layout::boot_stack_top());
    ctx::fp_restore(&ctx::FpState::default());
    // SAFETY: a valid user frame; the user tables are live.
    unsafe { ctx::enter_user(&frame) }
}

/// `write(1/2, va, len)` for the self-test user program: resolves `va`
/// through the self-test tables and prints through the direct map.
pub fn user_write(va: usize, len: usize) -> isize {
    let guard = TABLES.lock();
    let Some(tables) = guard.as_ref() else {
        return patina_abi::errno::Errno(patina_abi::errno::EFAULT).to_ret();
    };
    let mut done = 0usize;
    while done < len {
        let cur = va + done;
        let Some((pa, _)) = tables.translate(cur & !(PAGE_SIZE - 1)) else {
            return patina_abi::errno::Errno(patina_abi::errno::EFAULT).to_ret();
        };
        let off = cur & (PAGE_SIZE - 1);
        let n = (PAGE_SIZE - off).min(len - done);
        // SAFETY: the page is mapped in the user tables, hence RAM reachable
        // through the direct map.
        let bytes = unsafe {
            core::slice::from_raw_parts(phys_to_virt(pa).as_ptr::<u8>().add(off), n)
        };
        crate::kprint!("{}", core::str::from_utf8(bytes).unwrap_or("<non-utf8>"));
        done += n;
    }
    len as isize
}

fn timer_test() -> ! {
    let t0 = timer::now_ns();
    timer::init();
    let limit = t0 + 5_000_000_000;
    loop {
        let ticks = trap::TICKS.load(Ordering::Relaxed);
        if ticks >= 5 {
            let dt = timer::now_ns() - t0;
            crate::kprintln!(
                "[patina] selftest timer: ok ({} ticks in {} ms at {} Hz)",
                ticks,
                dt / 1_000_000,
                trap::TICK_HZ
            );
            crate::exit(0);
        }
        if timer::now_ns() > limit {
            panic!("selftest timer: only {} ticks after 5 s", ticks);
        }
        cpu::wait_for_interrupt();
    }
}

static MAIN_CTX: SpinLock<ctx::KernelContext> = SpinLock::new(ctx::KernelContext::new());
static OTHER_CTX: SpinLock<ctx::KernelContext> = SpinLock::new(ctx::KernelContext::new());

extern "C" fn other_thread(arg: usize) -> ! {
    crate::kprintln!("[patina] selftest switch: in new context (arg {:#x})", arg);
    let other = OTHER_CTX.lock().as_mut_ptr();
    let main = MAIN_CTX.lock().as_ptr();
    // SAFETY: both contexts are valid; main is parked in `switch` below.
    unsafe { ctx::switch(other, main) };
    crate::kprintln!("[patina] selftest switch: resumed new context");
    let other = OTHER_CTX.lock().as_mut_ptr();
    let main = MAIN_CTX.lock().as_ptr();
    // SAFETY: as above.
    unsafe { ctx::switch(other, main) };
    panic!("selftest switch: new context resumed after its last switch");
}

fn switch_test() -> ! {
    const ORDER: usize = 3; // 8 pages = 32 KiB kernel stack
    let stack = frame::alloc_frames(ORDER).expect("selftest: stack frames");
    let top = phys_to_virt(stack).as_usize() + (PAGE_SIZE << ORDER);
    *OTHER_CTX.lock() = ctx::new_kernel_context(top, other_thread, 0x1234);
    let flags = arch::irq::save_disable();
    for round in 0..2 {
        let main = MAIN_CTX.lock().as_mut_ptr();
        let other = OTHER_CTX.lock().as_ptr();
        // SAFETY: `other` was built by new_kernel_context / saved by switch.
        unsafe { ctx::switch(main, other) };
        crate::kprintln!("[patina] selftest switch: back in boot context (round {})", round);
    }
    arch::irq::restore(flags);
    crate::kprintln!("[patina] selftest switch: ok");
    crate::exit(0)
}
