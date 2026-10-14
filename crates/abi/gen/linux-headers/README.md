# Linux ABI value extractor

Produces `crates/abi/src/linux_expected.rs`, the expected sizes, offsets and
constants that `src/layout_check.rs` compares every patina-abi struct and
constant against, for x86_64 and aarch64. The tests read only the generated
file; the headers are needed only to regenerate it.

Rerun (needs network, `curl`, `bsdtar`, and any clang with the X86 and
AArch64 backends, e.g. Xcode's):

```sh
inc=$(crates/abi/gen/linux-headers/fetch-headers.sh "$(mktemp -d)")
crates/abi/gen/linux-headers/extract.sh "$inc"
cargo test -p patina-abi
```

- `fetch-headers.sh` downloads Debian's `linux-libc-dev` (sanitized UAPI
  headers for all Debian arches; pinned by version and SHA-256) into an
  empty directory and unpacks it. Bump `DEB`/`SHA256` there to move to a
  newer kernel.
- `layout.c` lists what to extract; `kinternal.h` transcribes the
  kernel-internal pieces (rt_sigframe, rt_sigaction's struct sigaction,
  user_msghdr, MSG_*/AF_*/SOCK_*, arm64 signal-frame record placement,
  CPU-clock encoding) from the matching kernel source, with file:line.
  Re-check those citations when bumping the version.
- `extract.sh` compiles `layout.c` (plus every `__NR_*` and `E*` macro the
  headers define) with `clang --target=<arch>-linux-gnu -nostdinc -S` and
  scrapes the values from inline-asm `.ascii` directives. Nothing is linked
  or run.
- `stub/sys/socket.h` stands in for the one libc header a UAPI header
  (`linux/vm_sockets.h`) includes.

Current data: Linux 7.2.10 (`linux-libc-dev_7.2.10-1_all.deb`).
