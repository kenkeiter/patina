//! Early-boot kernel parameter lookup (doc/cli-contract.md).
//!
//! The full parser is `patina_cmdline::Cmdline` (argv, env, epoch, ...),
//! used by `kernel_main` once the heap exists. The two functions here do
//! not allocate, for the console choice in `arch/*/boot.rs` (before the
//! heap) and the boot self-test hook.
//!
//! Kernel parameters are the tokens before the first `--`, plus raw
//! `key=value` tokens after it (the CLI percent-encodes `=` in real args,
//! so such a token is a VMM-appended parameter; decision D11). Quoting is
//! not handled here; `patina_cmdline` handles it.

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
