//! arm64 `rt_sigframe` construction and `rt_sigreturn` (Linux
//! `arch/arm64/kernel/signal.c`: `get_sigframe`, `setup_sigframe`,
//! `setup_return`, `restore_sigframe`). See the contract in `arch/mod.rs`.
//!
//! Frame: `struct rt_sigframe { siginfo; ucontext }` at the new `sp`
//! (16-aligned), with an AAPCS64 frame record (interrupted x29/x30)
//! right above it so unwinders can walk through the handler. FP/SIMD state
//! lives in the trap frame on arm64 (D28), so `fpsimd_context` is built
//! from and restored into `TrapFrame::{v, fpcr, fpsr}`; `esr_context` is
//! added for fault signals. Handler entry: x0 = signo, x1 = &info, x2 =
//! &uc, sp = frame, x29 = &record, x30 = the return trampoline, pc =
//! handler, PSTATE.BTYPE and TCO cleared.
//!
//! Return trampoline: `sa_restorer` with `SA_RESTORER`; otherwise Linux
//! uses the vDSO's `__kernel_rt_sigreturn`. Go on arm64 does NOT set
//! `SA_RESTORER` (`_SA_RESTORER` is 0 in its arm64 defs), and patina has
//! no vDSO yet, so the first such delivery maps one read-only executable
//! page at [`SIGTRAMP_HINT`] (or wherever `mmap` puts it) holding the same
//! two instructions, `mov x8, #__NR_rt_sigreturn; svc #0`.

use alloc::vec::Vec;
use core::arch::asm;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::aarch64::{FpsimdContext, FrameRecord, RtSigframe, nr, sigframe_placement};
use patina_abi::signal::{SaFlags, SigSet, StackT};
use patina_abi::zerocopy::IntoBytes;

use super::ctx::TrapFrame;
use crate::task::signal::{FaultInfo, SetupError, SigDelivery, sas_ss_flags};

const PSR_BTYPE_MASK: u64 = 0b11 << 10;
const PSR_TCO: u64 = 1 << 25;
/// PSTATE bits that make a restored state invalid for EL0: M[3:0] != EL0t,
/// nRW (AArch32), D/A/I/F masked (Linux `valid_native_regs`).
const PSR_INVALID: u64 = 0xf | (1 << 4) | (0xf << 6);

/// Preferred address of the sigreturn trampoline page: just above the main
/// stack (which ends at 0x7fff_f000_0000, D36), where the vDSO would go.
pub const SIGTRAMP_HINT: usize = 0x7fff_f000_0000;
static SIGTRAMP: AtomicU64 = AtomicU64::new(0);

/// The user address of the sigreturn trampoline, mapping it on first use.
fn sigtramp() -> Result<u64, SetupError> {
    let v = SIGTRAMP.load(Ordering::Acquire);
    if v != 0 {
        return Ok(v);
    }
    const PROT_RW: u32 = 0x3;
    const PROT_RX: u32 = 0x5;
    const MAP_PRIVATE_ANON: u32 = 0x22;
    // movz x8, #__NR_rt_sigreturn ; svc #0
    let code: [u32; 2] = [0xd280_0008 | ((nr::RT_SIGRETURN as u32) << 5), 0xd400_0001];
    let mut s = crate::task::process().space.lock();
    let va = s
        .mmap(SIGTRAMP_HINT, 4096, PROT_RW, MAP_PRIVATE_ANON, -1, 0)
        .map_err(|_| SetupError::Fault)?;
    s.copy_to_user(va, code.as_bytes())
        .and_then(|_| s.mprotect(va, 4096, PROT_RX))
        .map_err(|_| SetupError::Fault)?;
    let (pa, _) = s.tables.translate(va).ok_or(SetupError::Fault)?;
    let kva = crate::mm::phys_to_virt(pa).0;
    // The code was written through the direct map: clean it to the point
    // of unification and drop stale instruction-cache lines.
    // SAFETY: cache maintenance on a mapped kernel address; no memory is
    // changed.
    unsafe {
        asm!(
            "dc cvau, {0}",
            "dsb ish",
            "ic ialluis",
            "dsb ish",
            "isb",
            in(reg) kva,
            options(nostack, preserves_flags)
        )
    };
    SIGTRAMP.store(va as u64, Ordering::Release);
    Ok(va as u64)
}

/// Fault details of a data/instruction abort at `addr`.
pub fn fault_info(frame: &TrapFrame, addr: u64) -> FaultInfo {
    FaultInfo {
        addr,
        code: frame.esr,
        trapno: 0,
    }
}

/// Fault details of another synchronous exception (`fault_address` 0,
/// as Linux's `arm64_notify_die`).
pub fn exception_info(frame: &TrapFrame) -> FaultInfo {
    FaultInfo {
        addr: 0,
        code: frame.esr,
        trapno: 0,
    }
}

/// Re-execute the `svc` that returned an `ERESTART*` code: x0 = orig_x0,
/// pc -= 4, and for `restart_syscall` x8 = `nr`.
pub fn rewind_syscall(frame: &mut TrapFrame, nr: Option<usize>) {
    frame.x[0] = frame.orig_x0;
    frame.elr -= 4;
    if let Some(n) = nr {
        frame.x[8] = n as u64;
    }
}

fn fpsimd(frame: &TrapFrame) -> FpsimdContext {
    let mut c = FpsimdContext {
        fpsr: frame.fpsr as u32,
        fpcr: frame.fpcr as u32,
        ..FpsimdContext::default()
    };
    for (i, r) in c.vregs.iter_mut().enumerate() {
        *r = [frame.v[2 * i], frame.v[2 * i + 1]];
    }
    c
}

/// Build the signal frame for `d` on the user stack and point `frame` at
/// the handler.
pub fn setup_frame(frame: &mut TrapFrame, d: &SigDelivery<'_>) -> Result<(), SetupError> {
    let flags = d.action.flags();
    let restorer = if flags.contains(SaFlags::RESTORER) {
        d.action.sa_restorer
    } else {
        sigtramp()?
    };
    let mut sp = frame.sp;
    if flags.contains(SaFlags::ONSTACK) && sas_ss_flags(&d.altstack, sp) == 0 {
        sp = d.altstack.ss_sp + d.altstack.ss_size;
    }
    if sp < (size_of::<RtSigframe>() + 64) as u64 {
        return Err(SetupError::Fault);
    }
    let p = sigframe_placement(sp);

    let mut f = RtSigframe {
        info: *d.info,
        ..RtSigframe::default()
    };
    f.uc.uc_flags = 0;
    f.uc.uc_link = 0;
    f.uc.uc_stack = d.altstack;
    f.uc.uc_sigmask = d.mask;
    let sc = &mut f.uc.uc_mcontext;
    sc.fault_address = d.fault.map_or(0, |x| x.addr);
    sc.regs = frame.x;
    sc.sp = frame.sp;
    sc.pc = frame.elr;
    sc.pstate = frame.spsr;
    sc.write_reserved(&fpsimd(frame), d.fault.map(|x| x.code));
    let rec = FrameRecord {
        fp: frame.x[29],
        lr: frame.x[30],
    };
    let mut buf = Vec::with_capacity(size_of::<RtSigframe>() + size_of::<FrameRecord>());
    buf.extend_from_slice(f.as_bytes());
    buf.extend_from_slice(rec.as_bytes());
    debug_assert_eq!(p.frame + size_of::<RtSigframe>() as u64, p.frame_record);
    crate::task::process()
        .space
        .lock()
        .copy_to_user(p.frame as usize, &buf)
        .map_err(|_| SetupError::Fault)?;

    frame.x[0] = d.signo as u64;
    frame.x[1] = p.frame + offset_of!(RtSigframe, info) as u64;
    frame.x[2] = p.frame + offset_of!(RtSigframe, uc) as u64;
    frame.x[29] = p.frame_record;
    frame.x[30] = restorer;
    frame.sp = p.frame;
    frame.elr = d.action.sa_handler;
    frame.spsr &= !(PSR_BTYPE_MASK | PSR_TCO);
    Ok(())
}

/// `rt_sigreturn`: `sp` points at the `rt_sigframe`. Restores x0-x30, sp,
/// pc, PSTATE (validated) and FP/SIMD into `frame`; returns the saved
/// mask and `uc_stack`. `Err` on a bad frame (caller forces SIGSEGV).
pub fn restore_frame(frame: &mut TrapFrame) -> Result<(SigSet, StackT), ()> {
    let sp = frame.sp;
    if sp & 15 != 0 {
        return Err(());
    }
    let f: RtSigframe = crate::task::process()
        .space
        .lock()
        .read_val(sp as usize)
        .map_err(|_| ())?;
    let sc = &f.uc.uc_mcontext;
    if sc.pstate & PSR_INVALID != 0 {
        return Err(());
    }
    let fp = sc.read_fpsimd().ok_or(())?;
    frame.x = sc.regs;
    frame.sp = sc.sp;
    frame.elr = sc.pc;
    frame.spsr = sc.pstate;
    frame.fpsr = fp.fpsr as u64;
    frame.fpcr = fp.fpcr as u64;
    for (i, r) in fp.vregs.iter().enumerate() {
        frame.v[2 * i] = r[0];
        frame.v[2 * i + 1] = r[1];
    }
    Ok((f.uc.uc_sigmask, f.uc.uc_stack))
}
