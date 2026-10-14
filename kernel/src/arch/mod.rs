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
//!                                     // (KERNEL_VA_BASE maps to it)
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
