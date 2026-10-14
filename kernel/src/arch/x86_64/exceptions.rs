//! IDT and CPU exception handling.
//!
//! Vectors 0-31 get `global_asm!` stubs that push a uniform [`TrapFrame`]
//! (a dummy error code for vectors without one, then the vector number, then
//! all GPRs) and call [`handle_exception`]. Vectors 32-255 are left
//! not-present (interrupts stay masked in Phase 0; a stray one raises #NP,
//! which is dumped). #DF runs on IST1 (see `gdt.rs`).
//!
//! In Phase 0 every exception is fatal: the handler prints a register dump
//! through `console::panic_write_fmt` and panics. The `iretq` return path is
//! in place for Phase 1.

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::mem::size_of;

use super::cpu;
use super::gdt::{IST_DOUBLE_FAULT, KERNEL_CS};
use crate::console::panic_write_fmt;

/// Register state at exception entry. Layout is shared with the asm below.
#[repr(C)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    // Pushed by the CPU.
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

const _: () = assert!(size_of::<TrapFrame>() == 176 && size_of::<TrapFrame>().is_multiple_of(16));

/// Vectors for which the CPU pushes an error code.
const fn has_error_code(v: usize) -> bool {
    matches!(v, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30)
}

global_asm!(
    r#"
.macro ISR_NOERR n
    .balign 16
__isr\n:
    pushq   $0
    pushq   $\n
    jmp     __isr_common
.endm
.macro ISR_ERR n
    .balign 16
__isr\n:
    pushq   $\n
    jmp     __isr_common
.endm

    .section .text.isr, "ax"
    ISR_NOERR 0
    ISR_NOERR 1
    ISR_NOERR 2
    ISR_NOERR 3
    ISR_NOERR 4
    ISR_NOERR 5
    ISR_NOERR 6
    ISR_NOERR 7
    ISR_ERR   8
    ISR_NOERR 9
    ISR_ERR   10
    ISR_ERR   11
    ISR_ERR   12
    ISR_ERR   13
    ISR_ERR   14
    ISR_NOERR 15
    ISR_NOERR 16
    ISR_ERR   17
    ISR_NOERR 18
    ISR_NOERR 19
    ISR_NOERR 20
    ISR_ERR   21
    ISR_NOERR 22
    ISR_NOERR 23
    ISR_NOERR 24
    ISR_NOERR 25
    ISR_NOERR 26
    ISR_NOERR 27
    ISR_NOERR 28
    ISR_ERR   29
    ISR_ERR   30
    ISR_NOERR 31

__isr_common:
    pushq   %rax
    pushq   %rbx
    pushq   %rcx
    pushq   %rdx
    pushq   %rsi
    pushq   %rdi
    pushq   %rbp
    pushq   %r8
    pushq   %r9
    pushq   %r10
    pushq   %r11
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15
    cld
    movq    %rsp, %rdi                  // &mut TrapFrame (16-byte aligned)
    call    {handler}
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %r11
    popq    %r10
    popq    %r9
    popq    %r8
    popq    %rbp
    popq    %rdi
    popq    %rsi
    popq    %rdx
    popq    %rcx
    popq    %rbx
    popq    %rax
    addq    $16, %rsp                   // vector, error code
    iretq

    .section .rodata.isr_table, "a"
    .balign 8
    .global __isr_table
__isr_table:
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    .quad __isr\n
    .endr
"#,
    handler = sym handle_exception,
    options(att_syntax)
);

unsafe extern "C" {
    static __isr_table: [u64; 32];
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

    /// Present, DPL0, 64-bit interrupt gate (IF cleared on entry).
    fn interrupt(handler: u64, ist: u8) -> Gate {
        Gate {
            offset_lo: handler as u16,
            selector: KERNEL_CS,
            ist,
            attrs: 0x8e,
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

/// Fill and load the IDT on this CPU.
pub(super) fn init() {
    // SAFETY: single-threaded boot; the stubs are valid handlers.
    unsafe {
        let idt = &mut *IDT.0.get();
        let table = &*core::ptr::addr_of!(__isr_table);
        for (v, &addr) in table.iter().enumerate() {
            let ist = if v == 8 { IST_DOUBLE_FAULT } else { 0 };
            idt[v] = Gate::interrupt(addr, ist);
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

/// Rust side of every exception stub.
extern "C" fn handle_exception(tf: &mut TrapFrame) {
    let v = tf.vector as usize;
    let name = NAMES.get(v).copied().unwrap_or("?");
    let cr2 = cpu::read_cr2();
    let cr3 = cpu::read_cr3();
    panic_write_fmt(format_args!(
        "\n[patina] unhandled exception: {} (vector {}) on cpu {}\n",
        name,
        v,
        cpu::id()
    ));
    if has_error_code(v) {
        panic_write_fmt(format_args!("  error code {:#x}", tf.error));
        if v == 14 {
            let e = tf.error;
            panic_write_fmt(format_args!(
                " ({}, {}, {} mode{}{})",
                if e & 1 != 0 {
                    "protection violation"
                } else {
                    "not present"
                },
                if e & 2 != 0 { "write" } else { "read" },
                if e & 4 != 0 { "user" } else { "kernel" },
                if e & 8 != 0 { ", reserved bit set" } else { "" },
                if e & 16 != 0 {
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
    panic_write_fmt(format_args!(
        "  RIP    {:#018x}  CS  {:#06x}  RFLAGS {:#018x}\n  RSP    {:#018x}  SS  {:#06x}  CR3    {:#018x}\n",
        tf.rip, tf.cs, tf.rflags, tf.rsp, tf.ss, cr3
    ));
    panic_write_fmt(format_args!(
        "  RAX {:#018x}  RBX {:#018x}  RCX {:#018x}\n  RDX {:#018x}  RSI {:#018x}  RDI {:#018x}\n  RBP {:#018x}  R8  {:#018x}  R9  {:#018x}\n  R10 {:#018x}  R11 {:#018x}  R12 {:#018x}\n  R13 {:#018x}  R14 {:#018x}  R15 {:#018x}\n",
        tf.rax,
        tf.rbx,
        tf.rcx,
        tf.rdx,
        tf.rsi,
        tf.rdi,
        tf.rbp,
        tf.r8,
        tf.r9,
        tf.r10,
        tf.r11,
        tf.r12,
        tf.r13,
        tf.r14,
        tf.r15
    ));
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
