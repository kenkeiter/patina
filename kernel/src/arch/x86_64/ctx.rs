//! Register state: the user [`TrapFrame`], per-thread [`FpState`], kernel
//! [`KernelContext`] switching and the first entry to ring 3.
//!
//! See `arch/mod.rs` (Phase 1 additions) for the contract and `entry.rs` for
//! the asm that builds and restores frames.

use core::arch::{asm, global_asm};
use core::fmt;
use core::mem::{offset_of, size_of};

use super::cpu;
use super::gdt::{USER_CS, USER_DS};

const RFLAGS_RESERVED1: u64 = 1 << 1;
const RFLAGS_IF: u64 = 1 << 9;

/// Register state at every entry (syscall, exception, interrupt, from ring 3
/// or ring 0). Built on the kernel stack by `entry.rs`; one layout serves
/// `iretq` and `sysretq` returns. From `rip` up it is exactly the frame the
/// CPU pushes on an interrupt; SYSCALL entry pushes the same words itself.
#[repr(C)]
#[derive(Clone, Debug, Default)]
pub struct TrapFrame {
    /// User FS base (TLS). Read at entry from ring 3, written back at exit.
    /// Not meaningful for frames from ring 0.
    pub fs_base: u64,
    /// RAX at SYSCALL entry (the syscall number); `u64::MAX` for exceptions
    /// and interrupts.
    pub orig_rax: u64,
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
    /// Exception / IRQ vector, or [`VECTOR_SYSCALL`].
    pub vector: u64,
    /// CPU error code (0 when the vector has none).
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// `TrapFrame::vector` for frames built by the SYSCALL entry.
pub const VECTOR_SYSCALL: u64 = 0x100;

pub(super) const TF_SIZE: usize = size_of::<TrapFrame>();
pub(super) const TF_CS: usize = offset_of!(TrapFrame, cs);
pub(super) const TF_ORIG_RAX: usize = offset_of!(TrapFrame, orig_rax);
pub(super) const TF_R15: usize = offset_of!(TrapFrame, r15);
pub(super) const TF_VECTOR: usize = offset_of!(TrapFrame, vector);
/// Offset of `rsp` from `vector` (the asm pops up to `vector` first).
pub(super) const TF_RSP_FROM_VECTOR: usize = offset_of!(TrapFrame, rsp) - TF_VECTOR;

const _: () = {
    assert!(TF_SIZE == 192 && TF_SIZE.is_multiple_of(16));
    assert!(TF_R15 == 16 && TF_VECTOR == 16 + 15 * 8);
    assert!(offset_of!(TrapFrame, rip) == TF_VECTOR + 16);
    assert!(offset_of!(TrapFrame, ss) == TF_SIZE - 8);
};

impl TrapFrame {
    /// Fresh ring-3 state: rip = `entry`, rsp = `sp`, everything else 0,
    /// RFLAGS = IF | bit 1, user code64/data selectors.
    pub fn new_user(entry: u64, sp: u64) -> TrapFrame {
        TrapFrame {
            orig_rax: u64::MAX,
            rip: entry,
            cs: USER_CS as u64,
            rflags: RFLAGS_IF | RFLAGS_RESERVED1,
            rsp: sp,
            ss: USER_DS as u64,
            ..TrapFrame::default()
        }
    }

    #[inline]
    pub fn syscall_nr(&self) -> usize {
        self.orig_rax as usize
    }

    #[inline]
    pub fn syscall_args(&self) -> [u64; 6] {
        [self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9]
    }

    #[inline]
    pub fn set_syscall_ret(&mut self, v: u64) {
        self.rax = v;
    }

    #[inline]
    pub fn pc(&self) -> u64 {
        self.rip
    }

    #[inline]
    pub fn set_pc(&mut self, v: u64) {
        self.rip = v;
    }

    #[inline]
    pub fn sp(&self) -> u64 {
        self.rsp
    }

    #[inline]
    pub fn set_sp(&mut self, v: u64) {
        self.rsp = v;
    }

    #[inline]
    pub fn tls(&self) -> u64 {
        self.fs_base
    }

    #[inline]
    pub fn set_tls(&mut self, v: u64) {
        self.fs_base = v;
    }

    #[inline]
    #[allow(clippy::wrong_self_convention)] // name fixed by the arch contract
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }

    /// Register dump via `kprintln!` (fatal user faults).
    pub fn dump(&self) {
        self.dump_with(&mut |a| crate::kprint!("{}", a));
    }

    /// Register dump through `out` (the fatal kernel path uses the panic
    /// console writer).
    pub(super) fn dump_with(&self, out: &mut dyn FnMut(fmt::Arguments<'_>)) {
        out(format_args!(
            "  RIP    {:#018x}  CS  {:#06x}  RFLAGS {:#018x}\n  RSP    {:#018x}  SS  {:#06x}  CR3    {:#018x}\n",
            self.rip,
            self.cs,
            self.rflags,
            self.rsp,
            self.ss,
            cpu::read_cr3()
        ));
        if self.from_user() {
            out(format_args!(
                "  FS_BASE {:#018x}  ORIG_RAX {:#x}  vector {:#x} error {:#x}\n",
                self.fs_base, self.orig_rax, self.vector, self.error
            ));
        }
        out(format_args!(
            "  RAX {:#018x}  RBX {:#018x}  RCX {:#018x}\n  RDX {:#018x}  RSI {:#018x}  RDI {:#018x}\n  RBP {:#018x}  R8  {:#018x}  R9  {:#018x}\n  R10 {:#018x}  R11 {:#018x}  R12 {:#018x}\n  R13 {:#018x}  R14 {:#018x}  R15 {:#018x}\n",
            self.rax,
            self.rbx,
            self.rcx,
            self.rdx,
            self.rsi,
            self.rdi,
            self.rbp,
            self.r8,
            self.r9,
            self.r10,
            self.r11,
            self.r12,
            self.r13,
            self.r14,
            self.r15
        ));
    }

    /// Whether this ring-3 frame can be returned to with `sysretq` without
    /// losing state: SYSRET loads RIP from RCX and RFLAGS from R11 and fixed
    /// selectors, and faults in ring 0 on a non-canonical RIP (Intel), so
    /// the frame must already agree with that. Otherwise `iretq` is used.
    pub(super) fn sysret_ok(&self) -> bool {
        const RFLAGS_TF: u64 = 1 << 8;
        const RFLAGS_RF: u64 = 1 << 16;
        // Linux's TASK_SIZE_MAX: one page below the canonical boundary.
        const MAX_RIP: u64 = 0x7fff_ffff_f000;
        self.cs == USER_CS as u64
            && self.ss == USER_DS as u64
            && self.rcx == self.rip
            && self.r11 == self.rflags
            && self.rip < MAX_RIP
            && self.rflags & (RFLAGS_TF | RFLAGS_RF) == 0
    }
}

// ---------------------------------------------------------------------------
// FP / SIMD state.

/// Size of [`FpState`]: the legacy 512-byte region, the 64-byte XSAVE header
/// and room for AVX (256 bytes at offset 576) and some slack.
pub const FP_STATE_SIZE: usize = 1024;
const MXCSR_OFFSET: usize = 24;
const XSTATE_BV_OFFSET: usize = 512;

/// Per-thread user FP/SIMD state. With XSAVE (CR4.OSXSAVE, XCR0 =
/// x87|SSE|AVX) it is an XSAVE area in standard format; otherwise the first
/// 512 bytes are an FXSAVE area. Upper AVX-512 / AMX state is never enabled
/// (Phase 2 may size this dynamically from CPUID.0xD).
#[repr(C, align(64))]
#[derive(Clone)]
pub struct FpState([u8; FP_STATE_SIZE]);

impl Default for FpState {
    /// x87 and SSE in their reset state: FCW = 0x37f (all exceptions
    /// masked, 64-bit precision, round to nearest), MXCSR = 0x1f80, all
    /// registers zero; XSTATE_BV = 0 so XRSTOR loads the init state for
    /// every component (MXCSR is still taken from the image).
    fn default() -> Self {
        let mut a = [0u8; FP_STATE_SIZE];
        a[0..2].copy_from_slice(&0x37fu16.to_le_bytes());
        a[MXCSR_OFFSET..MXCSR_OFFSET + 4].copy_from_slice(&0x1f80u32.to_le_bytes());
        a[XSTATE_BV_OFFSET..XSTATE_BV_OFFSET + 8].copy_from_slice(&0u64.to_le_bytes());
        FpState(a)
    }
}

impl fmt::Debug for FpState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FpState { .. }")
    }
}

/// Save the live (user) FP/SIMD state. The kernel is soft-float, so the
/// registers still hold user state whenever the kernel runs.
pub fn fp_save(into: &mut FpState) {
    let p = into.0.as_mut_ptr();
    if cpu::uses_xsave() {
        // SAFETY: 64-byte aligned area large enough for XCR0's components
        // (checked against CPUID.0xD in `cpu::init_features`).
        unsafe {
            asm!("xsave64 [{}]", in(reg) p, in("eax") u32::MAX, in("edx") u32::MAX,
                 options(nostack, preserves_flags))
        };
    } else {
        // SAFETY: 16-byte aligned 512-byte area; CR4.OSFXSR is set at boot.
        unsafe { asm!("fxsave64 [{}]", in(reg) p, options(nostack, preserves_flags)) };
    }
}

/// Load `from` into the FP/SIMD registers (before returning to user mode).
pub fn fp_restore(from: &FpState) {
    let p = from.0.as_ptr();
    if cpu::uses_xsave() {
        // SAFETY: a valid XSAVE image (Default or produced by `fp_save`);
        // reserved header bytes are zero.
        unsafe {
            asm!("xrstor64 [{}]", in(reg) p, in("eax") u32::MAX, in("edx") u32::MAX,
                 options(nostack, preserves_flags, readonly))
        };
    } else {
        // SAFETY: a valid FXSAVE image (MXCSR reserved bits clear).
        unsafe { asm!("fxrstor64 [{}]", in(reg) p, options(nostack, preserves_flags, readonly)) };
    }
}

// ---------------------------------------------------------------------------
// Kernel context switch.

/// Callee-saved kernel state of a thread parked in [`switch`] (or a new
/// context built by [`new_kernel_context`]).
#[repr(C)]
#[derive(Clone, Debug, Default)]
pub struct KernelContext {
    rsp: u64,
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
}

impl KernelContext {
    pub const fn new() -> Self {
        KernelContext {
            rsp: 0,
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rflags: 0,
        }
    }

    pub fn as_ptr(&self) -> *const KernelContext {
        self
    }

    pub fn as_mut_ptr(&mut self) -> *mut KernelContext {
        self
    }
}

global_asm!(
    r#"
    .section .text.ctx_switch, "ax"
    .global __patina_switch
    // rdi = prev, rsi = next
__patina_switch:
    popq    %rax                        // return address
    movq    %rsp, 0(%rdi)
    movq    %rbx, 8(%rdi)
    movq    %rbp, 16(%rdi)
    movq    %r12, 24(%rdi)
    movq    %r13, 32(%rdi)
    movq    %r14, 40(%rdi)
    movq    %r15, 48(%rdi)
    movq    %rax, 56(%rdi)
    pushfq
    popq    64(%rdi)

    movq    0(%rsi), %rsp
    movq    8(%rsi), %rbx
    movq    16(%rsi), %rbp
    movq    24(%rsi), %r12
    movq    32(%rsi), %r13
    movq    40(%rsi), %r14
    movq    48(%rsi), %r15
    pushq   64(%rsi)
    popfq
    jmpq    *56(%rsi)

    // First run of a context from new_kernel_context: r12 = entry, r13 = arg,
    // rsp = stack top (16-aligned, so the call leaves the ABI alignment).
    .global __patina_ctx_start
__patina_ctx_start:
    movq    %r13, %rdi
    callq   *%r12
    ud2
"#,
    options(att_syntax)
);

unsafe extern "C" {
    fn __patina_switch(prev: *mut KernelContext, next: *const KernelContext);
    fn __patina_ctx_start() -> !;
}

const _: () = {
    assert!(offset_of!(KernelContext, rsp) == 0);
    assert!(offset_of!(KernelContext, rip) == 56);
    assert!(offset_of!(KernelContext, rflags) == 64);
};

/// A context that, when switched to, calls `entry(arg)` on the stack whose
/// top is `stack_top` (16-aligned), with interrupts disabled (RFLAGS = bit 1
/// only) and rbp = 0. `entry` must not return.
pub fn new_kernel_context(
    stack_top: usize,
    entry: extern "C" fn(usize) -> !,
    arg: usize,
) -> KernelContext {
    assert!(
        stack_top.is_multiple_of(16),
        "kernel stack top not 16-aligned"
    );
    KernelContext {
        rsp: stack_top as u64,
        r12: entry as usize as u64,
        r13: arg as u64,
        rip: __patina_ctx_start as *const () as u64,
        rflags: RFLAGS_RESERVED1,
        ..KernelContext::new()
    }
}

/// Save the callee-saved registers, RSP and RFLAGS of the caller into
/// `prev` and resume `next`. Returns when something switches back to
/// `prev`; the interrupt flag is restored from the saved RFLAGS.
///
/// # Safety
/// `prev` must be writable and `next` a context saved by `switch` or built
/// by `new_kernel_context` whose stack is still valid; call with interrupts
/// disabled. Neither may be accessed concurrently.
#[inline]
pub unsafe fn switch(prev: *mut KernelContext, next: *const KernelContext) {
    // SAFETY: caller's contract; the asm preserves every callee-saved
    // register across the call as the C ABI requires.
    unsafe { __patina_switch(prev, next) }
}

/// Enter ring 3 with `frame`: copies it to the current kernel stack, runs
/// `trap::before_return_to_user`, masks IRQs, loads FS base, `swapgs`,
/// restores all GPRs and `iretq` (or `sysretq` if the frame allows it).
/// Load the thread's FP state with [`fp_restore`] first, and set the
/// kernel stack for later entries with `cpu::set_kernel_stack`.
///
/// # Safety
/// The user page tables must be live; `frame` must be a ring-3 frame.
pub unsafe fn enter_user(frame: &TrapFrame) -> ! {
    assert!(frame.from_user(), "enter_user: not a ring-3 frame");
    // SAFETY: caller's contract; the asm never returns.
    unsafe { super::entry::__patina_enter_user(frame) }
}

// ---------------------------------------------------------------------------
// Self-test user program.

global_asm!(
    r#"
    .section .rodata.selftest_user, "a"
    .global __selftest_user_start
    .global __selftest_user_end
__selftest_user_start:
    movl    ${nr_write}, %eax
    movl    $1, %edi
    leaq    2f(%rip), %rsi
    movl    $21, %edx
    syscall
    movl    $0x401000, %ebx
    movl    $42, (%rbx)
    movl    ${nr_exit_group}, %eax
    movl    $7, %edi
    syscall
    ud2
2:  .ascii  "selftest user: hello\n"
__selftest_user_end:
"#,
    nr_write = const patina_abi::x86_64::nr::WRITE,
    nr_exit_group = const patina_abi::x86_64::nr::EXIT_GROUP,
    options(att_syntax)
);

unsafe extern "C" {
    static __selftest_user_start: u8;
    static __selftest_user_end: u8;
}

/// Position-independent ring-3 code: `write(1, "selftest user: hello\n",
/// 21)`, store to 0x401000, `exit_group(7)`, `ud2`.
pub fn selftest_user_code() -> &'static [u8] {
    let start = core::ptr::addr_of!(__selftest_user_start);
    let end = core::ptr::addr_of!(__selftest_user_end) as usize;
    // SAFETY: both symbols delimit one blob in .rodata.
    unsafe { core::slice::from_raw_parts(start, end - start as usize) }
}
