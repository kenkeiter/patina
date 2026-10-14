# Decisions

Deviations from, or refinements of, `doc/patina-design.md`, one paragraph each.

## ABI layout check is generated from Linux 7.2.10 UAPI headers (§13)

`crates/abi/src/linux_expected.rs` holds sizes, alignments, offsets and
constant values for x86_64 and aarch64, produced by
`crates/abi/gen/linux-headers/extract.sh`: it compiles a C list of
`sizeof`/`offsetof`/macro references with `clang --target=<arch>-linux-gnu
-nostdinc -S` against Debian's `linux-libc-dev_7.2.10-1` (pinned by SHA-256)
and scrapes the values from the assembly, so nothing Linux-built is run and the
tests need no headers. Kernel-internal ABI that UAPI does not export (the
`rt_sigframe` structs, `rt_sigaction`'s `struct sigaction`, `user_msghdr`,
`MSG_*`/`AF_*`/`SOCK_*`, arm64 `__reserved` record placement, CPU-clock id
encoding) is transcribed from the v7.2.10 source into `kinternal.h` with
file:line citations and compiled the same way; frame-placement and
`AT_HWCAP` derivation logic is tested against the cited source in
`layout_check.rs`. Every extracted key must be consumed by a check, so the
list cannot silently drift. Syscall numbers and errno still come from
`golang.org/x/sys` via `gen/gen.go` but are now diffed against the headers'
full `__NR_*`/`E*` sets.

## `FUTEX_CMD_MASK` keeps the pre-7.2 value

Linux 7.2 added `FUTEX_ROBUST_UNLOCK` (512) and `FUTEX_ROBUST_LIST32` (1024)
and strips them in `FUTEX_CMD_MASK`. patina does not implement them, so its
mask still clears only `FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME`: an op that
carries a robust flag decodes to an unknown command and gets `ENOSYS`, as it
would on Linux < 7.2, instead of silently running the base op. Go does not use
these flags. The layout check compares the mask with the 7.2 value minus the
two flags.
