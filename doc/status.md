# patina status

Per-phase checklist against `doc/patina-design.md` §14. An item is
**verified** only when a named test or command actually exercised it on this
host (Apple Silicon macOS, QEMU 11.1.2, cargo 1.99.0); the test is named next
to it. Deviations from the design are recorded in `doc/decisions.md`.

Legend: `[x]` verified · `[~]` implemented but unverified (or only partly
verified; the note says what is missing) · `[ ]` not started.

## Re-running the checks

```
scripts/smoke.sh [aarch64] [x86_64]                 # build + boot, expect banner and exit 0
scripts/build-kernel.sh <aarch64|x86_64> [--debug]  # -> out/kernel-aarch64.Image, out/kernel-x86_64-pvh.elf
scripts/run-qemu.sh <arch> [initrd.cpio] [-- [params --] args]
scripts/run-qemu.sh aarch64 -- patina.selftest=panic|fault|alloc
```

Environment for `run-qemu.sh` (and `smoke.sh`): `PATINA_MEM` (MiB, default
512), `PATINA_CPUS` (default 1), `PATINA_ACCEL` (`hvf`/`kvm`/`tcg`),
`PATINA_TIMEOUT` (seconds, default 60), `PATINA_KERNEL`, `PATINA_LOG`,
`PATINA_QEMU_EXTRA`. `build-kernel.sh` honors `PATINA_PROFILE=dev|release`
(default release). `run-qemu.sh` exits with the code from the
`[patina] exit code N` serial line, 124 on timeout, 125 if no sentinel.

## Phase 0 — Bring-up

Test names used below (all aarch64, release profile unless noted):

| Name | Command | Observed |
|---|---|---|
| **build-a64** | `scripts/build-kernel.sh aarch64` | `out/kernel-aarch64.Image` (82120 bytes) + `out/kernel-aarch64.elf`; header: branch at 0, `text_offset` 0, `image_size` incl. bss, flags 0xa, magic `ARM\x64` at offset 56 |
| **boot-hvf** | `scripts/run-qemu.sh aarch64` (`-M virt,gic-version=3 -cpu host -accel hvf`) | `[patina] early: console arm,pl011 ... @ 0x9000000`; `[patina] early: final page tables live (11 pool pages), TTBR0 parked, PAN on`; banner `[patina] patina-kernel 0.1.0 (aarch64)`; memory map (usable / kernel 480 KiB at 0x40200000 / firmware (DTB) 1 MiB at 0x48000000); `ram: total 512 MiB`; cmdline/params/argv; `initrd: none`; `platform: dtb @ 0x48000000`; `frames: 128616 free / 130664 total`; `[patina] exit code 0`; script exit 0 via PSCI SYSTEM_OFF |
| **boot-tcg** | `PATINA_ACCEL=tcg scripts/run-qemu.sh aarch64` (`-accel tcg -cpu max`) | same output, exit 0 |
| **initrd-args** | `scripts/run-qemu.sh aarch64 t.cpio -- foo=bar -- a b%20c` | `initrd: 0x48000000-0x48000200 (512 bytes)` and an `initrd` region in the map; `params: ["console=ttyAMA0","patina.epoch=…","foo=bar"]`; `argv: ["/app","a","b c"]` |
| **selftest-panic** | `... -- patina.selftest=panic` | `[patina] panic: selftest at kernel/src/selftest.rs:27`, `[patina] exit code 255` |
| **selftest-fault** | `... -- patina.selftest=fault` | `unhandled exception: EL1h sync on cpu 0: data abort (same EL) (EC=0x25)`, `ESR 0x96000004 FAR 0xffffdeadbeef0000`, ELR/SPSR/x0–x30 dump, panic, exit 255 |
| **selftest-alloc** | `... -- patina.selftest=alloc` | 9 frame blocks (1328 frames) round-trip; 10k boxes + 6×3 MiB vecs; heap grows 8→28 MiB; `selftest alloc: ok`; exit 0 |
| **mem-sizes** | `PATINA_MEM=2048` / `128` / `64` | 523785 frames / 32385 frames with DTB at 0x44000000 / boots |
| **timeout** | `PATINA_TIMEOUT=3 PATINA_QEMU_EXTRA=-S scripts/run-qemu.sh aarch64` | exit 124 |
| **smoke-a64** | `scripts/smoke.sh aarch64` | `PASS aarch64`, exit 0 |
| **dev-profile** | `PATINA_PROFILE=dev scripts/build-kernel.sh aarch64`, then boot + selftests | boots on HVF and TCG; `alloc` and `fault` selftests pass |
| **clippy-a64** | `cargo clippy -p patina-kernel --target aarch64-unknown-none` | 0 warnings |
| **el2-drop** | TCG with `-M virt,virtualization=on` (GICv2 and GICv3; DTB PSCI method `smc`) and `-M virt,secure=on,virtualization=on` | exit 0 in all three |

### Workspace and build

- [x] Cargo workspace (`members = ["kernel", "crates/*"]`, `default-members = ["crates/*"]`), toolchain pinned to 1.99.0 with both bare-metal targets — **build-a64**
- [x] `patina-kernel` builds for `aarch64-unknown-none` (release and dev) — **build-a64**, **dev-profile**, **clippy-a64**
- [~] `patina-kernel` builds for `x86_64-unknown-none` — see x86_64 below
- [x] Per-arch linker script selected by `kernel/build.rs` (`src/arch/<arch>/linker.ld`); aarch64 script verified — **build-a64**
- [x] `panic = "abort"` in both profiles; release keeps debug info (`debug = true`), `out/kernel-aarch64.elf` carries symbols — **build-a64**
- [x] Host-testable crates under `crates/` (`patina-abi`, `patina-ctlproto`, `patina-cmdline`, `patina-cpio`); they are not yet used by the kernel

### aarch64

- [x] Linux arm64 `Image` header (`text_offset` 0, flags LE/4K/anywhere, `image_size` incl. bss) — **build-a64**, booted by QEMU in **boot-hvf**
- [x] EL2 → EL1h drop (HCR_EL2.RW, timer access, CPTR_EL2, GICv3 ICC_SRE_EL2) — **el2-drop**; EL1 entry path — **boot-hvf**
- [x] MMU: 4 KiB granule, 48-bit VA, kernel in TTBR1 at `KERNEL_VA_BASE`, direct map of all RAM at `PHYS_OFFSET`, TTBR0 parked on an empty table — **boot-hvf**, **mem-sizes**
- [x] Final kernel tables with per-section permissions and an unmapped stack guard page (`paging::init_kernel_tables`) — **boot-hvf** (log line); permissions not separately probed
- [x] PAN enabled after the `ID_AA64MMFR1_EL1` check — **boot-hvf** (`PAN on`)
- [x] Exception vectors with register dump (ESR/FAR/ELR/SPSR/x0–x30) then panic — **selftest-fault**
- [x] DTB parsing (`arch/aarch64/dtb.rs`): memory nodes — **boot-hvf**, **mem-sizes**; `bootargs` — **initrd-args**; `stdout-path` — **boot-hvf**; `linux,initrd-start/end` — **initrd-args**; `/psci` `method` (hvc and smc) — **boot-hvf**, **el2-drop**
- [~] `/reserved-memory` and `/memreserve/` → reserved regions: implemented, untested (QEMU emits neither)
- [~] DTB at the very end of RAM (Firecracker's placement): boot code maps the two 2 MiB blocks starting at x0's block, so it should work; untested (QEMU places it mid-RAM)
- [x] PL011 console — **boot-hvf**, **boot-tcg**
- [~] 16550 MMIO console (`reg-shift`, `reg-io-width`; Firecracker / Cloud Hypervisor arm64): implemented, untested (QEMU `virt` has no 16550)
- [x] Console selection from `console=` / DTB `stdout-path` / first UART — **boot-hvf** (`console=ttyAMA0` path only)
- [x] PSCI SYSTEM_OFF poweroff (HVC and SMC conduits) — **boot-hvf** (HVC), **el2-drop** (SMC)
- [ ] SMP: only `-smp 1` booted; secondary CPUs are not started (Phase 2)

### Arch-neutral kernel

- [x] Buddy frame allocator (`mm/frame.rs`) — **boot-hvf** (frame counts), **selftest-alloc**, **mem-sizes**
- [x] Kernel heap (`mm/heap.rs`, `buddy_system_allocator::Heap` + `SpinLock`, grows on demand) — **selftest-alloc**
- [x] Panic handler: `[patina] panic: …`, `[patina] exit code 255`, poweroff — **selftest-panic**
- [x] `kernel_main`: banner, memory map, RAM totals, cmdline/params/argv (percent-decoded, argv[0] = `/app`), initrd range, platform, frame/heap stats — **boot-hvf**, **initrd-args**
- [x] Exit sentinel `[patina] exit code N` then poweroff — **boot-hvf**, **selftest-panic**
- [x] Self-test hooks `patina.selftest=panic|fault|alloc` — the three selftest rows

### x86_64

TODO(supervisor): replace this block with x86_64 results once the parallel bring-up is verified.

All items below are in progress and unverified; no results are claimed.

- [~] `patina-kernel` builds for `x86_64-unknown-none`; `out/kernel-x86_64-pvh.elf`
- [~] PVH `ELFNOTE` (`XEN_ELFNOTE_PHYS32_ENTRY`), 32-bit protected-mode entry → long mode
- [~] Higher-half kernel at `KERNEL_VA_BASE` + direct map at `PHYS_OFFSET`, 4-level paging, NX
- [~] `hvm_start_info`: memory map, cmdline, module list (initrd)
- [~] 16550 console at port 0x3f8
- [~] IDT with register dump on fatal exceptions
- [~] Poweroff via `isa-debug-exit` with ACPI / i8042 fallback
- [~] `scripts/run-qemu.sh x86_64` (`-M microvm -accel tcg -cpu max`) and `scripts/smoke.sh x86_64`

### Scripts and docs

- [x] `scripts/build-kernel.sh` — **build-a64**, **dev-profile**
- [x] `scripts/run-qemu.sh`: cmdline per `doc/cli-contract.md`, initrd, params/args split, exit status from the sentinel, hard timeout — **boot-hvf**, **initrd-args**, **timeout**
- [x] `scripts/smoke.sh` — **smoke-a64**
- [x] `doc/status.md`, `doc/decisions.md`, `doc/crates.md` audit notes for every kernel dependency

## Per-target matrix

`V` verified (named test) · `U` untested (code written to the spec, never
booted) · `—` not applicable · `?` x86_64, in progress.

| Target | boot | console | poweroff | initrd |
|---|---|---|---|---|
| QEMU `virt` aarch64, HVF | V **boot-hvf** | V PL011 | V PSCI HVC | V **initrd-args** |
| QEMU `virt` aarch64, TCG | V **boot-tcg**, **el2-drop** | V PL011 | V PSCI HVC / SMC | V (same DTB path; **initrd-args** run on HVF) |
| QEMU `microvm` x86_64, TCG | ? | ? | ? | ? |
| Firecracker aarch64 | U (no KVM on this host) | U 16550 MMIO | U PSCI | U |
| Firecracker x86_64 | U | U | U | U |
| Cloud Hypervisor aarch64 | U | U 16550 MMIO | U PSCI | U |
| Cloud Hypervisor x86_64 | U | U | U | U |
| Apple VZ aarch64 | U (needs the CLI's VZ shim) | U (`hvc0` = virtio-console, not driven yet; falls back to a DTB UART if any) | U PSCI | U |

## Phase 1 — Hello

Prerequisites carried over from Phase 0 (see `doc/decisions.md`):

- [ ] Save/restore user FP/SIMD state on every kernel entry (the kernel itself uses FP/SIMD on `aarch64-unknown-none`), or move the kernel to a soft-float target
- [ ] Dedicated exception stack (today a kernel stack overflow recurses in the vector)
- [ ] Switch the kernel's cmdline parsing from `kernel/src/cmdline.rs` to `crates/cmdline` (`patina-cmdline`)

Phase 1 proper:

- [ ] ELF loader for static non-PIE Go binaries (`PT_LOAD` with flags, zero `.bss`); clear error for PIE/dynamic
- [ ] User address space in TTBR0 / user half of PML4; user mode entry (EL0 / ring 3)
- [ ] Syscall entry (`SVC` / `SYSCALL`) and per-arch ABI layer, `ENOSYS` + log-once for unknown numbers
- [ ] Initial user stack with Linux-exact argv/envp/auxv (`AT_PAGESZ`, `AT_PHDR/PHENT/PHNUM`, `AT_ENTRY`, `AT_RANDOM`, `AT_HWCAP`/`AT_HWCAP2`, `AT_SECURE`, `AT_UID`…)
- [ ] Load `/app` from the initrd cpio (`patina-cpio`)
- [ ] Tier-1 syscalls needed by single-threaded `fmt.Println`: `write`, `exit_group`, `mmap`/`munmap`, `rt_sigaction`, `rt_sigprocmask`, `sigaltstack`, `clock_gettime`, `getrandom`, `sched_getaffinity`, `arch_prctl` (x86), `uname`, `prlimit64`…
- [ ] User page faults → `SIGSEGV` (delivery may be stubbed until Phase 2)
- [ ] First Go binary (`testapps/hello`) prints and exits 0 on both arches

## Phase 2 — Runtime

- [ ] `clone` (thread flag set only, else `EINVAL`), kernel stacks with guard pages, TLS (`FS_BASE` / `TPIDR_EL0`)
- [ ] Per-vCPU runqueues, tick, IPIs, idle `WFI`/`HLT`
- [ ] SMP bring-up (PSCI `CPU_ON` / INIT-SIPI-SIPI)
- [ ] `futex` WAIT/WAKE (+ `_PRIVATE`, timeouts)
- [ ] Signals: per-thread/process pending + masks, Linux-exact `rt_sigframe`, `rt_sigreturn`, `sigaltstack`, `SIG_DFL` termination (128+signo), fault → `SIGSEGV`/`SIGBUS`/`SIGFPE`/`SIGILL`/`SIGTRAP`
- [ ] `tgkill`/`kill` (async preemption via `SIGURG`)
- [ ] Timers: `nanosleep`, `setitimer`, `timer_create`/`settime`/`delete`, per-thread CPU time (`SIGPROF`)
- [ ] Remaining Tier-1 syscalls (§7): `read openat close exit getpid gettid madvise mprotect mremap sched_yield gettimeofday getrlimit pipe2 fcntl ioctl(TCGETS→ENOTTY) epoll_create1 epoll_ctl epoll_pwait eventfd2`
- [ ] Interrupt controller (GICv3 / LAPIC+IOAPIC) and timers (generic timer / TSC, pvclock)
- [ ] Goroutines, GC, async preemption, pprof working
- [ ] Firecracker x86_64 + aarch64 boots (needs a KVM host)

## Phase 3 — OS

- [ ] VFS: initrd unpacked into a writable in-memory tree
- [ ] devfs (`null zero full urandom random console stdin stdout stderr`), procfs-lite, `/sys/kernel/mm/transparent_hugepage/hpage_pmd_size`, `/tmp`
- [ ] Pipes, eventfd, epoll over a common `File` trait
- [ ] Tier-2 file syscalls (§7): `pread64 pwrite64 readv writev lseek newfstatat/fstat statx getdents64 mkdirat unlinkat renameat2 readlinkat symlinkat linkat ftruncate fchmod fchown utimensat fsync faccessat2 dup dup3 getcwd chdir getuid geteuid getgid getegid`
- [ ] virtio-mmio discovery (DTB / `virtio_mmio.device=` / microvm probe)
- [ ] Control channel: virtio-vsock port 4242 and the `patina.ctl` virtio-serial port; `ping`, `signal`, `stop`, `time`, `dump`; `ready`, `exit`, `panic`, `log` events
- [ ] virtio-rng; QEMU scripts add the `patina.ctl` port and `virtio-rng-device`
- [ ] `os` package tests pass

## Phase 4 — Net

- [ ] virtio-net (`virtio-drivers`), smoltcp interface task
- [ ] Socket syscalls (Tier 2): `socket socketpair bind listen accept4 connect getsockname getpeername setsockopt getsockopt sendto recvfrom sendmsg recvmsg shutdown`
- [ ] Loopback (127.0.0.1, ::1), `AF_UNIX` stream + datagram
- [ ] `ip=` static and `dhcp`; generated `/etc/resolv.conf`, `/etc/hosts`
- [ ] CLI `build` / `run` / `ctl` shipped
- [ ] **Milestone A**: `net/http` hello + graceful `patina ctl stop` (Notify and exit-143 variants) on Firecracker, Cloud Hypervisor, QEMU, both arches (+ Apple VZ)

## Phase 5 — Hardening

- [ ] Runtime test-suite harness + skip list (**Milestone B**), Tier-3 syscalls, vDSO, virtio-pci (`q35`), console-port control fallback, `AF_VSOCK` for the program, `--strace`, multi-queue net, benchmarks.
