//! Linux user ABI for x86_64 and aarch64, as seen by a static `GOOS=linux`
//! Go binary: syscall numbers, errno, flag constants, `#[repr(C)]` structs,
//! and signal-frame layouts.
//!
//! Layout:
//!
//! - Arch-neutral modules ([`errno`], [`signal`], [`time`], [`fs`], [`mm`],
//!   [`sched`], [`epoll`], [`net`], [`misc`], [`auxv`]) hold everything that is
//!   byte-identical on x86_64 and aarch64 (both use the 64-bit
//!   `asm-generic` definitions for these).
//! - [`x86_64`] and [`aarch64`] hold what differs: syscall numbers (`nr`),
//!   `struct stat`, `epoll_event` (packed on x86_64), `O_*` flags,
//!   `sigcontext`/`ucontext`/`rt_sigframe`, `MINSIGSTKSZ`, clone argument
//!   order, hwcaps. Both are compiled on every host so their layouts are
//!   testable anywhere.
//! - [`current`] re-exports the module for the target architecture.
//!
//! All structs derive [`zerocopy`] traits so the kernel can copy them to and
//! from user memory as bytes without `unsafe`. User-space pointers inside
//! structs are typed as `u64` (they are user virtual addresses, never
//! dereferenced directly by the kernel).
//!
//! Both targets are little-endian; helpers that pack sub-fields into byte
//! arrays assume that.
//!
//! `layout_check` (tests) compares every struct layout and constant here,
//! for both arches, against values compiled from the Linux UAPI headers
//! (plus transcribed kernel-internal definitions); see
//! `gen/linux-headers/README.md`. Syscall tables and errno are generated
//! from `golang.org/x/sys/unix` by `gen/gen.go` and checked the same way.

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod auxv;
pub mod epoll;
pub mod errno;
pub mod fs;
pub mod misc;
pub mod mm;
pub mod net;
pub mod sched;
pub mod signal;
pub mod time;

pub mod aarch64;
pub mod x86_64;

/// The ABI module for the architecture being compiled for.
#[cfg(target_arch = "x86_64")]
pub use crate::x86_64 as current;

/// The ABI module for the architecture being compiled for.
#[cfg(target_arch = "aarch64")]
pub use crate::aarch64 as current;

pub use zerocopy;

#[doc(hidden)]
pub use bitflags as __bitflags;

/// Look up a syscall name in a `(number, name)` table (see `nr::TABLE`).
pub fn syscall_name(table: &'static [(usize, &'static str)], nr: usize) -> Option<&'static str> {
    match table.binary_search_by_key(&nr, |&(n, _)| n) {
        Ok(mut i) => {
            // Return the first entry for this number (the primary name).
            while i > 0 && table[i - 1].0 == nr {
                i -= 1;
            }
            Some(table[i].1)
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod layout_check;
#[cfg(test)]
#[rustfmt::skip]
mod linux_expected;
