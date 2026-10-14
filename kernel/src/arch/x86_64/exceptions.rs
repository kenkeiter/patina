//! IDT and exception / interrupt dispatch.
//!
//! Gates (all 64-bit interrupt gates, selector 0x08, so IF is clear on
//! entry): vectors 0-31 (#BP with DPL 3 so `int3` works from ring 3, #DF on
//! IST1), the LAPIC timer ([`TIMER_VECTOR`]), the three IPI vectors and spurious
//! ([`SPURIOUS_VECTOR`]) vectors. Every other vector stays not-present
//! (a stray one raises #NP / #GP, which is dumped or reported).
//!
//! Entry asm and the frame layout are in `entry.rs`; [`handle_trap`] gets
//! every exception and IRQ:
//! * timer: EOI, re-arm the one-shot (`timer::on_interrupt`), then
//!   `trap::timer_tick` when a tick or deadline was due;
//! * IPIs ([`IPI_RESCHEDULE_VECTOR`], [`IPI_STOP_VECTOR`],
//!   [`IPI_TLB_FLUSH_VECTOR`]): EOI, then `trap::ipi`;
//! * device interrupts (0x30-0x7f, I/O APIC): `irq::dispatch`, then EOI;
//! * spurious: nothing (no EOI);
//! * any exception from ring 0, and NMI / #DF / #MC from anywhere: fatal
//!   register dump through `console::panic_write_fmt`, then `panic!`;
//! * other exceptions from ring 3: IRQs are re-enabled and the fault is
//!   handed to `trap::user_fault` / `trap::user_exception`; the frame then
//!   returns to ring 3 through the common exit path (retry on #PF).

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;

use super::ctx::TrapFrame;
use super::entry::{
    __isr_dev, __isr_ipi, __isr_spurious, __isr_table, __isr_timer, IPI_RESCHEDULE_VECTOR,
    IPI_STOP_VECTOR, IPI_TLB_FLUSH_VECTOR, SPURIOUS_VECTOR, TIMER_VECTOR,
};
use super::gdt::{IST_DOUBLE_FAULT, KERNEL_CS};
use super::{cpu, irq, lapic, timer};
use crate::console::panic_write_fmt;
use crate::trap::{self, Access, Exception, FaultKind, IpiKind};

/// Vectors for which the CPU pushes an error code.
const fn has_error_code(v: usize) -> bool {
    matches!(v, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30)
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Gate {
    offset_lo: u16,
    selector: u16,
    ist: u8,
    attrs: u8,
    offset_mid: u16,
    offset_hi: u32,
    _reserved: u32,
}

const _: () = assert!(size_of::<Gate>() == 16);

impl Gate {
    const EMPTY: Gate = Gate {
        offset_lo: 0,
        selector: 0,
        ist: 0,
        attrs: 0,
        offset_mid: 0,
        offset_hi: 0,
        _reserved: 0,
    };

    /// Present 64-bit interrupt gate (IF cleared on entry) with `dpl`.
    fn interrupt(handler: u64, ist: u8, dpl: u8) -> Gate {
        Gate {
            offset_lo: handler as u16,
            selector: KERNEL_CS,
            ist,
            attrs: 0x8e | (dpl << 5),
            offset_mid: (handler >> 16) as u16,
            offset_hi: (handler >> 32) as u32,
            _reserved: 0,
        }
    }
}

#[repr(C, align(16))]
struct Idt(UnsafeCell<[Gate; 256]>);
// SAFETY: written once during single-threaded boot, then read-only (by the CPU).
unsafe impl Sync for Idt {}

static IDT: Idt = Idt(UnsafeCell::new([Gate::EMPTY; 256]));

#[repr(C, packed)]
struct DescriptorPtr {
    limit: u16,
    base: u64,
}

/// Fill and load the IDT on the boot CPU (secondary CPUs only [`load`] it).
pub(super) fn init() {
    // SAFETY: single-threaded boot; the stubs are valid handlers.
    unsafe {
        let idt = &mut *IDT.0.get();
        let table = &*core::ptr::addr_of!(__isr_table);
        for (v, &addr) in table.iter().enumerate() {
            let ist = if v == 8 { IST_DOUBLE_FAULT } else { 0 };
            let dpl = if v == 3 { 3 } else { 0 };
            idt[v] = Gate::interrupt(addr, ist, dpl);
        }
        idt[TIMER_VECTOR as usize] = Gate::interrupt(__isr_timer, 0, 0);
        idt[SPURIOUS_VECTOR as usize] = Gate::interrupt(__isr_spurious, 0, 0);
        let ipi = &*core::ptr::addr_of!(__isr_ipi);
        idt[IPI_RESCHEDULE_VECTOR as usize] = Gate::interrupt(ipi[0], 0, 0);
        idt[IPI_STOP_VECTOR as usize] = Gate::interrupt(ipi[1], 0, 0);
        idt[IPI_TLB_FLUSH_VECTOR as usize] = Gate::interrupt(ipi[2], 0, 0);
        let dev = &*core::ptr::addr_of!(__isr_dev);
        for (i, &addr) in dev.iter().enumerate() {
            idt[irq::DEV_VECTOR_BASE as usize + i] = Gate::interrupt(addr, 0, 0);
        }
        load();
    }
}

/// Load the (already filled) IDT on the current CPU.
pub(super) fn load() {
    let ptr = DescriptorPtr {
        limit: (size_of::<[Gate; 256]>() - 1) as u16,
        base: IDT.0.get() as u64,
    };
    // SAFETY: the IDT is a static that lives forever.
    unsafe { asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags)) };
}

const NAMES: [&str; 32] = [
    "#DE divide error",
    "#DB debug",
    "NMI",
    "#BP breakpoint",
    "#OF overflow",
    "#BR bound range exceeded",
    "#UD invalid opcode",
    "#NM device not available",
    "#DF double fault",
    "coprocessor segment overrun",
    "#TS invalid TSS",
    "#NP segment not present",
    "#SS stack-segment fault",
    "#GP general protection",
    "#PF page fault",
    "reserved (15)",
    "#MF x87 floating-point error",
    "#AC alignment check",
    "#MC machine check",
    "#XM SIMD floating-point",
    "#VE virtualization exception",
    "#CP control protection",
    "reserved (22)",
    "reserved (23)",
    "reserved (24)",
    "reserved (25)",
    "reserved (26)",
    "reserved (27)",
    "#HV hypervisor injection",
    "#VC VMM communication",
    "#SX security exception",
    "reserved (31)",
];

// #PF error code bits.
const PF_P: u64 = 1 << 0;
const PF_W: u64 = 1 << 1;
const PF_RSVD: u64 = 1 << 3;
const PF_I: u64 = 1 << 4;

/// Rust side of every exception and IRQ stub (see `entry.rs`).
pub(super) extern "C" fn handle_trap(tf: &mut TrapFrame) {
    let v = tf.vector;
    if v == TIMER_VECTOR as u64 {
        let from_user = tf.from_user();
        if from_user {
            tf.fs_base = cpu::read_fs_base();
        }
        // EOI and re-arm first: the hook may switch threads before this
        // one returns.
        lapic::eoi();
        if timer::on_interrupt() {
            trap::timer_tick(tf, from_user);
        }
        return;
    }
    if v == IPI_RESCHEDULE_VECTOR as u64
        || v == IPI_STOP_VECTOR as u64
        || v == IPI_TLB_FLUSH_VECTOR as u64
    {
        if tf.from_user() {
            tf.fs_base = cpu::read_fs_base();
        }
        // EOI first: `Stop` never returns, `Reschedule` may switch.
        lapic::eoi();
        let kind = if v == IPI_STOP_VECTOR as u64 {
            IpiKind::Stop
        } else if v == IPI_TLB_FLUSH_VECTOR as u64 {
            IpiKind::TlbFlush
        } else {
            IpiKind::Reschedule
        };
        trap::ipi(tf, kind);
        return;
    }
    if v == SPURIOUS_VECTOR as u64 {
        return;
    }
    if (irq::DEV_VECTOR_BASE as u64..irq::DEV_VECTOR_END as u64).contains(&v) {
        // The return path may switch threads (a woken thread, need_resched)
        // and reloads FS from the frame.
        if tf.from_user() {
            tf.fs_base = cpu::read_fs_base();
        }
        irq::dispatch(v as u8);
        lapic::eoi();
        return;
    }
    if !tf.from_user() || matches!(v, 2 | 8 | 18) || v >= 32 {
        fatal(tf);
    }

    // From ring 3: GS is already the kernel's.
    tf.fs_base = cpu::read_fs_base();
    let cr2 = cpu::read_cr2();
    irq::enable();
    match v {
        14 => {
            let e = tf.error;
            let access = if e & PF_I != 0 {
                Access::Exec
            } else if e & PF_W != 0 {
                Access::Write
            } else {
                Access::Read
            };
            let kind = if e & PF_RSVD != 0 {
                FaultKind::Other
            } else if e & PF_P == 0 {
                FaultKind::NotPresent
            } else {
                FaultKind::Permission
            };
            trap::user_fault(tf, cr2 as usize, access, kind);
        }
        // #AC reports no address; Linux sends SIGBUS/BUS_ADRALN with none.
        17 => trap::user_fault(tf, 0, Access::Read, FaultKind::Alignment),
        0 => trap::user_exception(tf, Exception::DivideError),
        3 => trap::user_exception(tf, Exception::Breakpoint),
        6 => trap::user_exception(tf, Exception::IllegalInstruction),
        16 | 19 => trap::user_exception(tf, Exception::FloatingPoint),
        _ => trap::user_exception(
            tf,
            Exception::Other {
                code: (v << 32) | (tf.error & 0xffff_ffff),
            },
        ),
    }
}

/// Fatal exception: register dump through the panic console, then panic.
fn fatal(tf: &TrapFrame) -> ! {
    let v = tf.vector as usize;
    let name = NAMES.get(v).copied().unwrap_or("unexpected vector");
    let cr2 = cpu::read_cr2();
    panic_write_fmt(format_args!(
        "\n[patina] unhandled exception: {} (vector {}) in {} mode on cpu {}\n",
        name,
        v,
        if tf.from_user() { "user" } else { "kernel" },
        cpu::id()
    ));
    if has_error_code(v) {
        panic_write_fmt(format_args!("  error code {:#x}", tf.error));
        if v == 14 {
            let e = tf.error;
            panic_write_fmt(format_args!(
                " ({}, {}, {} mode{}{})",
                if e & PF_P != 0 {
                    "protection violation"
                } else {
                    "not present"
                },
                if e & PF_W != 0 { "write" } else { "read" },
                if e & 4 != 0 { "user" } else { "kernel" },
                if e & PF_RSVD != 0 {
                    ", reserved bit set"
                } else {
                    ""
                },
                if e & PF_I != 0 {
                    ", instruction fetch"
                } else {
                    ""
                },
            ));
        }
        panic_write_fmt(format_args!("\n"));
    }
    if v == 14 {
        panic_write_fmt(format_args!("  CR2    {:#018x}\n", cr2));
    }
    tf.dump_with(&mut |a| panic_write_fmt(a));
    if v == 14 {
        panic!(
            "unhandled {} at RIP={:#x} CR2={:#x} error={:#x}",
            name, tf.rip, cr2, tf.error
        );
    }
    panic!(
        "unhandled {} at RIP={:#x} error={:#x}",
        name, tf.rip, tf.error
    );
}
