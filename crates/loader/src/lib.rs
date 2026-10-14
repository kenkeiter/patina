//! Program loading for patina (design §4 "ELF loading", "Stacks"; §7 auxv).
//!
//! - [`elf`]: validates a static, non-PIE ELF64 executable (what
//!   `CGO_ENABLED=0 go build` produces for `GOOS=linux`) and exposes its
//!   `PT_LOAD` segments and the values the kernel needs for the auxv.
//! - [`stack`]: builds the initial user stack image (argc/argv/envp/auxv,
//!   strings, `AT_RANDOM` bytes) byte-for-byte the way Linux
//!   `fs/binfmt_elf.c` lays it out.
//!
//! Pure byte manipulation: no hardware access, no `unsafe`, host-testable.

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

extern crate alloc;

pub mod elf;
pub mod stack;

/// Page granularity the loader works at (both arches use a 4 KiB granule).
pub const PAGE_SIZE: u64 = 4096;

/// Exclusive end of the user half of the address space (48-bit VA).
pub const USER_VA_END: u64 = 0x0000_8000_0000_0000;

pub(crate) fn page_down(x: u64) -> u64 {
    x & !(PAGE_SIZE - 1)
}

/// Round up to a page; `None` on overflow.
pub(crate) fn page_up(x: u64) -> Option<u64> {
    x.checked_add(PAGE_SIZE - 1).map(page_down)
}
