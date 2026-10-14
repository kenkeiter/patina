//! EL1 exception vectors.
//!
//! Every vector saves a full [`TrapFrame`] on the current stack (SP_EL1;
//! Phase 0 has no per-CPU exception stack) and calls [`handle_exception`].
//! In Phase 0 every exception is fatal: the handler prints a register dump
//! (incl. ESR/FAR/ELR/SPSR) and panics. The restore path is already in place
//! for Phase 1 (IRQs, syscalls).

use core::arch::global_asm;

use crate::console::panic_write_fmt;

/// Register state at exception entry. Layout is shared with the asm below.
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp: u64,
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
}

const FRAME_SIZE: usize = core::mem::size_of::<TrapFrame>();
const _: () = assert!(FRAME_SIZE == 288 && FRAME_SIZE.is_multiple_of(16));

global_asm!(
    r#"
.macro VECTOR kind
    .balign 0x80
    sub     sp, sp, #{frame}
    stp     x0, x1, [sp, #0]
    mov     x1, #\kind
    b       __exception_common
.endm

    .section .text.vectors, "ax"
    .balign 0x800
    .global __exception_vectors
__exception_vectors:
    VECTOR 0    // current EL, SP_EL0: sync
    VECTOR 1    //                     irq
    VECTOR 2    //                     fiq
    VECTOR 3    //                     serror
    VECTOR 4    // current EL, SP_ELx: sync
    VECTOR 5    //                     irq
    VECTOR 6    //                     fiq
    VECTOR 7    //                     serror
    VECTOR 8    // lower EL, AArch64:  sync
    VECTOR 9    //                     irq
    VECTOR 10   //                     fiq
    VECTOR 11   //                     serror
    VECTOR 12   // lower EL, AArch32:  sync
    VECTOR 13   //                     irq
    VECTOR 14   //                     fiq
    VECTOR 15   //                     serror

__exception_common:
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    add     x2, sp, #{frame}            // SP before the exception
    stp     x30, x2, [sp, #240]
    mrs     x2, elr_el1
    mrs     x3, spsr_el1
    stp     x2, x3, [sp, #256]
    mrs     x2, esr_el1
    mrs     x3, far_el1
    stp     x2, x3, [sp, #272]
    mov     x0, sp                      // &mut TrapFrame
    // x1 = vector kind
    bl      {handler}

    // Return path (unused while every exception is fatal).
    ldp     x2, x3, [sp, #256]
    msr     elr_el1, x2
    msr     spsr_el1, x3
    ldp     x0, x1, [sp, #0]
    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30, [sp, #240]
    add     sp, sp, #{frame}
    eret
"#,
    frame = const FRAME_SIZE,
    handler = sym handle_exception,
);

const KIND_NAMES: [&str; 16] = [
    "EL1t sync",
    "EL1t IRQ",
    "EL1t FIQ",
    "EL1t SError",
    "EL1h sync",
    "EL1h IRQ",
    "EL1h FIQ",
    "EL1h SError",
    "EL0/64 sync",
    "EL0/64 IRQ",
    "EL0/64 FIQ",
    "EL0/64 SError",
    "EL0/32 sync",
    "EL0/32 IRQ",
    "EL0/32 FIQ",
    "EL0/32 SError",
];

fn ec_name(ec: u64) -> &'static str {
    match ec {
        0x00 => "unknown reason",
        0x01 => "WFI/WFE trap",
        0x07 => "SVE/SIMD/FP access trap",
        0x0e => "illegal execution state",
        0x15 => "SVC (AArch64)",
        0x18 => "MSR/MRS/system instruction trap",
        0x20 => "instruction abort from lower EL",
        0x21 => "instruction abort (same EL)",
        0x22 => "PC alignment fault",
        0x24 => "data abort from lower EL",
        0x25 => "data abort (same EL)",
        0x26 => "SP alignment fault",
        0x2c => "FP exception",
        0x2f => "SError",
        0x30 | 0x31 => "breakpoint",
        0x32 | 0x33 => "software step",
        0x34 | 0x35 => "watchpoint",
        0x3c => "BRK instruction",
        _ => "other",
    }
}

fn abort_detail(ec: u64, iss: u64) -> Option<(&'static str, &'static str)> {
    if !matches!(ec, 0x20 | 0x21 | 0x24 | 0x25) {
        return None;
    }
    let fsc = iss & 0x3f;
    let kind = match fsc {
        0x00..=0x03 => "address size fault",
        0x04..=0x07 => "translation fault",
        0x09..=0x0b => "access flag fault",
        0x0d..=0x0f => "permission fault",
        0x10 => "synchronous external abort",
        0x21 => "alignment fault",
        _ => "other fault",
    };
    let rw = if matches!(ec, 0x24 | 0x25) {
        if iss & (1 << 6) != 0 { "write" } else { "read" }
    } else {
        "fetch"
    };
    Some((kind, rw))
}

/// Rust side of every vector. `kind` is the vector index (0..16).
extern "C" fn handle_exception(tf: &mut TrapFrame, kind: u64) {
    let ec = tf.esr >> 26;
    let iss = tf.esr & 0x1ff_ffff;
    let name = KIND_NAMES.get(kind as usize).copied().unwrap_or("?");
    panic_write_fmt(format_args!(
        "\n[patina] unhandled exception: {} on cpu {}: {} (EC={:#04x})\n",
        name,
        super::cpu::id(),
        ec_name(ec),
        ec
    ));
    if let Some((fault, rw)) = abort_detail(ec, iss) {
        let level = iss & 0x3;
        panic_write_fmt(format_args!("  {} on {} at level {}\n", fault, rw, level));
    }
    panic_write_fmt(format_args!(
        "  ESR  {:#018x}  FAR  {:#018x}\n  ELR  {:#018x}  SPSR {:#018x}\n  SP   {:#018x}\n",
        tf.esr, tf.far, tf.elr, tf.spsr, tf.sp
    ));
    for i in (0..31).step_by(2) {
        if i + 1 < 31 {
            panic_write_fmt(format_args!(
                "  x{:<2}  {:#018x}  x{:<2}  {:#018x}\n",
                i,
                tf.x[i],
                i + 1,
                tf.x[i + 1]
            ));
        } else {
            panic_write_fmt(format_args!("  x{:<2}  {:#018x}\n", i, tf.x[i]));
        }
    }
    panic!(
        "unhandled {} exception: {} (ESR={:#x} FAR={:#x} ELR={:#x})",
        name,
        ec_name(ec),
        tf.esr,
        tf.far,
        tf.elr
    );
}
