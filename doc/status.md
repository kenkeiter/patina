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
scripts/run-qemu.sh aarch64 -- patina.selftest=panic|fault|alloc|user|userfault|timer|switch|vm|sched|kstack
scripts/test-hello.sh [aarch64] [x86_64]            # build hello, pack initrd, run, check output + exit 0/3
scripts/test-hello.sh --threads <arch>              # run testapps/threads and show its output
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
- [x] `patina-kernel` builds for `x86_64-unknown-none` — **build-x86** (see x86_64 below)
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

Test names (all QEMU 11.1.2 `-M microvm -accel tcg -cpu max`, release profile unless noted):

| Name | Command | Observed |
|---|---|---|
| **build-x86** | `scripts/build-kernel.sh x86_64`; `llvm-readelf -l -n out/kernel-x86_64-pvh.elf` | three `PT_LOAD` segments at physical 0x200000/0x20c000/0x20f000 (vaddr 0xffffffff802xxxxx, R E / R / RW), one `PT_NOTE` with owner `Xen` type 0x12 desc = 0x200000 (`_start32`) |
| **boot-x86** | `scripts/run-qemu.sh x86_64` | `[patina] early: console 16550 @ port 0x3f8 ... start_info @ 0x1580 v1 ... memmap 6 entries`, the raw PVH memmap, `final page tables live (7 pool pages, ...), identity map dropped, NX on`, banner `[patina] patina-kernel 0.1.0 (x86_64)`, memory map (8 KiB firmware = start_info/cmdline/memmap, 628 KiB reserved low memory, kernel 448 KiB at 0x200000, 510 MiB usable), `platform: pvh start_info @ 0x1580`, `frames: 128369 free / 130671 total`, `[patina] exit code 0`; QEMU terminated by isa-debug-exit, script exit 0 in under a second |
| **initrd-args-x86** | `scripts/run-qemu.sh x86_64 t.cpio -- foo=bar -- a b%20c` | `initrd: 0x1ffff000-0x1ffff200 (512 bytes)`, `params: [..., "foo=bar"]`, `argv: ["/app","a","b c"]`; a 3 MB initrd also reported correctly |
| **selftest-panic-x86** | `... -- patina.selftest=panic` | exit 255 |
| **selftest-fault-x86** | `... -- patina.selftest=fault` | `unhandled exception: #PF page fault (vector 14)`, `error code 0x0 (not present, read, kernel mode)`, `CR2 0xffffdeadbeef0000`, RIP/CS/RFLAGS/RSP/SS/CR3 + GPR dump, panic, exit 255 |
| **selftest-alloc-x86** | `... -- patina.selftest=alloc` | `selftest alloc: ok`, exit 0 |
| **mem-sizes-x86** | `PATINA_MEM=128` / `PATINA_MEM=4096` | 32391 frames / memmap `0x100000000-0x140000000 ram` shown as `1 GiB usable`, 1047887 frames; a temporary test wrote and read back PA 0x13ffff000 through the direct map |
| **cpus-x86** | `PATINA_CPUS=2 scripts/run-qemu.sh x86_64` | boots and exits 0 (APs never started) |
| **double-fault-x86** | temporary (uncommitted) stack-recursion test | `#DF double fault` dump on IST1, exit 255 (no triple fault) |
| **machines-x86** | `PATINA_MACHINE=pc` / `q35` | both boot and exit 0 with initrd; on `pc` without `isa-debug-exit` the probe read 0xff and QEMU still exited through the ACPI 0x604 / i8042 fallback (which of the two is unknown); `microvm` without `isa-debug-exit` hangs until the timeout (no PM port, no i8042) |
| **smoke-both** | `scripts/smoke.sh` | `PASS aarch64`, `PASS x86_64`, exit 0 |
| **dev-profile-x86** | `scripts/build-kernel.sh x86_64 --debug`, then boot + selftests | boots; `alloc` ok, `fault` shows the CR2 dump |
| **clippy-x86** | `cargo clippy -p patina-kernel --target x86_64-unknown-none` | 0 warnings (aarch64 re-run: still 0) |

- [x] `patina-kernel` builds for `x86_64-unknown-none` (release and dev); `out/kernel-x86_64-pvh.elf` — **build-x86**, **dev-profile-x86**, **clippy-x86**
- [x] PVH `ELFNOTE` (`XEN_ELFNOTE_PHYS32_ENTRY`) in a `PT_NOTE` segment; `p_paddr` = physical load address — **build-x86**, loaded by QEMU in **boot-x86**
- [x] 32-bit protected-mode entry → long mode (`arch/x86_64/boot.rs`: boot GDT, 4-level boot tables, PAE/LME/NXE/PG) — **boot-x86**
- [x] Higher-half kernel window at `KERNEL_VA_BASE` (image linked at `KERNEL_VA_BASE + 0x200000`), direct map of all RAM at `PHYS_OFFSET` (2 MiB pages, RW+NX, > 4 GiB covered), identity map dropped at the CR3 switch — **boot-x86**, **mem-sizes-x86**
- [x] Final kernel tables with per-section permissions (`.text` R-X, `.rodata` R+NX, data/bss RW+NX) and an unmapped boot-stack guard page — **boot-x86** (log line); permissions not separately probed
- [x] `hvm_start_info` (v1): memmap — **boot-x86**, **mem-sizes-x86**; cmdline — **initrd-args-x86**; modlist (initrd = module 0) — **initrd-args-x86**; `rsdp_paddr` recorded (0 on this QEMU)
- [~] `hvm_start_info` v0 / no memmap: deliberately panics with a clear message; untested (no such VMM here)
- [x] GDT (kernel/user selectors), TSS with IST1 for `#DF`, IDT with 32 exception gates and full register dump — **selftest-fault-x86**, **double-fault-x86**
- [x] 16550 console at port 0x3f8 (`console=ttyS0`) — **boot-x86**
- [x] Poweroff via `isa-debug-exit` (probe `inb(0xf4) != 0xff`) — **boot-x86**; ACPI PM1a 0x604 S5 / i8042 reset fallback — **machines-x86** on `pc` (one of the two worked; untested individually)
- [ ] Cloud Hypervisor ACPI shutdown (FADT/DSDT) and microvm GED sleep register: not implemented
- [x] `scripts/run-qemu.sh x86_64` (`-M microvm -accel tcg -cpu max`, `PATINA_MACHINE` override) and `scripts/smoke.sh x86_64` — **boot-x86**, **smoke-both**
- [ ] SMP: APs are never started (Phase 2)

### Scripts and docs

- [x] `scripts/build-kernel.sh` — **build-a64**, **dev-profile**
- [x] `scripts/run-qemu.sh`: cmdline per `doc/cli-contract.md`, initrd, params/args split, exit status from the sentinel, hard timeout — **boot-hvf**, **initrd-args**, **timeout**
- [x] `scripts/smoke.sh` — **smoke-a64**
- [x] `doc/status.md`, `doc/decisions.md`, `doc/crates.md` audit notes for every kernel dependency

## Per-target matrix

`V` verified (named test) · `U` untested (code written to the spec, never
booted) · `—` not applicable.

| Target | boot | console | poweroff | initrd |
|---|---|---|---|---|
| QEMU `virt` aarch64, HVF | V **boot-hvf** | V PL011 | V PSCI HVC | V **initrd-args** |
| QEMU `virt` aarch64, TCG | V **boot-tcg**, **el2-drop** | V PL011 | V PSCI HVC / SMC | V (same DTB path; **initrd-args** run on HVF) |
| QEMU `microvm` x86_64, TCG | V **boot-x86** | V 16550 port 0x3f8 | V isa-debug-exit | V **initrd-args-x86** |
| QEMU `pc` / `q35` x86_64, TCG | V **machines-x86** | V 16550 | V isa-debug-exit; ACPI/i8042 fallback (one of them) | V **machines-x86** |
| Firecracker aarch64 | U (no KVM on this host) | U 16550 MMIO | U PSCI | U |
| Firecracker x86_64 | U | U | U | U |
| Cloud Hypervisor aarch64 | U | U 16550 MMIO | U PSCI | U |
| Cloud Hypervisor x86_64 | U | U | U | U |
| Apple VZ aarch64 | U (needs the CLI's VZ shim) | U (`hvc0` = virtio-console, not driven yet; falls back to a DTB UART if any) | U PSCI | U |

## Phase 1 — Hello

Test names (both arches unless noted; aarch64 = QEMU `virt` HVF, x86_64 = QEMU `microvm` TCG, release profile):

| Name | Command | Observed |
|---|---|---|
| **test-hello** | `scripts/test-hello.sh` | per arch, no args: `hello from patina`, `argv: ["/app"]`, `PASS hello (exit 0)`, `[patina] exit code 0`; with `-- 3 a b`: `argv: ["/app" "3" "a" "b"]`, `PASS hello (exit 3)`, `[patina] exit code 3`; final `PASS aarch64`, `PASS x86_64`, script exit 0 |
| **test-hello-tcg** | `PATINA_ACCEL=tcg scripts/test-hello.sh aarch64` | `PASS aarch64` |
| **hello-strace** | `scripts/run-qemu.sh aarch64 out/test/hello-aarch64.cpio -- patina.strace=1 -- 3 a b` | `[strace] 1 clone(0x50f00, …) = 2`, `… = 3`, `[strace] 3 futex(…, 0x81 WAKE_PRIVATE, 1) = 1` / `[strace] 1 futex(…, 0x80 WAIT_PRIVATE, 0) = 0`, `[strace] 2 nanosleep(…) = 0` (sysmon), `[strace] 1 exit_group(0x3)`; x86_64 shows `arch_prctl(0x1002 ARCH_SET_FS, …) = 0` and `clone(0xd0f00, …)` |
| **selftest-user** | `scripts/run-qemu.sh <arch> -- patina.selftest=user` | hand-assembled EL0/ring-3 code: `selftest user: hello` via `write`, store to a mapped page, `exit_group(7)` → `[patina] exit code 7` |
| **selftest-userfault** | `… patina.selftest=userfault` | the hello line, `[patina] fatal: user Write fault (NotPresent) at 0x401000`, register dump, exit 139 |
| **selftest-timer** | `… patina.selftest=timer` | aarch64 `gic: v3 dist @ 0x8000000, redist @ 0x80a0000`, `timer: virtual timer INTID 27, 24000000 Hz counter`; both: `selftest timer: ok (5 ticks in 52 ms at 100 Hz)`, exit 0 |
| **selftest-switch** | `… patina.selftest=switch` | ping-pong between the boot context and a fresh kernel context, `selftest switch: ok` |
| **selftest-vm** | `… patina.selftest=vm` | mmap hints, top-down fallback, `MAP_FIXED` over a reservation, uaccess across pages, `mprotect` to NONE and back, `mremap` (refused without MAYMOVE, moved with it), `DONTNEED` zeroing, frames returned on munmap: `selftest vm: ok (7 frames held by page tables)` |
| **selftest-sched** | `… patina.selftest=sched` | three kernel threads: `spinner preempted 6 times, sleeps 3, woken true, timed out true, stacks 3 allocated / 2 free`, `selftest sched: ok` |
| **selftest-kstack** | `… patina.selftest=kstack` | aarch64: `kernel stack overflow (sp 0xffffa00000000e70); running on the overflow stack` + dump; x86_64: `#DF double fault` on IST1 with RSP in the guard page; exit 255 |
| **gic-variants** (aarch64, TCG) | `PATINA_ACCEL=tcg PATINA_QEMU_EXTRA="-M virt,virtualization=on"` (also `gic-version=2`, `secure=on`) with `patina.selftest=user` / `timer` | exit 7 / `timer: ok` in every combination; GICv2 logs `gic: v2 dist @ 0x8000000, cpu @ 0x8010000` |
| **x86-nofeat** | `PATINA_QEMU_EXTRA="-cpu max,-fsgsbase,-xsave,-smep,-smap"` with the four arch selftests | same results through the MSR FS_BASE and FXSAVE fallbacks |
| **loader-errors** | initrd without `app` / shell script as `app` / x86 binary on aarch64 / random bytes | `fatal: /app not found in the initrd` (127) / `/app: not an ELF file (bad magic)` (126) / `ELF built for the wrong architecture: expected aarch64 (GOARCH=arm64), found e_machine 62 (x86_64)` (126) / `initrd is not a valid newc cpio archive` (127) |
| **threads-app** | `scripts/test-hello.sh --threads aarch64` (also x86_64) | `threads: GOMAXPROCS=1 NumCPU=1`, 1000 goroutines + GC workers run across several threads, then `[patina] ENOSYS 20 (epoll_create1)`, `runtime: epollcreate failed with 38`, `fatal error: runtime: netpollinit failed`, exit 2 (needs epoll/eventfd2/pipe2 — Phase 3 — and `SIGURG` delivery — Phase 2) |
| **host-tests** | `cargo test --workspace --exclude patina-kernel` | 93 tests: abi 35, cmdline 11, cpio 10, ctlproto 22, loader 14 (incl. parsing `go build` output for both arches and rejecting `-buildmode=pie`) |
| **clippy-p1** | `cargo clippy -p patina-kernel --target {aarch64,x86_64}-unknown-none` | 0 warnings on both |
| **smoke-p1** | `scripts/smoke.sh` | `PASS aarch64`, `PASS x86_64` (no initrd → Phase 0 path, exit 0) |

Prerequisites carried over from Phase 0 (see `doc/decisions.md`):

- [x] User FP/SIMD state saved on every kernel entry on aarch64 (eager, in the trap frame; D28); x86_64 kernel is soft-float, FP saved on switch with XSAVE/FXSAVE — **selftest-user**, **test-hello** (Go uses NEON/SSE throughout), **x86-nofeat**; a temporary user loop verified q/FPCR/TPIDR_EL0 survive ticks that clobber them in the kernel (not committed)
- [x] Dedicated exception stack: aarch64 per-CPU overflow stack (D29), x86_64 IST1 — **selftest-kstack**
- [x] Kernel cmdline parsing via `patina-cmdline` (`kernel/src/cmdline.rs` keeps only a no-alloc `param` for early boot); raw `k=v` after `--` is a kernel param (D11) — **test-hello** (argv), **hello-strace** (`patina.strace=1` after `--`), cmdline host tests

Phase 1 proper:

- [x] ELF loader for static non-PIE Go binaries (`patina-loader`): `PT_LOAD` with flags, eager fill, zeroed bss, `AT_PHDR` computed as Linux does; clear errors for PIE/dynamic/wrong arch; arm64 link base 0x10000 with 64 KiB `p_align` handled at 4 KiB — **test-hello**, **loader-errors**, **host-tests**
- [x] User address space (TTBR0 / user half of PML4), lazy anonymous mmap honouring Go's hints, `PROT_NONE` reservations without frames, `munmap`/`mprotect`/`madvise`/`mremap`/`brk`/`mincore` (D34, D36) — **selftest-vm**, **test-hello** (Go's arena reservations and `MAP_FIXED` commits in **hello-strace**)
- [x] User mode entry (EL0 / ring 3), syscall entry (`SVC`; `SYSCALL`/`SYSRET` with `swapgs`, per-CPU GS, TSS `rsp0`), TLS (`TPIDR_EL0`; `FS_BASE` via `arch_prctl`, FSGSBASE when available), `ENOSYS` + log-once, `patina.strace=1` (D27, D32, D37) — **selftest-user**, **hello-strace**, **threads-app** (`ENOSYS 20 (epoll_create1)` line)
- [x] Initial user stack with Linux-exact argv/envp/auxv (`AT_PAGESZ`, `AT_PHDR/PHENT/PHNUM`, `AT_ENTRY`, `AT_RANDOM`, `AT_HWCAP`/`AT_HWCAP2` (arm64 from `ID_AA64ISAR0_EL1` + `HWCAP_CPUID`, D31), `AT_SECURE`, `AT_UID…`, `AT_CLKTCK`, `AT_PLATFORM`, `AT_EXECFN`); argv[0] = `/app`, env from `patina.env` — **test-hello** (`argv:` line), **host-tests** (stack layout decoded back)
- [x] `/app` loaded from the initrd cpio (`patina-cpio`) — **test-hello**, **loader-errors**
- [x] Tier-1 syscalls needed by hello: `write read openat close fcntl ioctl fstat newfstatat mmap munmap mprotect madvise mremap brk mincore rt_sigaction rt_sigprocmask sigaltstack getpid gettid getuid… set_tid_address uname sched_getaffinity sched_yield prlimit64 getrlimit getrandom clock_gettime gettimeofday nanosleep clock_nanosleep clone futex exit exit_group tgkill kill arch_prctl(x86)` — **test-hello**, **hello-strace**
- [x] Threads: `clone` (Go's flag set, `CLONE_SETTLS` on amd64), per-thread 32 KiB kernel stacks with guard pages, uniprocessor round-robin scheduler with a 100 Hz tick (generic timer + GICv3/v2; LAPIC timer calibrated on the PIT), wait queues, sleep list, futex `WAIT`/`WAKE` (+`_BITSET`, `_PRIVATE`, timeouts), thread `exit` with `clear_child_tid` (D30, D33, D38–D41) — **test-hello** (Go's sysmon/GC threads), **hello-strace**, **selftest-sched**, **selftest-timer**, **selftest-kstack**
- [x] User page faults: lazy fill, else a fatal dump (registers + VMA table) and exit 139 — **selftest-userfault**; `SIGSEGV` delivery is Phase 2
- [~] Signal state: `rt_sigaction` for 1..64 (KILL/STOP rejected), per-thread `rt_sigprocmask`, `sigaltstack` stored and validated — **hello-strace** (114 `rt_sigaction` calls, `sigaltstack`, `rt_sigprocmask`); nothing is delivered, `rt_sigreturn` is fatal, `tgkill`/`kill` only log (so Go's `SIGURG` async preemption is a no-op)
- [x] `exit_group` / last thread exit → `[patina] exit code N` and poweroff — **test-hello** (exit 0 and 3)
- [x] First Go binary (`testapps/hello`) prints and exits with the right code on both arches — **test-hello**, **test-hello-tcg**
- [~] `AT_RANDOM`/`getrandom` from a non-cryptographic PRNG (D35) until virtio-rng (Phase 3)
- [ ] `patina run` end-to-end through the CLI (the CLI's QEMU driver and bundle format are untested against this kernel; `scripts/test-hello.sh` uses `scripts/run-qemu.sh`)

## Phase 2 — Runtime

- [x] `clone` (thread flag set only, else `EINVAL`), kernel stacks with guard pages, TLS (`FS_BASE` / `TPIDR_EL0`) — done in Phase 1 (**test-hello**, **selftest-kstack**)
- [~] Per-vCPU runqueues, tick, IPIs, idle `WFI`/`HLT`: uniprocessor runqueue, tick and idle done in Phase 1 (**selftest-sched**); per-vCPU indexing and IPIs pending
- [ ] SMP bring-up (PSCI `CPU_ON` / INIT-SIPI-SIPI)
- [x] `futex` WAIT/WAKE (+ `_PRIVATE`, timeouts) — done in Phase 1 (**hello-strace**, **selftest-sched** for timeouts)
- [ ] Signals: per-thread/process pending + masks, Linux-exact `rt_sigframe`, `rt_sigreturn`, `sigaltstack`, `SIG_DFL` termination (128+signo), fault → `SIGSEGV`/`SIGBUS`/`SIGFPE`/`SIGILL`/`SIGTRAP`
- [ ] `tgkill`/`kill` (async preemption via `SIGURG`)
- [~] Timers: `nanosleep` (10 ms resolution, Phase 1), `setitimer`, `timer_create`/`settime`/`delete`, per-thread CPU time (`SIGPROF`)
- [~] Remaining Tier-1 syscalls (§7): `pipe2 setitimer timer_create/settime/delete epoll_create1 epoll_ctl epoll_pwait eventfd2` still `ENOSYS` (the rest landed in Phase 1)
- [~] Interrupt controller (GICv3 / LAPIC+IOAPIC) and timers (generic timer / TSC, pvclock): GICv3 (+v2 fallback) and LAPIC timer + TSC done in Phase 1 (**selftest-timer**, **gic-variants**); IOAPIC, SGIs/IPIs, pvclock pending
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
