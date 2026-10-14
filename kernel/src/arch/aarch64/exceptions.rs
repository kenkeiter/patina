//! EL1 exception vectors and the EL0 entry/exit paths.
//!
//! Every vector saves a full [`TrapFrame`] (x0-x30, ELR/SPSR/ESR/FAR,
//! TPIDR_EL0, FPCR/FPSR, q0-q31) on SP_EL1 and calls into Rust:
//!
//! * **From EL0** (`__el0_entry` → [`el0_handler`]): SVC → `trap::syscall`,
//!   aborts → `trap::user_fault`, other sync exceptions →
//!   `trap::user_exception`, IRQ → GIC dispatch / `trap::timer_tick(.., true)`.
//!   The `sp` slot holds SP_EL0. Sync exceptions run their hook with IRQs
//!   unmasked; IRQs run with IRQs masked. Afterwards IRQs are masked,
//!   `trap::before_return_to_user` runs, SPSR is sanitized and
//!   `__ret_to_el0` restores everything (incl. FP, TPIDR_EL0, SP_EL0) and
//!   `eret`s. SP_EL1 after the `eret` equals the frame's address +
//!   `FRAME_SIZE`, so the next entry from EL0 reuses the same slot.
//! * **From EL1** (`__el1_entry` → [`el1_handler`]): IRQ → GIC dispatch /
//!   `trap::timer_tick(.., false)` and return (FP state of the interrupted
//!   kernel code is restored too); everything else is a kernel bug: register
//!   dump through `console::panic_write_fmt`, then `panic!`. The `sp` slot
//!   holds SP_EL1 before the exception.
//!
//! # Kernel stack overflow (decision D15)
//!
//! The EL1h sync vector checks the stack before touching it. It uses
//! TPIDRRO_EL0 as a scratch register (zeroed again on the normal path) and:
//! * `sp` not 16-aligned → overflow path;
//! * otherwise it probes `sp - OVERFLOW_PROBE` (8 KiB) with `AT S1E1W` and
//!   takes the overflow path if PAR_EL1.F reports that the address does not
//!   translate as writable. Kernel stacks are contiguous mappings with an
//!   unmapped guard page below, so a successful probe means the frame (832
//!   bytes) plus the fatal-dump code have at least 8 KiB of mapped stack.
//!   This works for any stack size and alignment (no Linux-style
//!   stack-size-aligned `tbnz` trick needed) and it catches both an `sp` that
//!   is already inside the guard page and one just above it.
//!
//! The overflow path switches to this CPU's 16 KiB static overflow stack
//! (`OVERFLOW_STACKS[cpu::id()]`, CPU id from TPIDR_EL1; ids beyond
//! `MAX_CPUS` share the last stack), stores the original sp in the frame and
//! dumps. EL1 sync exceptions are always fatal, so clobbering TPIDR_EL0 /
//! TPIDRRO_EL0 as scratch on this path is harmless. A fault while already on
//! the overflow stack (with the probe failing again) halts the CPU instead
//! of looping.
//!
//! # Phase 2 notes
//! Per-CPU data is reached from asm through TPIDR_EL1 (today: the CPU index;
//! Phase 2 can store a per-CPU struct pointer there and keep `cpu::id()` as a
//! load from it, then index the overflow stack through that struct). SP_EL0
//! is dead while in the kernel (user sp lives in the frame), so it is free
//! for a `current` pointer as on Linux.

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;

use super::ctx::{self, FRAME_SIZE, OFF_ELR, OFF_ESR, OFF_FPCR, OFF_SP, OFF_TPIDR, OFF_V, TrapFrame};
use super::{cpu, irq};
use crate::console::panic_write_fmt;
use crate::trap::{self, Access, Exception, FaultKind};

/// Bytes of mapped stack required below sp on EL1h sync entry.
const OVERFLOW_PROBE: usize = 8192;
/// Per-CPU overflow stack size.
const OVERFLOW_STACK_SIZE: usize = 16 * 1024;
/// Kind code passed for the EL1h sync overflow path.
const KIND_OVERFLOW: u64 = 16;

#[repr(C, align(16))]
struct OverflowStacks(UnsafeCell<[[u8; OVERFLOW_STACK_SIZE]; cpu::MAX_CPUS]>);
// SAFETY: only used as raw stack memory by the vector asm, one stack per CPU.
unsafe impl Sync for OverflowStacks {}

#[unsafe(no_mangle)]
static __patina_overflow_stacks: OverflowStacks =
    OverflowStacks(UnsafeCell::new([[0; OVERFLOW_STACK_SIZE]; cpu::MAX_CPUS]));

global_asm!(
    r#"
// ---- Frame save/restore. sp = frame base; x0/x1 already at [sp, #0].
.macro SAVE_REST
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
    str     x30, [sp, #240]
    mrs     x2, elr_el1
    mrs     x3, spsr_el1
    stp     x2, x3, [sp, #{elr}]
    mrs     x2, esr_el1
    mrs     x3, far_el1
    stp     x2, x3, [sp, #{esr}]
    mrs     x2, tpidr_el0
    stp     x2, x0, [sp, #{tpidr}]          // tpidr, orig_x0
    mrs     x2, fpcr
    mrs     x3, fpsr
    stp     x2, x3, [sp, #{fpcr}]
    stp     q0, q1, [sp, #({v} + 0)]
    stp     q2, q3, [sp, #({v} + 32)]
    stp     q4, q5, [sp, #({v} + 64)]
    stp     q6, q7, [sp, #({v} + 96)]
    stp     q8, q9, [sp, #({v} + 128)]
    stp     q10, q11, [sp, #({v} + 160)]
    stp     q12, q13, [sp, #({v} + 192)]
    stp     q14, q15, [sp, #({v} + 224)]
    stp     q16, q17, [sp, #({v} + 256)]
    stp     q18, q19, [sp, #({v} + 288)]
    stp     q20, q21, [sp, #({v} + 320)]
    stp     q22, q23, [sp, #({v} + 352)]
    stp     q24, q25, [sp, #({v} + 384)]
    stp     q26, q27, [sp, #({v} + 416)]
    stp     q28, q29, [sp, #({v} + 448)]
    stp     q30, q31, [sp, #({v} + 480)]
.endm

// Restore FP/SIMD, ELR/SPSR and x0-x30 from the frame at sp; pop it.
.macro RESTORE_COMMON
    ldp     x2, x3, [sp, #{fpcr}]
    msr     fpcr, x2
    msr     fpsr, x3
    ldp     q0, q1, [sp, #({v} + 0)]
    ldp     q2, q3, [sp, #({v} + 32)]
    ldp     q4, q5, [sp, #({v} + 64)]
    ldp     q6, q7, [sp, #({v} + 96)]
    ldp     q8, q9, [sp, #({v} + 128)]
    ldp     q10, q11, [sp, #({v} + 160)]
    ldp     q12, q13, [sp, #({v} + 192)]
    ldp     q14, q15, [sp, #({v} + 224)]
    ldp     q16, q17, [sp, #({v} + 256)]
    ldp     q18, q19, [sp, #({v} + 288)]
    ldp     q20, q21, [sp, #({v} + 320)]
    ldp     q22, q23, [sp, #({v} + 352)]
    ldp     q24, q25, [sp, #({v} + 384)]
    ldp     q26, q27, [sp, #({v} + 416)]
    ldp     q28, q29, [sp, #({v} + 448)]
    ldp     q30, q31, [sp, #({v} + 480)]
    ldp     x2, x3, [sp, #{elr}]
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
.endm

.macro VECTOR_EL1 kind
    .balign 0x80
    sub     sp, sp, #{frame}
    stp     x0, x1, [sp, #0]
    mov     x1, #\kind
    b       __el1_entry
.endm

.macro VECTOR_EL0 kind
    .balign 0x80
    sub     sp, sp, #{frame}
    stp     x0, x1, [sp, #0]
    mov     x1, #\kind
    b       __el0_entry
.endm

    .section .text.vectors, "ax"
    .balign 0x800
    .global __exception_vectors
__exception_vectors:
    VECTOR_EL1 0    // current EL, SP_EL0: sync
    VECTOR_EL1 1    //                     irq
    VECTOR_EL1 2    //                     fiq
    VECTOR_EL1 3    //                     serror

    // current EL, SP_ELx: sync. Stack check before touching the stack.
    .balign 0x80
    msr     tpidrro_el0, x0             // scratch (EL1 sync is always fatal)
    mov     x0, sp
    tst     x0, #0xf
    b.ne    __el1_overflow
    // Probe sp-16, sp-4096 and sp-8192: every page of [sp-8192, sp) holds
    // one of them, so a single-page guard cannot be skipped.
    sub     x0, x0, #16
    at      s1e1w, x0
    isb
    mrs     x0, par_el1
    tbnz    x0, #0, __el1_overflow      // PAR_EL1.F: no writable translation
    mov     x0, sp
    sub     x0, x0, #4096
    at      s1e1w, x0
    isb
    mrs     x0, par_el1
    tbnz    x0, #0, __el1_overflow
    mov     x0, sp
    sub     x0, x0, #{probe}
    at      s1e1w, x0
    isb
    mrs     x0, par_el1
    tbnz    x0, #0, __el1_overflow
    mrs     x0, tpidrro_el0
    msr     tpidrro_el0, xzr
    sub     sp, sp, #{frame}
    stp     x0, x1, [sp, #0]
    mov     x1, #4
    b       __el1_entry

    VECTOR_EL1 5    //                     irq
    VECTOR_EL1 6    //                     fiq
    VECTOR_EL1 7    //                     serror
    VECTOR_EL0 8    // lower EL, AArch64:  sync
    VECTOR_EL0 9    //                     irq
    VECTOR_EL0 10   //                     fiq
    VECTOR_EL0 11   //                     serror
    VECTOR_EL0 12   // lower EL, AArch32:  sync
    VECTOR_EL0 13   //                     irq
    VECTOR_EL0 14   //                     fiq
    VECTOR_EL0 15   //                     serror

// ---- Kernel stack overflow: original x0 in TPIDRRO_EL0, sp unusable.
__el1_overflow:
    mov     x0, sp
    msr     tpidr_el0, x0               // original sp
    adrp    x0, __patina_overflow_stacks
    add     x0, x0, :lo12:__patina_overflow_stacks
    mov     sp, x0
    mrs     x0, tpidr_el1               // cpu id
    cmp     x0, #({max_cpus} - 1)
    b.ls    1f
    mov     x0, #({max_cpus} - 1)
1:  add     x0, x0, #1
    lsl     x0, x0, #{ovf_shift}
    add     sp, sp, x0                  // top of this CPU's overflow stack
    // Already on this overflow stack (a fault in the dump)? Halt.
    mrs     x0, tpidr_el0
    sub     x0, sp, x0                  // top - original sp
    cmp     x0, #{ovf_size}
    b.ls    __patina_exception_hang
    sub     sp, sp, #{frame}
    str     x1, [sp, #8]
    mrs     x0, tpidrro_el0
    str     x0, [sp, #0]
    SAVE_REST
    mrs     x2, tpidr_el0
    str     x2, [sp, #{sp_off}]
    mov     x0, sp
    mov     x1, #{kind_overflow}
    bl      {el1_handler}
    b       __patina_exception_hang

__patina_exception_hang:
    msr     daifset, #0xf
    wfi
    b       __patina_exception_hang

// ---- Entry from EL1 (x1 = vector kind).
__el1_entry:
    SAVE_REST
    add     x2, sp, #{frame}
    str     x2, [sp, #{sp_off}]
    mov     x0, sp
    bl      {el1_handler}               // returns only for IRQs
    msr     daifset, #0xf
    RESTORE_COMMON
    eret

// ---- Entry from EL0 (x1 = vector kind).
__el0_entry:
    SAVE_REST
    mrs     x2, sp_el0
    str     x2, [sp, #{sp_off}]
    mov     x0, sp
    bl      {el0_handler}               // returns with IRQs masked
    b       __ret_to_el0

// ---- First entry to EL0: x0 = *const TrapFrame (IRQs masked by caller).
    .global __patina_enter_user
__patina_enter_user:
    sub     sp, sp, #{frame}
    mov     x1, sp
    mov     x2, #({frame} / 16)
1:  ldp     x3, x4, [x0], #16
    stp     x3, x4, [x1], #16
    subs    x2, x2, #1
    b.ne    1b
    mov     x29, #0
    mov     x0, sp
    bl      {prepare_user}
    // fall through

// ---- Return to EL0 from the frame at sp (IRQs masked).
__ret_to_el0:
    msr     daifset, #0xf
    ldr     x2, [sp, #{tpidr}]
    msr     tpidr_el0, x2
    ldr     x2, [sp, #{sp_off}]
    msr     sp_el0, x2
    RESTORE_COMMON
    eret
"#,
    frame = const FRAME_SIZE,
    elr = const OFF_ELR,
    esr = const OFF_ESR,
    tpidr = const OFF_TPIDR,
    fpcr = const OFF_FPCR,
    v = const OFF_V,
    sp_off = const OFF_SP,
    probe = const OVERFLOW_PROBE,
    max_cpus = const cpu::MAX_CPUS,
    ovf_size = const OVERFLOW_STACK_SIZE,
    ovf_shift = const OVERFLOW_STACK_SIZE.trailing_zeros(),
    kind_overflow = const KIND_OVERFLOW,
    el1_handler = sym el1_handler,
    el0_handler = sym el0_handler,
    prepare_user = sym prepare_user,
);

const _: () = assert!(OVERFLOW_STACK_SIZE.is_power_of_two());

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

/// Fatal kernel exception: register dump, then panic.
fn fatal(tf: &TrapFrame, kind: u64) -> ! {
    let overflow = kind == KIND_OVERFLOW;
    let kind = if overflow { 4 } else { kind };
    let ec = tf.esr >> 26;
    let iss = tf.esr & 0x1ff_ffff;
    let name = KIND_NAMES.get(kind as usize).copied().unwrap_or("?");
    panic_write_fmt(format_args!(
        "\n[patina] unhandled exception: {} on cpu {}: {} (EC={:#04x})\n",
        name,
        cpu::id(),
        ec_name(ec),
        ec
    ));
    if overflow {
        panic_write_fmt(format_args!(
            "  kernel stack overflow (sp {:#x}); running on the overflow stack\n",
            tf.sp
        ));
    }
    if let Some((fault, rw)) = abort_detail(ec, iss) {
        let level = iss & 0x3;
        panic_write_fmt(format_args!("  {} on {} at level {}\n", fault, rw, level));
    }
    tf.write_dump(&mut |a| panic_write_fmt(a));
    panic!(
        "unhandled {} exception: {}{} (ESR={:#x} FAR={:#x} ELR={:#x})",
        name,
        ec_name(ec),
        if overflow { ", kernel stack overflow" } else { "" },
        tf.esr,
        tf.far,
        tf.elr
    );
}

/// Rust side of the EL1 vectors (and the overflow path).
extern "C" fn el1_handler(tf: &mut TrapFrame, kind: u64) {
    match kind {
        5 => irq::handle(tf, false),
        _ => fatal(tf, kind),
    }
}

/// Rust side of the EL0 vectors. Returns with IRQs masked, ready for
/// `__ret_to_el0`.
extern "C" fn el0_handler(tf: &mut TrapFrame, kind: u64) {
    match kind {
        8 => {
            irq::enable();
            el0_sync(tf);
        }
        9 => irq::handle(tf, true),
        12 => {
            // AArch32 is never entered; treat as an illegal state.
            irq::enable();
            trap::user_exception(tf, Exception::IllegalInstruction);
        }
        13 => irq::handle(tf, true),
        _ => {
            // FIQ / SError from EL0: not expected on any supported VMM.
            fatal(tf, kind)
        }
    }
    prepare_user(tf);
}

/// Mask IRQs, run the return-to-user hook, sanitize PSTATE.
extern "C" fn prepare_user(tf: &mut TrapFrame) {
    irq::disable();
    trap::before_return_to_user(tf);
    irq::disable();
    ctx::sanitize_user(tf);
}

fn el0_sync(tf: &mut TrapFrame) {
    let esr = tf.esr;
    let ec = esr >> 26;
    let iss = esr & 0x1ff_ffff;
    match ec {
        0x15 => trap::syscall(tf),
        0x20 | 0x24 => {
            let access = if ec == 0x20 {
                Access::Exec
            } else if iss & (1 << 6) != 0 {
                Access::Write
            } else {
                Access::Read
            };
            let kind = match iss & 0x3f {
                0x04..=0x07 => FaultKind::NotPresent,
                0x0c..=0x0f => FaultKind::Permission,
                0x21 => FaultKind::Alignment,
                _ => FaultKind::Other,
            };
            trap::user_fault(tf, tf.far as usize, access, kind);
        }
        0x22 => trap::user_fault(tf, tf.far as usize, Access::Exec, FaultKind::Alignment),
        0x26 => trap::user_fault(tf, tf.sp as usize, Access::Read, FaultKind::Alignment),
        0x18 if emulate_mrs(tf, iss) => {}
        0x00 | 0x0e => trap::user_exception(tf, Exception::IllegalInstruction),
        0x3c => trap::user_exception(tf, Exception::Breakpoint),
        0x2c => trap::user_exception(tf, Exception::FloatingPoint),
        _ => trap::user_exception(tf, Exception::Other { code: esr }),
    }
}

/// Emulate an EL0 `MRS` of an ID register (EC 0x18 with FEAT_IDST), as
/// Linux does when it advertises `HWCAP_CPUID` (Go reads MIDR_EL1 that way).
/// Values: MIDR_EL1 real, MPIDR_EL1 = bit 31 only, REVIDR_EL1 = 0,
/// ID_AA64ISAR0_EL1 real, ID_AA64PFR0_EL1 restricted to EL0/EL1 (AArch64
/// only), FP, AdvSIMD and DIT; every other register in the ID space reads 0
/// ("not implemented"). Returns false if the trapped access is not an ID
/// register read.
fn emulate_mrs(tf: &mut TrapFrame, iss: u64) -> bool {
    let read = iss & 1 == 1;
    let op0 = (iss >> 20) & 3;
    let op2 = (iss >> 17) & 7;
    let op1 = (iss >> 14) & 7;
    let crn = (iss >> 10) & 0xf;
    let rt = ((iss >> 5) & 0x1f) as usize;
    let crm = (iss >> 1) & 0xf;
    if !read || op0 != 3 || op1 != 0 || crn != 0 {
        return false;
    }
    let v: u64 = match (crm, op2) {
        (0, 0) => cpu::read_midr(),
        (0, 5) => 1 << 31,
        (0, 6) => 0,
        (6, 0) => cpu::read_id_aa64isar0(),
        (4, 0) => {
            let pfr0 = cpu::read_id_aa64pfr0();
            (pfr0 & ((0xff << 16) | (0xf << 48))) | 0x11
        }
        (0..=7, _) => 0,
        _ => return false,
    };
    if rt != 31 {
        tf.x[rt] = v;
    }
    tf.elr += 4;
    true
}

/// Install the vector table on this CPU (boot code does it for the boot CPU).
pub fn install() {
    // SAFETY: __exception_vectors is the 2 KiB aligned table above.
    unsafe {
        asm!(
            "adrp {t}, __exception_vectors",
            "add {t}, {t}, :lo12:__exception_vectors",
            "msr vbar_el1, {t}",
            "isb",
            t = out(reg) _,
            options(nostack, preserves_flags)
        )
    };
}
