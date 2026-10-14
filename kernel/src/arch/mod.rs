//! Architecture abstraction.
//!
//! Every architecture module (`arch/<arch>/`) must provide the items below;
//! the arch-neutral kernel uses only these names (re-exported as `arch::*`).
//!
//! ```text
//! pub const PHYS_OFFSET: usize;      // 0xffff_8000_0000_0000, base of the direct map
//! pub const KERNEL_VA_BASE: usize;   // 0xffff_ffff_8000_0000, kernel image link address
//! pub const NAME: &str;              // "aarch64" / "x86_64", used in the banner
//! pub fn kernel_load_phys() -> usize; // physical address the image was loaded at
//!                                     // (aarch64: KERNEL_VA_BASE maps to it;
//!                                     // x86_64: image at KERNEL_VA_BASE + it)
//!
//! pub mod cpu {
//!     pub fn id() -> usize;             // logical CPU index (boot CPU = 0)
//!     pub fn halt() -> !;               // mask interrupts, idle forever
//!     pub fn wait_for_interrupt();      // WFI / HLT once
//! }
//! pub mod irq {
//!     pub struct Flags;                 // opaque saved interrupt state (Copy)
//!     pub fn save_disable() -> Flags;   // mask local IRQs, return previous state
//!     pub fn restore(f: Flags);         // restore state returned by save_disable
//!     pub fn enabled() -> bool;
//!     pub fn enable();                  // unmask local IRQs (kernel threads)
//! }
//! pub mod paging {
//!     bitflags! { pub struct MapFlags: u32 { READ, WRITE, EXEC, DEVICE, USER } }
//!     pub enum MapError { ... }
//!     // Map [va, va+len) -> [pa, pa+len) in the kernel tables. Page aligned.
//!     pub fn map_range(va: VirtAddr, pa: PhysAddr, len: usize, flags: MapFlags)
//!         -> Result<(), MapError>;
//!     // Map MMIO at PHYS_OFFSET + pa with device attributes; returns the VA of
//!     // `pa` itself (sub-page offset preserved). Panics on failure.
//!     pub fn map_device(pa: PhysAddr, len: usize) -> VirtAddr;
//!     pub fn flush_tlb_all();
//! }
//! pub mod power {
//!     pub fn poweroff(code: i32) -> !;  // never returns; halts if the VMM ignores it
//! }
//! pub mod console;                      // UART driver(s); arch boot code calls
//!                                       // crate::console::set(&'static dyn Console)
//! ```
//!
//! Phase 1 additions (user mode, syscalls, threads, timer). The arch layer
//! implements these; the arch-neutral core (`trap.rs`, `mm/`, `task/`,
//! `syscall/`) uses only these names. Hooks the arch calls into the core are
//! in `crate::trap` (see that file for the rules).
//!
//! ```text
//! pub mod ctx {
//!     /// Complete user register state saved at every entry from user mode
//!     /// (syscall, fault, interrupt). #[repr(C)], layout shared with the
//!     /// entry asm. Includes the TLS base (TPIDR_EL0 / FS_BASE) and, on
//!     /// aarch64, the FP/SIMD registers + FPCR/FPSR (saved eagerly on every
//!     /// entry because the kernel itself uses SIMD; see decision D5). On
//!     /// x86_64 (soft-float kernel) FP state lives in `FpState` instead.
//!     /// Lives on the current kernel stack while the thread is in the
//!     /// kernel; the core copies it into the thread struct for threads that
//!     /// have not run yet (thread 0, clone children).
//!     pub struct TrapFrame { .. }   // Clone
//!     impl TrapFrame {
//!         /// Fresh user state: pc = entry, sp = sp, every other register 0,
//!         /// PSTATE/RFLAGS = user mode with interrupts enabled.
//!         pub fn new_user(entry: u64, sp: u64) -> TrapFrame;
//!         pub fn syscall_nr(&self) -> usize;        // x8 / rax (orig)
//!         pub fn syscall_args(&self) -> [u64; 6];   // x0-x5 / rdi rsi rdx r10 r8 r9
//!         pub fn set_syscall_ret(&mut self, v: u64);// x0 / rax
//!         pub fn pc(&self) -> u64;  pub fn set_pc(&mut self, v: u64);
//!         pub fn sp(&self) -> u64;  pub fn set_sp(&mut self, v: u64);
//!         pub fn tls(&self) -> u64; pub fn set_tls(&mut self, v: u64);
//!         pub fn from_user(&self) -> bool;          // SPSR.M / CS.RPL
//!         pub fn dump(&self);                       // register dump via kprintln
//!     }
//!     /// FP/SIMD state kept per thread outside the TrapFrame (x86_64: the
//!     /// 512-byte FXSAVE area, 16-aligned; aarch64: empty, since the frame
//!     /// carries it). Default = the state a fresh thread starts with.
//!     pub struct FpState { .. }     // Default, Clone
//!     pub fn fp_save(into: &mut FpState);   // save the live user FP state
//!     pub fn fp_restore(from: &FpState);    // load it (before enter_user)
//!     /// Kernel-side callee-saved context for `switch`.
//!     pub struct KernelContext { .. }       // Default
//!     /// Context that, when switched to, runs `entry(arg)` on the kernel
//!     /// stack whose top is `stack_top` (16-aligned). `entry` never returns.
//!     pub fn new_kernel_context(stack_top: usize, entry: extern "C" fn(usize) -> !, arg: usize) -> KernelContext;
//!     /// Save the current kernel context into `prev` and resume `next`.
//!     /// Returns when another CPU/thread switches back to `prev`. Called
//!     /// with interrupts disabled; the restored thread continues with the
//!     /// interrupt state it had when it was switched out.
//!     pub unsafe fn switch(prev: *mut KernelContext, next: *const KernelContext);
//!     /// Enter user mode with `frame` (ERET / IRETQ), restoring FP state
//!     /// from the frame (aarch64) — call `fp_restore` first on x86_64.
//!     /// Runs `trap::before_return_to_user` first. Never returns.
//!     pub unsafe fn enter_user(frame: &TrapFrame) -> !;
//! }
//! pub mod uspace {
//!     /// Lowest address above user space (both arches: 47-bit user VA).
//!     pub const USER_VA_END: usize = 0x0000_8000_0000_0000;
//!     /// User page tables (TTBR0 root / a PML4 whose upper half mirrors the
//!     /// kernel's). 4 KiB pages only. `MapFlags::USER` is implied.
//!     pub struct UserTables { .. }
//!     impl UserTables {
//!         pub fn new() -> Option<UserTables>;
//!         pub fn root(&self) -> PhysAddr;
//!         /// Map one page; AlreadyMapped if `va` has a translation.
//!         pub fn map(&mut self, va: usize, pa: PhysAddr, flags: MapFlags) -> Result<(), MapError>;
//!         /// Remove the translation for `va` (TLB-invalidated); returns
//!         /// the frame that was mapped, or None.
//!         pub fn unmap(&mut self, va: usize) -> Option<PhysAddr>;
//!         /// Change the permissions of a mapped page (TLB-invalidated).
//!         pub fn protect(&mut self, va: usize, flags: MapFlags) -> bool;
//!         pub fn translate(&self, va: usize) -> Option<(PhysAddr, MapFlags)>;
//!         /// Make these the live user tables on this CPU.
//!         pub unsafe fn activate(&self);
//!     }
//! }
//! pub mod timer {
//!     /// Start the periodic tick at `trap::TICK_HZ`; each tick calls
//!     /// `trap::timer_tick`. Also unmasks IRQs on this CPU.
//!     pub fn init();
//!     /// Monotonic nanoseconds since boot (CNTVCT / calibrated TSC).
//!     pub fn now_ns() -> u64;
//! }
//! pub mod cpu {
//!     /// Kernel stack to use for the next entry from user mode on this CPU
//!     /// (x86_64: TSS.rsp0 + the per-CPU SYSCALL stack; aarch64: no-op,
//!     /// SP_EL1 is whatever the kernel left it at). Called by the scheduler
//!     /// before `switch`/`enter_user`.
//!     pub fn set_kernel_stack(top: usize);
//!     /// Idle: called with IRQs masked; waits for an interrupt, lets it
//!     /// run, returns with IRQs masked again. An IRQ that became pending
//!     /// after the caller's last check is never lost (x86 `sti; hlt`;
//!     /// aarch64 WFI wakes on a masked pending IRQ). The scheduler's idle
//!     /// loop uses it.
//!     pub fn idle_wait();
//!     /// (AT_HWCAP, AT_HWCAP2) for auxv.
//!     pub fn hwcap() -> (u64, u64);
//! }
//! ```
//!
//! Entry paths the arch must provide (all end by restoring `frame`):
//! * syscall (`SVC #0` / `SYSCALL`+`SYSRET` with `swapgs` and the per-CPU
//!   kernel stack) → `trap::syscall(&mut frame)`;
//! * user data/instruction fault → `trap::user_fault(..)`, then retry;
//! * other user sync exceptions → `trap::user_exception(..)`;
//! * timer IRQ (from user or kernel mode) → `trap::timer_tick(..)`;
//! * every return to user mode → `trap::before_return_to_user(..)`.
//!
//! Kernel-mode faults remain fatal (register dump + panic), on a stack that
//! survives a kernel stack overflow (per-CPU overflow stack on aarch64,
//! IST1 on x86_64).
//!
//! Boot contract: the arch entry (`arch/<arch>/boot.rs`) sets up the stack,
//! exception vectors, the higher-half kernel mapping and the direct map of all
//! RAM, registers a console, builds a `bootinfo::BootInfo`, calls
//! `mm::frame::init(&bi.memory)` then `mm::heap::init()`, and finally
//! `crate::kernel_main(&'static BootInfo)`. Fatal exceptions print a register
//! dump through `console::panic_write_fmt` and then `panic!`.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;
