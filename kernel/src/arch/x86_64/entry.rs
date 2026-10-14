//! Kernel entry/exit asm: exception and IRQ stubs, SYSCALL entry, the
//! common return-to-user path and the first entry to ring 3.
//!
//! Every entry builds a [`TrapFrame`] on the kernel stack (16-byte aligned)
//! and calls a Rust handler with `rdi = &mut TrapFrame`:
//!
//! * Exceptions / IRQs (`__isr<N>`): the CPU switches to TSS.rsp0 when it
//!   came from ring 3 (IST1 for #DF), pushes ss/rsp/rflags/cs/rip (+ error
//!   code); the stub pushes a dummy error code if needed, the vector, the 15
//!   GPRs, `orig_rax = -1` and an `fs_base` slot. If CS.RPL = 3 it then does
//!   `swapgs`. Handler: `exceptions::handle_trap`, IRQs masked (interrupt
//!   gates).
//! * SYSCALL (`__patina_syscall_entry`, LSTAR): `swapgs`, park the user RSP
//!   in `PerCpu.user_rsp_scratch`, load `PerCpu.kernel_stack_top`, push the
//!   same words the CPU would push for an interrupt from ring 3 (ss =
//!   USER_DS, rsp, rflags = r11, cs = USER_CS, rip = rcx), error 0, vector
//!   [`VECTOR_SYSCALL`], GPRs, `orig_rax = rax`. SFMASK clears IF|TF|DF|AC|NT;
//!   `syscall_entry` re-enables IRQs once the frame is complete.
//!
//! Return: if the frame is from ring 0, pop and `iretq`. Otherwise
//! `__patina_user_return`: `prepare_user_return` (IRQs off,
//! `trap::before_return_to_user`, IRQs off again, write FS base, decide
//! sysret vs iret), `swapgs`, pop the GPRs, then `sysretq` (with the user RSP
//! loaded from the frame) when `TrapFrame::sysret_ok`, else `iretq`.
//!
//! The FS base is read/written in Rust (`cpu::read_fs_base` /
//! `write_fs_base`: RDFSBASE/WRFSBASE when CR4.FSGSBASE is on, else the
//! FS_BASE MSR), so the asm has a single path for both.

use core::arch::global_asm;

use super::cpu::{self, MSR_EFER, MSR_LSTAR, MSR_SFMASK, MSR_STAR};
use super::ctx::{TF_CS, TF_ORIG_RAX, TF_RSP_FROM_VECTOR, TF_SIZE, TrapFrame, VECTOR_SYSCALL};
use super::gdt::{KERNEL_CS, USER_CS, USER_CS32, USER_DS};
use super::percpu::{OFF_KSTACK, OFF_USER_RSP};
use super::{exceptions, irq};

/// LAPIC timer vector (Linux's LOCAL_TIMER_VECTOR).
pub const TIMER_VECTOR: u8 = 0xec;
/// LAPIC spurious-interrupt vector (SVR); no EOI.
pub const SPURIOUS_VECTOR: u8 = 0xff;
/// IPI vectors (`trap::IpiKind::Reschedule` / `Stop` / `TlbFlush`). Above
/// the timer so the LAPIC priority classes order them first (0xf > 0xe).
pub const IPI_RESCHEDULE_VECTOR: u8 = 0xf0;
pub const IPI_STOP_VECTOR: u8 = 0xf1;
pub const IPI_TLB_FLUSH_VECTOR: u8 = 0xf2;

const EFER_SCE: u64 = 1 << 0;
/// RFLAGS bits cleared on SYSCALL: TF | IF | DF | NT | AC.
const SFMASK: u64 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 14) | (1 << 18);

const _: () = {
    // SYSRET: CS = STAR[63:48] + 16, SS = STAR[63:48] + 8 (RPL forced to 3).
    assert!((USER_CS32 & !3) + 16 == USER_CS & !3);
    assert!((USER_CS32 & !3) + 8 == USER_DS & !3);
};

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
.macro PUSH_GPRS
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
.endm
.macro POP_GPRS
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
    ISR_NOERR {timer}
    ISR_NOERR {spurious}
    ISR_NOERR {ipi_resched}
    ISR_NOERR {ipi_stop}
    ISR_NOERR {ipi_tlb}

__isr_common:
    PUSH_GPRS
    subq    $16, %rsp                   // orig_rax, fs_base
    movq    $-1, {tf_orig}(%rsp)
    movq    $0, (%rsp)
    testb   $3, {tf_cs}(%rsp)
    jz      1f
    swapgs                              // from ring 3: kernel GS
1:  cld
    movq    %rsp, %rdi                  // &mut TrapFrame (16-byte aligned)
    call    {handle_trap}
    testb   $3, {tf_cs}(%rsp)
    jnz     __patina_user_return
    addq    $16, %rsp
    POP_GPRS
    addq    $16, %rsp                   // vector, error code
    iretq

    .global __patina_syscall_entry
    .balign 16
__patina_syscall_entry:
    swapgs
    movq    %rsp, %gs:{off_user_rsp}
    movq    %gs:{off_kstack}, %rsp
    pushq   ${user_ds}                  // ss
    pushq   %gs:{off_user_rsp}          // rsp
    pushq   %r11                        // rflags
    pushq   ${user_cs}                  // cs
    pushq   %rcx                        // rip
    pushq   $0                          // error
    pushq   ${vec_syscall}              // vector
    PUSH_GPRS
    pushq   %rax                        // orig_rax
    pushq   $0                          // fs_base (filled in Rust)
    cld
    movq    %rsp, %rdi
    call    {syscall_entry}
    // fall through

    // rsp = &TrapFrame of a ring-3 frame.
    .global __patina_user_return
__patina_user_return:
    movq    %rsp, %rdi
    call    {prepare}                   // IRQs masked from here on
    testl   %eax, %eax                  // flags survive swapgs/lea/pop
    swapgs                              // user GS
    leaq    16(%rsp), %rsp              // fs_base, orig_rax
    POP_GPRS
    jnz     2f
    leaq    16(%rsp), %rsp              // vector, error code
    iretq
2:  movq    {tf_rsp_from_vec}(%rsp), %rsp  // rcx = rip, r11 = rflags
    sysretq

    // rdi = &TrapFrame (anywhere): copy it onto the current stack, return.
    .global __patina_enter_user
__patina_enter_user:
    movq    %rdi, %rsi
    subq    ${tf_size}, %rsp
    andq    $-16, %rsp
    movq    %rsp, %rdi
    movl    ${tf_words}, %ecx
    cld
    rep movsq
    jmp     __patina_user_return

    .section .rodata.isr_table, "a"
    .balign 8
    .global __isr_table
__isr_table:
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    .quad __isr\n
    .endr
    .global __isr_timer
__isr_timer:
    .quad __isr{timer}
    .global __isr_spurious
__isr_spurious:
    .quad __isr{spurious}
    .global __isr_ipi
__isr_ipi:
    .quad __isr{ipi_resched}
    .quad __isr{ipi_stop}
    .quad __isr{ipi_tlb}
"#,
    timer = const TIMER_VECTOR,
    spurious = const SPURIOUS_VECTOR,
    ipi_resched = const IPI_RESCHEDULE_VECTOR,
    ipi_stop = const IPI_STOP_VECTOR,
    ipi_tlb = const IPI_TLB_FLUSH_VECTOR,
    tf_orig = const TF_ORIG_RAX,
    tf_cs = const TF_CS,
    tf_rsp_from_vec = const TF_RSP_FROM_VECTOR,
    tf_size = const TF_SIZE,
    tf_words = const TF_SIZE / 8,
    off_user_rsp = const OFF_USER_RSP,
    off_kstack = const OFF_KSTACK,
    user_ds = const USER_DS,
    user_cs = const USER_CS,
    vec_syscall = const VECTOR_SYSCALL,
    handle_trap = sym exceptions::handle_trap,
    syscall_entry = sym syscall_entry,
    prepare = sym prepare_user_return,
    options(att_syntax)
);

unsafe extern "C" {
    /// Exception stubs for vectors 0-31 (addresses).
    pub(super) static __isr_table: [u64; 32];
    pub(super) static __isr_timer: u64;
    pub(super) static __isr_spurious: u64;
    /// Reschedule and Stop IPI stubs.
    pub(super) static __isr_ipi: [u64; 3];
    fn __patina_syscall_entry();
    pub(super) fn __patina_enter_user(frame: *const TrapFrame) -> !;
}

/// Rust side of SYSCALL: the frame is complete, GS is the kernel's.
extern "C" fn syscall_entry(frame: &mut TrapFrame) {
    frame.fs_base = cpu::read_fs_base();
    // Interrupts are safe from here: the frame is on the kernel stack and
    // GS is the kernel's, so an IRQ is an ordinary kernel-mode interrupt.
    irq::enable();
    crate::trap::syscall(frame);
}

/// Called on every return to ring 3 with `rsp = frame`. Returns 1 if the
/// asm may use `sysretq`, 0 for `iretq`. Leaves IRQs masked.
extern "C" fn prepare_user_return(frame: &mut TrapFrame) -> u32 {
    let _ = irq::save_disable();
    crate::trap::before_return_to_user(frame);
    // The hook may have switched threads and come back with IRQs on.
    let _ = irq::save_disable();
    cpu::write_fs_base(frame.fs_base);
    frame.sysret_ok() as u32
}

/// Enable SYSCALL/SYSRET on this CPU (EFER.SCE, STAR, LSTAR, SFMASK).
pub(super) fn init_syscall() {
    let star = ((USER_CS32 as u64 & !3 | 3) << 48) | ((KERNEL_CS as u64) << 32);
    // SAFETY: architectural MSRs; the entry point is valid and the per-CPU
    // block (GS) with a kernel stack is set before any user code runs.
    unsafe {
        cpu::wrmsr(MSR_STAR, star);
        cpu::wrmsr(MSR_LSTAR, __patina_syscall_entry as *const () as u64);
        cpu::wrmsr(MSR_SFMASK, SFMASK);
        let efer = cpu::rdmsr(MSR_EFER);
        cpu::wrmsr(MSR_EFER, efer | EFER_SCE);
    }
}
