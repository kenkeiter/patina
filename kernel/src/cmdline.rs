//! Minimal kernel command-line split (doc/cli-contract.md):
//!
//! ```text
//! console=<dev> [k=v ...] patina.epoch=<secs> -- arg1 arg2
//! ```
//!
//! Tokens before the first `--` are kernel parameters; tokens after it are
//! argv[1..]. The CLI percent-encodes `=` inside args, so a raw token that
//! contains `=` after `--` is a kernel parameter appended by a VMM (e.g.
//! QEMU microvm's `virtio_mmio.device=`), not an arg.
//!
//! This is the Phase 0 stand-in; a host-testable cmdline crate replaces it.

use alloc::string::String;
use alloc::vec::Vec;

/// Kernel parameter tokens (raw, not decoded).
pub fn params(cmdline: &str) -> impl Iterator<Item = &str> {
    let mut seen_sep = false;
    cmdline.split_whitespace().filter(move |t| {
        if *t == "--" && !seen_sep {
            seen_sep = true;
            return false;
        }
        !seen_sep || t.contains('=')
    })
}

/// Value of kernel parameter `key` (first occurrence), raw.
pub fn param<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    params(cmdline).find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
}

/// argv[1..], percent-decoded.
pub fn args(cmdline: &str) -> Vec<String> {
    cmdline
        .split_whitespace()
        .skip_while(|t| *t != "--")
        .skip(1)
        .filter(|t| !t.contains('='))
        .map(percent_decode)
        .collect()
}

/// `%XX` -> byte; everything else literal. Invalid UTF-8 is replaced.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
