//! x86_64 `rt_sigframe` construction and `rt_sigreturn` (Linux
//! `arch/x86/kernel/signal.c` `get_sigframe`, `signal_64.c`
//! `x64_setup_rt_frame`/`restore_sigcontext`, `fpu/signal.c`). See the
//! contract in `arch/mod.rs`.
//!
//! Layout below the interrupted `rsp - 128` (red zone), or the top of the
//! alternate stack: the fpstate area (64-aligned; the thread's XSAVE image
//! for x87|SSE|AVX with `fpx_sw_bytes` in the legacy area's software
//! bytes and `FP_XSTATE_MAGIC2` after it, or the 512-byte FXSAVE image),
//! then `struct rt_sigframe { pretcode; ucontext; siginfo }` with `rsp ≡ 8
//! (mod 16)`. Both are written with one `copy_to_user` of a stack buffer
//! (the alignment gap between them is zero-filled). The handler starts
//! with the FPU in its init state, rdi = signo, rsi = &info, rdx = &uc,
//! rax = 0, DF/TF/RF clear; `pretcode` is `sa_restorer`.

use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, Ordering};

use patina_abi::signal::{SaFlags, SigSet, StackT};
use patina_abi::x86_64::{
    FP_XSTATE_MAGIC1, FP_XSTATE_MAGIC2, FpxSwBytes, REDZONE, RtSigframe, UC_FP_XSTATE,
    UC_SIGCONTEXT_SS, UC_STRICT_RESTORE_SS, UContext, XSAVE_HEADER_SIZE, XSTATE_LEGACY_SIZE,
    sigframe_placement, xstate_sigframe_size,
};
use patina_abi::zerocopy::{FromBytes, IntoBytes};

use super::cpu;
use super::ctx::{self, FpState, TrapFrame};
use super::gdt::{USER_CS, USER_DS};
use crate::task::signal::{
    FaultInfo, SetupError, SigDelivery, on_sig_stack, on_sig_stack_raw, sas_ss_flags,
};

const RF_CF: u64 = 1 << 0;
const RF_PF: u64 = 1 << 2;
const RF_AF: u64 = 1 << 4;
const RF_ZF: u64 = 1 << 6;
const RF_SF: u64 = 1 << 7;
const RF_TF: u64 = 1 << 8;
const RF_DF: u64 = 1 << 10;
const RF_OF: u64 = 1 << 11;
const RF_RF: u64 = 1 << 16;
const RF_AC: u64 = 1 << 18;
/// RFLAGS bits `rt_sigreturn` takes from the frame (Linux `FIX_EFLAGS`).
const FIX_EFLAGS: u64 =
    RF_AC | RF_OF | RF_DF | RF_TF | RF_SF | RF_ZF | RF_AF | RF_PF | RF_CF | RF_RF;

/// XFEATURE_MASK_FP | XFEATURE_MASK_SSE.
const XFEATURE_FPSSE: u64 = 0b11;
/// Byte offsets in the legacy/XSAVE area.
const OFF_MXCSR: usize = 24;
const OFF_MXCSR_MASK: usize = 28;
const OFF_SW_BYTES: usize = 464;
const OFF_XSTATE_BV: usize = XSTATE_LEGACY_SIZE;
/// End of the AVX (YMM_Hi128) component in the standard format.
const AVX_END: usize = 576 + 256;
/// Smallest valid XSAVE image (legacy + header).
const MIN_XSTATE: usize = XSTATE_LEGACY_SIZE + XSAVE_HEADER_SIZE;
/// Largest frame buffer: rt_sigframe + 64-byte alignment slack + 16 +
/// fpstate + magic2.
const BUF_SIZE: usize = 2048;
const _: () = assert!(size_of::<RtSigframe>() + 64 + 16 + ctx::FP_STATE_SIZE + 4 <= BUF_SIZE);

static XSTATE_SIZE: AtomicU32 = AtomicU32::new(0);
static MXCSR_MASK: AtomicU32 = AtomicU32::new(0);

/// User xstate size (`fpstate->user_size`): CPUID.0xD.0:EBX for the XCR0
/// components (832 for x87|SSE|AVX), or 512 with FXSAVE.
fn xstate_size() -> usize {
    if !cpu::uses_xsave() {
        return XSTATE_LEGACY_SIZE;
    }
    let s = XSTATE_SIZE.load(Ordering::Relaxed);
    if s != 0 {
        return s as usize;
    }
    let s = cpu::cpuid(0xd, 0).1.min(ctx::FP_STATE_SIZE as u32);
    XSTATE_SIZE.store(s, Ordering::Relaxed);
    s as usize
}

/// `mxcsr_feature_mask` (MXCSR_MASK from an FXSAVE image, 0xffbf if 0).
fn mxcsr_mask(img: &[u8]) -> u32 {
    let m = MXCSR_MASK.load(Ordering::Relaxed);
    if m != 0 {
        return m;
    }
    let mut m = u32::from_le_bytes(img[OFF_MXCSR_MASK..OFF_MXCSR_MASK + 4].try_into().unwrap());
    if m == 0 {
        m = 0xffbf;
    }
    MXCSR_MASK.store(m, Ordering::Relaxed);
    m
}

/// Fault details of a page fault at `addr` (`cr2`, error code, vector).
pub fn fault_info(frame: &TrapFrame, addr: u64) -> FaultInfo {
    FaultInfo {
        addr,
        code: frame.error,
        trapno: frame.vector,
    }
}

/// Fault details of another exception (cr2 0).
pub fn exception_info(frame: &TrapFrame) -> FaultInfo {
    FaultInfo {
        addr: 0,
        code: frame.error,
        trapno: frame.vector,
    }
}

/// Re-execute the `syscall` that returned an `ERESTART*` code: rax =
/// orig_rax (or `nr` for `restart_syscall`), rip -= 2.
pub fn rewind_syscall(frame: &mut TrapFrame, nr: Option<usize>) {
    frame.rax = nr.map_or(frame.orig_rax, |n| n as u64);
    frame.rip -= 2;
}

fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Build the signal frame for `d` on the user stack and point `frame` at
/// the handler. Resets the FPU to its init state for the handler.
pub fn setup_frame(frame: &mut TrapFrame, d: &SigDelivery<'_>) -> Result<(), SetupError> {
    let flags = d.action.flags();
    if !flags.contains(SaFlags::RESTORER) {
        return Err(SetupError::NoRestorer);
    }
    let xsave = cpu::uses_xsave();
    let xsize = xstate_size();
    let math = xstate_sigframe_size(xsize as u64, xsave);

    // get_sigframe
    let nested = on_sig_stack(&d.altstack, frame.rsp);
    let mut sp = frame.rsp.wrapping_sub(REDZONE);
    let mut entering = false;
    if flags.contains(SaFlags::ONSTACK) && sas_ss_flags(&d.altstack, sp) == 0 {
        sp = d.altstack.ss_sp + d.altstack.ss_size;
        entering = true;
    }
    if sp < BUF_SIZE as u64 + 4096 || sp > crate::arch::uspace::USER_VA_END as u64 {
        return Err(SetupError::Fault);
    }
    let p = sigframe_placement(sp, math);
    if (nested || entering) && !on_sig_stack_raw(&d.altstack, p.frame) {
        return Err(SetupError::Fault);
    }
    let total = (p.fpstate + math - p.frame) as usize;
    debug_assert!(total <= BUF_SIZE);
    let mut buf = [0u8; BUF_SIZE];

    // fpstate: the live user FP registers (the kernel is soft-float).
    let mut fp = FpState::default();
    ctx::fp_save(&mut fp);
    let fo = (p.fpstate - p.frame) as usize;
    {
        let area = &mut buf[fo..fo + math as usize];
        area[..xsize].copy_from_slice(&fp.bytes()[..xsize]);
        if xsave {
            let sw = FpxSwBytes {
                magic1: FP_XSTATE_MAGIC1,
                extended_size: math as u32,
                xfeatures: cpu::XCR0_USED,
                xstate_size: xsize as u32,
                padding: [0; 7],
            };
            area[OFF_SW_BYTES..OFF_SW_BYTES + size_of::<FpxSwBytes>()]
                .copy_from_slice(sw.as_bytes());
            // Header: only XSTATE_BV (FP|SSE forced on, as Linux), the
            // rest of the 64 bytes zero.
            let bv = get_u64(area, OFF_XSTATE_BV) | XFEATURE_FPSSE;
            area[OFF_XSTATE_BV..MIN_XSTATE].fill(0);
            put_u64(area, OFF_XSTATE_BV, bv);
            area[xsize..xsize + 4].copy_from_slice(&FP_XSTATE_MAGIC2.to_le_bytes());
        } else {
            area[OFF_SW_BYTES..XSTATE_LEGACY_SIZE].fill(0);
        }
    }

    let mut f = RtSigframe {
        pretcode: d.action.sa_restorer,
        info: *d.info,
        ..RtSigframe::default()
    };
    let uc = &mut f.uc;
    uc.uc_flags = UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS | if xsave { UC_FP_XSTATE } else { 0 };
    uc.uc_link = 0;
    uc.uc_stack = d.altstack;
    uc.uc_sigmask = d.mask;
    let sc = &mut uc.uc_mcontext;
    sc.r8 = frame.r8;
    sc.r9 = frame.r9;
    sc.r10 = frame.r10;
    sc.r11 = frame.r11;
    sc.r12 = frame.r12;
    sc.r13 = frame.r13;
    sc.r14 = frame.r14;
    sc.r15 = frame.r15;
    sc.rdi = frame.rdi;
    sc.rsi = frame.rsi;
    sc.rbp = frame.rbp;
    sc.rbx = frame.rbx;
    sc.rdx = frame.rdx;
    sc.rax = frame.rax;
    sc.rcx = frame.rcx;
    sc.rsp = frame.rsp;
    sc.rip = frame.rip;
    sc.eflags = frame.rflags;
    sc.cs = frame.cs as u16;
    sc.gs = 0;
    sc.fs = 0;
    sc.ss = frame.ss as u16;
    if let Some(fi) = d.fault {
        sc.err = fi.code;
        sc.trapno = fi.trapno;
        sc.cr2 = fi.addr;
    }
    sc.oldmask = d.mask;
    sc.fpstate = p.fpstate;
    buf[..size_of::<RtSigframe>()].copy_from_slice(f.as_bytes());

    crate::task::process()
        .space
        .lock()
        .copy_to_user(p.frame as usize, &buf[..total])
        .map_err(|_| SetupError::Fault)?;

    frame.rdi = d.signo as u64;
    frame.rsi = p.frame + offset_of!(RtSigframe, info) as u64;
    frame.rdx = p.frame + offset_of!(RtSigframe, uc) as u64;
    frame.rax = 0;
    frame.rsp = p.frame;
    frame.rip = d.action.sa_handler;
    frame.rflags &= !(RF_DF | RF_RF | RF_TF);
    frame.cs = USER_CS as u64;
    frame.ss = USER_DS as u64;
    // fpu__clear_user_states: the handler starts with init FP state.
    ctx::fp_restore(&FpState::default());
    Ok(())
}

/// Load the fpstate at user `buf` into the FPU (Linux `__fpu_restore_sig`
/// for 64-bit frames): XSAVE images with valid `fpx_sw_bytes` restore the
/// features they declare (FP/SSE always), anything else is treated as an
/// FXSAVE image; an MXCSR with reserved bits set is a bad frame. `buf` 0
/// resets the FPU.
fn restore_fp(buf: u64) -> Result<(), ()> {
    let mut st = FpState::default();
    if buf == 0 {
        ctx::fp_restore(&st);
        return Ok(());
    }
    let xsave = cpu::uses_xsave();
    let align = if xsave { 64 } else { 16 };
    if !buf.is_multiple_of(align) {
        return Err(());
    }
    let xsize = xstate_size();
    let mut space = crate::task::process().space.lock();
    let b = st.bytes_mut();
    space
        .copy_from_user(buf as usize, &mut b[..XSTATE_LEGACY_SIZE])
        .map_err(|_| ())?;
    if xsave {
        let sw =
            FpxSwBytes::read_from_bytes(&b[OFF_SW_BYTES..OFF_SW_BYTES + size_of::<FpxSwBytes>()])
                .map_err(|_| ())?;
        let size = sw.xstate_size as usize;
        let mut ok = sw.magic1 == FP_XSTATE_MAGIC1
            && size >= MIN_XSTATE
            && size <= xsize
            && sw.xstate_size <= sw.extended_size;
        if ok {
            let magic2: u32 = space.read_val(buf as usize + size).map_err(|_| ())?;
            ok = magic2 == FP_XSTATE_MAGIC2;
        }
        let mut bv = XFEATURE_FPSSE;
        if ok {
            space
                .copy_from_user(
                    buf as usize + XSTATE_LEGACY_SIZE,
                    &mut b[XSTATE_LEGACY_SIZE..size],
                )
                .map_err(|_| ())?;
            bv |= get_u64(b, OFF_XSTATE_BV) & sw.xfeatures & cpu::XCR0_USED;
            if size < AVX_END {
                bv &= XFEATURE_FPSSE;
            }
        }
        b[OFF_XSTATE_BV..MIN_XSTATE].fill(0);
        put_u64(b, OFF_XSTATE_BV, bv);
    }
    drop(space);
    let mxcsr = u32::from_le_bytes(b[OFF_MXCSR..OFF_MXCSR + 4].try_into().unwrap());
    let mut probe = FpState::default();
    ctx::fp_save(&mut probe);
    if mxcsr & !mxcsr_mask(probe.bytes()) != 0 {
        return Err(());
    }
    ctx::fp_restore(&st);
    Ok(())
}

/// `rt_sigreturn`: the restorer's `ret` popped `pretcode`, so `rsp` points
/// at `uc`. Restores the general registers (RFLAGS only the
/// `FIX_EFLAGS` bits; CS/SS forced to the user selectors, `orig_rax` = -1)
/// and the FPU; returns the saved mask and `uc_stack`. `Err` on a bad
/// frame (caller forces SIGSEGV).
pub fn restore_frame(frame: &mut TrapFrame) -> Result<(SigSet, StackT), ()> {
    let uc: UContext = crate::task::process()
        .space
        .lock()
        .read_val(frame.rsp as usize)
        .map_err(|_| ())?;
    let sc = &uc.uc_mcontext;
    // A non-canonical rip would fault the kernel's iretq; Linux turns that
    // into SIGSEGV too.
    if sc.rip >= crate::arch::uspace::USER_VA_END as u64 {
        return Err(());
    }
    frame.r8 = sc.r8;
    frame.r9 = sc.r9;
    frame.r10 = sc.r10;
    frame.r11 = sc.r11;
    frame.r12 = sc.r12;
    frame.r13 = sc.r13;
    frame.r14 = sc.r14;
    frame.r15 = sc.r15;
    frame.rdi = sc.rdi;
    frame.rsi = sc.rsi;
    frame.rbp = sc.rbp;
    frame.rbx = sc.rbx;
    frame.rdx = sc.rdx;
    frame.rax = sc.rax;
    frame.rcx = sc.rcx;
    frame.rsp = sc.rsp;
    frame.rip = sc.rip;
    frame.rflags = (frame.rflags & !FIX_EFLAGS) | (sc.eflags & FIX_EFLAGS);
    frame.cs = USER_CS as u64;
    frame.ss = USER_DS as u64;
    frame.orig_rax = u64::MAX;
    restore_fp(sc.fpstate)?;
    Ok((uc.uc_sigmask, uc.uc_stack))
}
