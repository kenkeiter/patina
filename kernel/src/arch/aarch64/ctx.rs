//! Register state: the user [`TrapFrame`], kernel context switching and the
//! first entry to EL0. See the Phase 1 contract in `arch/mod.rs`.
//!
//! # Trap frame
//!
//! Every exception entry (from EL0 or EL1) pushes one [`TrapFrame`] on the
//! current SP_EL1 stack (the overflow stack for a kernel stack overflow, see
//! `exceptions.rs`). For an entry from EL0 the frame sits at the very top of
//! the region the thread left on its kernel stack when it entered user mode:
//! `SP_EL1` at `eret` is `frame + size_of::<TrapFrame>()`, and the next entry
//! from EL0 builds the next frame at the same address. The FP/SIMD registers
//! (`q0`-`q31`, FPCR, FPSR) and TPIDR_EL0 are part of the frame and are
//! saved/restored eagerly on every entry/exit, because the kernel itself is
//! compiled with NEON (decision D5).
//!
//! # Kernel context
//!
//! [`KernelContext`] holds the AAPCS64 callee-saved state: x19-x28, x29
//! (fp), x30 (lr), sp and d8-d15 (the low halves of v8-v15 are callee-saved
//! too, and the kernel uses SIMD). [`switch`] saves into `prev` and loads
//! `next`; DAIF is not part of the context, so a thread resumes with the
//! interrupt state its `switch` call had (the contract requires that to be
//! "masked"). A context made by [`new_kernel_context`] starts in
//! `__patina_kctx_start` with IRQs in whatever state the switcher had (masked).

use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of};

use patina_abi::aarch64::nr;

/// Complete register state saved at every exception entry.
///
/// For entries from EL0 `sp` is SP_EL0 (the user stack pointer); for entries
/// from EL1 it is SP_EL1 before the exception (for the dump only).
/// `x[8]` keeps the syscall number and `x[0]` receives the return value;
/// `orig_x0` is x0 at entry (the first syscall argument), kept for syscall
/// restart in Phase 2. `v[2n]`/`v[2n+1]` are the low/high halves of `qn`.
#[repr(C, align(16))]
#[derive(Clone)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp: u64,
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
    /// TPIDR_EL0 (user TLS base).
    pub tpidr: u64,
    pub orig_x0: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    pub v: [u64; 64],
}

// Offsets used by the entry/exit assembly in `exceptions.rs`.
pub(super) const FRAME_SIZE: usize = size_of::<TrapFrame>();
pub(super) const OFF_SP: usize = offset_of!(TrapFrame, sp);
pub(super) const OFF_ELR: usize = offset_of!(TrapFrame, elr);
pub(super) const OFF_ESR: usize = offset_of!(TrapFrame, esr);
pub(super) const OFF_TPIDR: usize = offset_of!(TrapFrame, tpidr);
pub(super) const OFF_FPCR: usize = offset_of!(TrapFrame, fpcr);
pub(super) const OFF_V: usize = offset_of!(TrapFrame, v);
const _: () = {
    assert!(FRAME_SIZE == 832 && FRAME_SIZE.is_multiple_of(16));
    assert!(OFF_SP == 248);
    // Pairs stored with one stp each.
    assert!(offset_of!(TrapFrame, spsr) == OFF_ELR + 8);
    assert!(offset_of!(TrapFrame, far) == OFF_ESR + 8);
    assert!(offset_of!(TrapFrame, orig_x0) == OFF_TPIDR + 8);
    assert!(offset_of!(TrapFrame, fpsr) == OFF_FPCR + 8);
    assert!(OFF_V.is_multiple_of(16) && OFF_V + 30 * 16 <= 1008); // stp q imm7 range
};

/// SPSR.M = EL0t, AArch64.
const SPSR_EL0T: u64 = 0;
/// SPSR bits user code may own (everything else is forced to 0 on return to
/// EL0): NZCV, DIT, SSBS, BTYPE.
pub(super) const SPSR_USER_MASK: u64 = (0xf << 28) | (1 << 24) | (1 << 12) | (0b11 << 10);

impl TrapFrame {
    /// All-zero frame.
    pub const fn zeroed() -> TrapFrame {
        TrapFrame {
            x: [0; 31],
            sp: 0,
            elr: 0,
            spsr: 0,
            esr: 0,
            far: 0,
            tpidr: 0,
            orig_x0: 0,
            fpcr: 0,
            fpsr: 0,
            v: [0; 64],
        }
    }

    /// Fresh user state: pc = `entry`, sp = `sp`, everything else 0, EL0t
    /// with DAIF clear (interrupts enabled in user mode), FPCR = 0.
    pub fn new_user(entry: u64, sp: u64) -> TrapFrame {
        let mut f = TrapFrame::zeroed();
        f.elr = entry;
        f.sp = sp;
        f.spsr = SPSR_EL0T;
        f
    }

    #[inline]
    pub fn syscall_nr(&self) -> usize {
        self.x[8] as usize
    }
    #[inline]
    pub fn syscall_args(&self) -> [u64; 6] {
        [
            self.x[0], self.x[1], self.x[2], self.x[3], self.x[4], self.x[5],
        ]
    }
    #[inline]
    pub fn set_syscall_ret(&mut self, v: u64) {
        self.x[0] = v;
    }
    #[inline]
    pub fn pc(&self) -> u64 {
        self.elr
    }
    #[inline]
    pub fn set_pc(&mut self, v: u64) {
        self.elr = v;
    }
    #[inline]
    pub fn sp(&self) -> u64 {
        self.sp
    }
    #[inline]
    pub fn set_sp(&mut self, v: u64) {
        self.sp = v;
    }
    #[inline]
    pub fn tls(&self) -> u64 {
        self.tpidr
    }
    #[inline]
    pub fn set_tls(&mut self, v: u64) {
        self.tpidr = v;
    }
    /// SPSR.M[3:0] == EL0t.
    #[allow(clippy::wrong_self_convention)] // name fixed by the arch contract
    #[inline]
    pub fn from_user(&self) -> bool {
        self.spsr & 0xf == 0
    }

    /// Register dump (ESR/FAR/ELR/SPSR/SP, x0-x30, TPIDR_EL0) via `kprintln!`.
    pub fn dump(&self) {
        self.write_dump(&mut |a| crate::kprint!("{}", a));
    }

    /// The dump, written through `out` (the fatal kernel path uses
    /// `console::panic_write_fmt`).
    pub(super) fn write_dump(&self, out: &mut dyn FnMut(core::fmt::Arguments<'_>)) {
        out(format_args!(
            "  ESR  {:#018x}  FAR  {:#018x}\n  ELR  {:#018x}  SPSR {:#018x}\n  SP   {:#018x}  TPIDR_EL0 {:#018x}\n",
            self.esr, self.far, self.elr, self.spsr, self.sp, self.tpidr
        ));
        for i in (0..31).step_by(2) {
            if i + 1 < 31 {
                out(format_args!(
                    "  x{:<2}  {:#018x}  x{:<2}  {:#018x}\n",
                    i,
                    self.x[i],
                    i + 1,
                    self.x[i + 1]
                ));
            } else {
                out(format_args!("  x{:<2}  {:#018x}\n", i, self.x[i]));
            }
        }
    }
}

/// Per-thread FP state outside the frame: empty on aarch64, because the
/// [`TrapFrame`] carries q0-q31/FPCR/FPSR and they are saved and restored on
/// every kernel entry/exit. [`fp_save`] / [`fp_restore`] are no-ops.
#[derive(Clone, Copy, Debug, Default)]
pub struct FpState {
    _private: (),
}

/// No-op on aarch64 (FP state lives in the [`TrapFrame`]).
#[inline]
pub fn fp_save(_into: &mut FpState) {}

/// No-op on aarch64 (FP state lives in the [`TrapFrame`]).
#[inline]
pub fn fp_restore(_from: &FpState) {}

/// Callee-saved kernel state for [`switch`]. Layout shared with the asm.
#[repr(C)]
#[derive(Clone, Debug, Default)]
pub struct KernelContext {
    /// x19..=x28.
    x19_28: [u64; 10],
    fp: u64,
    lr: u64,
    sp: u64,
    _pad: u64,
    /// d8..=d15.
    d8_15: [u64; 8],
}

const _: () = assert!(size_of::<KernelContext>() == 176);

impl KernelContext {
    pub const fn new() -> KernelContext {
        KernelContext {
            x19_28: [0; 10],
            fp: 0,
            lr: 0,
            sp: 0,
            _pad: 0,
            d8_15: [0; 8],
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
    .section .text.patina_ctx, "ax"
    .balign 16
    .global __patina_switch
// x0 = prev (*mut KernelContext), x1 = next (*const KernelContext)
__patina_switch:
    stp     x19, x20, [x0, #0]
    stp     x21, x22, [x0, #16]
    stp     x23, x24, [x0, #32]
    stp     x25, x26, [x0, #48]
    stp     x27, x28, [x0, #64]
    stp     x29, x30, [x0, #80]
    mov     x9, sp
    str     x9, [x0, #96]
    stp     d8, d9, [x0, #112]
    stp     d10, d11, [x0, #128]
    stp     d12, d13, [x0, #144]
    stp     d14, d15, [x0, #160]

    ldp     x19, x20, [x1, #0]
    ldp     x21, x22, [x1, #16]
    ldp     x23, x24, [x1, #32]
    ldp     x25, x26, [x1, #48]
    ldp     x27, x28, [x1, #64]
    ldp     x29, x30, [x1, #80]
    ldr     x9, [x1, #96]
    mov     sp, x9
    ldp     d8, d9, [x1, #112]
    ldp     d10, d11, [x1, #128]
    ldp     d12, d13, [x1, #144]
    ldp     d14, d15, [x1, #160]
    ret

// First switch into a context from new_kernel_context: x19 = entry, x20 = arg.
__patina_kctx_start:
    mov     x0, x20
    mov     x29, #0
    mov     x30, #0
    br      x19

    .global __patina_selftest_user_start
    .global __patina_selftest_user_end
    .section .rodata.patina_selftest_user, "a"
    .balign 4
// EL0 self-test program (position independent; copied to a user page).
__patina_selftest_user_start:
    mov     x0, #1                      // fd
    adr     x1, 1f                      // buf
    mov     x2, #21                     // len
    mov     x8, #{nr_write}             // write
    svc     #0
    movz    x9, #0x40, lsl #16          // 0x401000 (selftest_user::DATA_VA)
    movk    x9, #0x1000
    str     x0, [x9]
    mov     x0, #7
    mov     x8, #{nr_exit_group}        // exit_group(7)
    svc     #0
    brk     #0                          // exit_group returned: trap
1:  .ascii  "selftest user: hello\n"
    .balign 4
__patina_selftest_user_end:
"#,
    nr_write = const nr::WRITE,
    nr_exit_group = const nr::EXIT_GROUP,
);

unsafe extern "C" {
    fn __patina_switch(prev: *mut KernelContext, next: *const KernelContext);
    fn __patina_kctx_start();
    static __patina_selftest_user_start: u8;
    static __patina_selftest_user_end: u8;
}

/// A context that, when switched to, runs `entry(arg)` on the stack whose top
/// is `stack_top` (16-aligned). IRQs are as the switcher left them (masked).
pub fn new_kernel_context(
    stack_top: usize,
    entry: extern "C" fn(usize) -> !,
    arg: usize,
) -> KernelContext {
    assert!(
        stack_top.is_multiple_of(16),
        "kernel stack top not 16-aligned"
    );
    let mut c = KernelContext::new();
    c.x19_28[0] = entry as *const () as u64;
    c.x19_28[1] = arg as u64;
    c.lr = __patina_kctx_start as *const () as u64;
    c.sp = stack_top as u64;
    c
}

/// Save the current callee-saved state into `prev` and resume `next`.
///
/// # Safety
/// `prev` must be writable, `next` must hold a context saved by `switch` or
/// built by [`new_kernel_context`] whose stack is still alive, and interrupts
/// must be masked.
#[inline]
pub unsafe fn switch(prev: *mut KernelContext, next: *const KernelContext) {
    // SAFETY: per the caller contract; __patina_switch is an AAPCS64
    // function that preserves every callee-saved register of the caller.
    unsafe { __patina_switch(prev, next) }
}

unsafe extern "C" {
    /// `exceptions.rs`: copy `*frame` onto the current stack, run the
    /// return-to-user hook, restore and `eret`.
    fn __patina_enter_user(frame: *const TrapFrame) -> !;
}

/// Enter EL0 with `frame`. The frame is copied onto the current kernel
/// stack (which becomes the stack for the next entry from EL0), then
/// `trap::before_return_to_user` runs with IRQs masked, then every register
/// (incl. FP/SIMD, TPIDR_EL0, SP_EL0, ELR/SPSR) is loaded and `eret`.
///
/// # Safety
/// The user page tables must be live and `frame` must describe a valid user
/// state; the caller's stack frames below this point are abandoned.
pub unsafe fn enter_user(frame: &TrapFrame) -> ! {
    // SAFETY: masking IRQs locally before switching the exception state.
    unsafe { asm!("msr daifset, #3", options(nostack, preserves_flags)) };
    // SAFETY: per the caller contract.
    unsafe { __patina_enter_user(frame) }
}

/// Bytes of the tiny EL0 self-test program: `write(1, "selftest user:
/// hello\n", 21)`, a store to 0x401000, `exit_group(7)`, `brk #0`.
pub fn selftest_user_code() -> &'static [u8] {
    // SAFETY: the two symbols bracket the program in .rodata.
    unsafe {
        let start = core::ptr::addr_of!(__patina_selftest_user_start);
        let end = core::ptr::addr_of!(__patina_selftest_user_end);
        core::slice::from_raw_parts(start, end as usize - start as usize)
    }
}

/// Sanitize the frame for a return to EL0: force EL0t/AArch64 with DAIF
/// clear and keep only the PSTATE bits user code may own.
#[inline]
pub(super) fn sanitize_user(frame: &mut TrapFrame) {
    frame.spsr &= SPSR_USER_MASK;
}
