# patina

patina is a Rust unikernel that runs unmodified, statically linked `GOOS=linux` Go binaries (`CGO_ENABLED=0`). The kernel is a Linux-ABI emulator for one process, not a general OS. Every feature exists because Go's runtime or standard library issues the syscall, because a test in Go's `runtime` suite needs it, or because the host lifecycle contract needs it (argv/env on the kernel cmdline, stdout on serial, and a control channel for stop, signals and the exit code). Go code runs in user mode (EL0 / ring 3) with real threads across SMP vCPUs. The program and its files travel as an initrd that is unpacked into memory at boot. Targets are QEMU, Firecracker, Cloud Hypervisor and Apple Virtualization.framework, on aarch64 and x86_64. The spec is [doc/patina-design.md](doc/patina-design.md).

## Status

The per-phase checklist is in [doc/status.md](doc/status.md). Items are marked verified only when a named test exercised them. All verification so far ran on one Apple Silicon macOS host (QEMU 11.1.2, cargo 1.99.0, Go 1.27.1).

| VMM | Milestone A (`net/http` hello, curl from the host, `patina ctl stop`) | Milestone B (Go std package tests) |
|---|---|---|
| QEMU aarch64 (`virt`, HVF) | Verified: `scripts/test-net.sh` | Verified: 13 packages, 0 failures at 1, 4 and 8 vCPUs |
| QEMU x86_64 (`microvm`/`q35`, TCG) | Verified: `scripts/test-net.sh` | Verified: 13 packages, 0 failures at 1 vCPU, 5 packages at 4 vCPUs |
| Apple VZ (aarch64) | Verified: `scripts/test-vz.sh` (console on `hvc0`, vsock control channel, NAT networking, `AF_VSOCK` for the program) | Verified: `os` and `net` packages, 0 failures (`PATINA_VMM=vz scripts/test-gopkg.sh`) |
| Firecracker (both arches) | Written to the spec, unverified (no KVM host). The CLI driver is tested only against fakes. | Not run |
| Cloud Hypervisor (both arches) | Written to the spec, unverified (no KVM host). The CLI driver is tested only against fakes. | Not run |

Milestone B covers `runtime`, `os`, `net`, `os/signal`, `time`, `sync`, `syscall`, `io`, `bufio`, `context`, `net/http`, `runtime/pprof` and `os/user`, with `-test.short`. The skip lists in `testapps/skiplist/` hold 36 tests, all of which need `os/exec` or a `GODEBUG` re-exec.

## Prerequisites

- **Rust via rustup.** The toolchain is pinned to 1.99.0 in `rust-toolchain.toml`, with `x86_64-unknown-none`, `aarch64-unknown-none`, `rust-src` and `llvm-tools`. Stable only, no nightly features. `env.sh` puts Homebrew's rustup (`/opt/homebrew/opt/rustup/bin`) ahead of Homebrew's own `rust` formula, which is too old for the pin. **`env.sh` is Homebrew-specific.** With a rustup.rs install, put `~/.cargo/bin` first on `PATH` instead. The scripts source `env.sh` themselves.
- **Go 1.27** for the CLI and the test programs. The Milestone B skip lists are tied to Go 1.27.1.
- **QEMU 11** (`qemu-system-aarch64`, `qemu-system-x86_64`, from Homebrew). On Apple Silicon, aarch64 runs with `-accel hvf` and x86_64 runs under TCG, which is slower. QEMU on macOS has no vsock, so the control channel uses a virtio-console port there instead.
- **Xcode command line tools** for `cli/scripts/build.sh`, which needs cgo for the VZ driver and `codesign` for the `com.apple.security.virtualization` entitlement. A plain `go build` works for QEMU, but `--vmm vz` will refuse to run.
- `python3`, `curl` and `cpio` for some test scripts (`test-ctl.sh`, `test-net.sh`, `mkinitrd.sh`).
- Firecracker and Cloud Hypervisor need a Linux host with KVM, and their binaries on `PATH` (or `--vmm-bin`).

## Quick start

These commands were verified from a fresh clone. The Milestone A loop below is the same one `scripts/test-net.sh` runs.

```sh
source ./env.sh
cargo test --workspace --exclude patina-kernel     # host tests
scripts/build-kernel.sh aarch64                    # -> out/kernel-aarch64.Image
scripts/build-kernel.sh x86_64                     # -> out/kernel-x86_64-pvh.elf
scripts/smoke.sh                                   # boot both kernels: PASS aarch64 / PASS x86_64
scripts/test-hello.sh                              # testapps/hello on both arches

(cd cli && go build ./... && go test ./...)
cli/scripts/build.sh                               # -> cli/bin/patina (ad-hoc signed)

# Milestone A by hand
cli/bin/patina build ./testapps/httphello --target qemu-aarch64 -o /tmp/http.patina
cli/bin/patina run /tmp/http.patina --name http --net user --publish 18080:8080 &
curl http://127.0.0.1:18080/                       # hello from patina
cli/bin/patina ctl http stop                       # {"ev":"exit","code":0,"forced":false}
```

`patina build` looks for the kernel in `$PATINA_KERNEL_DIR`, then in `<repo>/out/`. It finds the repo by walking up from the working directory and from the binary's own location. A `patina` binary copied outside the repo therefore needs `PATINA_KERNEL_DIR=<repo>/out` or `--kernel <path>`. Downloading release kernels is not implemented. Use `--target qemu-x86_64` for x86_64.

## CLI

```
patina build <pkg> --target <vmm>-<arch> -o <out.patina> [--files src:dst ...] [--kernel path] [--tags t] [--name n]
patina image inspect <image.patina> [--json]
patina run <image.patina> [--vmm qemu|firecracker|cloud-hypervisor|vz] [--mem MiB] [--cpus N]
           [--net none|user|<tap>] [--publish host:guest] [--ip addr/prefix[,gw[,dns]]]
           [--env K=V] [--name vm] [--timeout d] [--grace d] [--strace] [-v] [-- args...]
patina ctl <vm> stop [--grace 30s] | signal <SIG> | dump | ping | time
patina ctl --sock <path> | --vsock <path> <op> ...
patina ps
patina version
```

The available targets are `qemu`, `firecracker`, `cloud-hypervisor` and `vz`, each on `x86_64` or `aarch64`. `vz` is aarch64 only. `--net user` works on QEMU (slirp, with `--publish host:guest`) and on `vz` (macOS NAT; the guest address is printed and reachable from the host directly, so `--publish` is QEMU-only).

`patina run` exits with the guest program's exit code. A program with no `SIGTERM` handler that gets stopped exits with 143. A forced stop exits with 137, and `--timeout` exits with 124. The full kernel/CLI contract, including the cmdline, devices, exit codes and run state, is in [doc/cli-contract.md](doc/cli-contract.md).

## Repository layout

```
kernel/        patina-kernel: no_std, no_main. Arch code in src/arch/{aarch64,x86_64}/.
crates/        Host-testable logic: abi, ctlproto, cmdline, cpio, vfs, inet, loader, vdso, rand, fsutil.
cli/           Go module for the patina CLI (build / run / ctl / image / ps).
testapps/      Go programs used as payloads in tests; skiplist/ holds the Milestone B skip lists.
scripts/       Kernel build, initrd packing, QEMU runner, and the test suites.
doc/           Design, decisions, status, crate audits, CLI contract.
env.sh         PATH fix-up for Homebrew rustup.
```

Build output goes to `out/` (kernels, test logs under `out/test/`) and `target/`.

## Tests

The scripts work from any directory. Each one takes `aarch64` and/or `x86_64` and runs both by default. Every QEMU run has a timeout (`PATINA_TIMEOUT`, in seconds). The environment knobs (`PATINA_MEM`, `PATINA_CPUS`, `PATINA_ACCEL`, ...) are listed in [doc/status.md](doc/status.md#re-running-the-checks).

| Command | What it checks |
|---|---|
| `cargo test --workspace --exclude patina-kernel` | Host unit tests for the `crates/`. |
| `cd cli && go test ./...` | CLI unit tests, with fake VMMs. `PATINA_E2E=1 go test ./internal/run -run E2E -v` adds the end-to-end run against the real kernel. |
| `scripts/smoke.sh` | Builds and boots the kernel, then expects the banner and exit 0. |
| `scripts/test-hello.sh` | Runs `testapps/hello` (argv and exit code). `--app <name>` runs any testapp that prints `PASS <name>`, for example `fileio`, `sockets`, `unixsock` or `fdtest`. |
| `scripts/test-signals.sh` | Signal delivery, the no-handler death, sigstress, and crash/breakpoint signals. |
| `scripts/test-smp.sh` | Runs a testapp (default `smpstress`) at several vCPU counts. |
| `scripts/test-runtime.sh` | Phase 2 umbrella: hello, threads, smpstress, fdtest, signals and pproftest at 1, 2 and 4 vCPUs. |
| `scripts/test-ctl.sh` | The control channel over the QEMU `patina.ctl` port: ready/ping, stop, signal, dump, malformed input. |
| `scripts/test-os.sh` | Go's `os` package tests under patina (a wrapper around `test-gopkg.sh`). |
| `scripts/test-net.sh` | Milestone A on QEMU: httphello via `patina run --net user --publish`, curl, `ctl stop`, DHCP, virtio-pci. |
| `scripts/test-vz.sh` | Milestone A on Apple VZ (build and sign the CLI with `cli/scripts/build.sh` first): console, `ctl` ops, stop modes, httphello over NAT, threads/signals/sockets, `AF_VSOCK` echo. `cli/scripts/vztest.sh --e2e` runs the Go end-to-end variant. |
| `scripts/test-gonet.sh` | Go's `net` (or `--pkg net/http`) package tests (a wrapper around `test-gopkg.sh`). |
| `scripts/test-robust.sh` | Phase 5 robustness: tickless idle, sub-tick timers, fault to signal, stack/page-table reclaim, OOM, signal/fd edge cases. |
| `scripts/test-gopkg.sh <pkg>... [--arch a] [--cpus N]` | Milestone B: any Go std package's tests, packed as `/app`, with `testapps/skiplist/<pkg>.txt`. |
| `scripts/bench.sh` | Per-syscall benchmarks (`testapps/bench`), printing a table of medians. Not pass/fail. |

On a shared host, x86_64 under TCG can need a larger `PATINA_TIMEOUT` (and `GOPKG_TEST_TIMEOUT` for `test-gopkg.sh`).

## Documentation

- [doc/patina-design.md](doc/patina-design.md): the design. It is the spec.
- [doc/decisions.md](doc/decisions.md): deviations from the design and implementation decisions (D-numbers), each with its rationale.
- [doc/status.md](doc/status.md): per-phase checklist, each item with the test that verified it, and the per-target matrix.
- [doc/crates.md](doc/crates.md): an audit note for every crate dependency (version, purpose, no_std status, unsafe surface).
- [doc/cli-contract.md](doc/cli-contract.md): the kernel/CLI contract (cmdline, devices per VMM, control channel, exit codes, run state).

## Limitations by design

- **One process.** There is no `fork`, `execve` or `os/exec`. Threads (`clone` with `CLONE_THREAD`) are supported; new processes are not.
- **Nothing persists.** The filesystem is the initrd, unpacked into memory and writable, and there is no block device. Everything written is gone at power-off.
- **Static Go only.** The kernel rejects dynamic or PIE ELFs, and cgo is not supported.
- **Unauthenticated control channel.** Anyone who can reach the VMM's vsock or console socket can stop or signal the program. Those sockets are host-local and protected by filesystem permissions (design §15).
- **Direct kernel boot only.** There is no UEFI. Each arch has one kernel artifact: a PVH ELF on x86_64 and a Linux `Image` on aarch64.
