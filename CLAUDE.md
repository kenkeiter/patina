# patina

Rust unikernel that runs unmodified static `GOOS=linux` Go binaries. The design is `doc/patina-design.md` — read it; it is the spec. Deviations from it go in `doc/decisions.md` with a one-paragraph rationale.

## Toolchain

- `source ./env.sh` before any cargo command (puts rustup ahead of Homebrew's rust). The toolchain is pinned in `rust-toolchain.toml` (stable, `x86_64-unknown-none` + `aarch64-unknown-none`). No nightly features.
- Go 1.27 for the CLI and test programs. QEMU 11 from Homebrew (`qemu-system-aarch64` with `-accel hvf` works on this Apple Silicon host; x86_64 runs under TCG).
- Firecracker / Cloud Hypervisor cannot run on this host (no KVM). Write their code paths to the spec, mark them untested in `doc/status.md`.

## Layout

```
kernel/            patina-kernel: no_std, no_main binary. Arch code under src/arch/{aarch64,x86_64}/.
crates/abi/        Linux ABI: per-arch syscall numbers, #[repr(C)] structs, errno, signal frames. Host-testable.
crates/ctlproto/   Control-channel NDJSON parser/encoder (no serde). Host-testable.
crates/<other>/    Other arch-neutral, host-testable logic (cmdline, cpio, vfs core, ...).
cli/               Go module for the `patina` CLI (build / run / ctl / image).
testapps/          Go programs used as payloads in tests (hello, threads, signals, http, ...).
scripts/           Thin shell helpers (build kernel, make initrd, run qemu).
doc/               Design, decisions.md, status.md (per-phase checklist + what is verified).
```

Put logic that does not touch hardware in a `crates/` crate so `cargo test` runs it on the host. The kernel crate wires it to hardware.

## Commands

- Build kernel: `scripts/build-kernel.sh aarch64|x86_64` → `out/kernel-aarch64.Image`, `out/kernel-x86_64-pvh.elf`.
- Host tests: `cargo test --workspace --exclude patina-kernel`.
- Run under QEMU: `scripts/run-qemu.sh <arch> <initrd.cpio> [-- args]` (keep it working as features land).

## Rules

- Commits: plain messages, **no Co-Authored-By or any Claude/AI attribution lines**.
- Every crate dependency gets an audit note in `doc/crates.md` (version, why, no_std status, unsafe surface).
- Keep `doc/status.md` honest: mark items verified only when a test actually exercised them, and name the test.
- Every QEMU invocation in scripts/tests has a timeout so a hung guest can't block forever.
