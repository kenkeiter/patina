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
scripts/run-qemu.sh <arch> -- patina.selftest=panic|fault|alloc|user|userfault|timer|switch|vm|sched|kstack|smp
scripts/test-hello.sh [aarch64] [x86_64]            # build hello, pack initrd, run, check output + exit 0/3
scripts/test-hello.sh --app <name> [arch...]        # run testapps/<name>: expect 'PASS <name>' + exit 0
scripts/test-hello.sh --threads <arch>              # run testapps/threads and only show its output
scripts/test-signals.sh [aarch64] [x86_64]          # 6 signal cases per arch (signals, sigstress)
scripts/test-smp.sh [--app <name>] [--cpus "1 2 4"] [--repeat N] [arch...]
                                                    # app at each vCPU count (default smpstress; smpstress/threads get -cpus N)
scripts/test-runtime.sh [aarch64] [x86_64]          # Phase 2 umbrella: hello, threads, smpstress, fdtest, signals, pproftest per CPU count
scripts/test-hello.sh --app fileio [arch...]        # Phase 3 VFS/devfs/procfs/FIFO/os.Root coverage (48 checks; 50 on x86_64)
scripts/test-ctl.sh [aarch64] [x86_64]              # Phase 3 control channel over the patina.ctl port: 8 cases per arch
scripts/test-os.sh [aarch64] [x86_64]               # Go's own `os` package tests under patina with testapps/skiplist/os.txt
scripts/test-hello.sh --app sockets|unixsock [arch...] # Phase 4 AF_INET/AF_INET6 and AF_UNIX socket coverage over loopback
scripts/test-net.sh [aarch64] [x86_64]              # Phase 4 Milestone A on QEMU: httphello via patina run --net user --publish, DHCP, PCI
scripts/test-gonet.sh [--pkg net/http] [arch...]    # Go's own `net` (or net/http) package tests with testapps/skiplist/net.txt
scripts/test-gopkg.sh <pkg>... [--arch aarch64|x86_64|both] [--cpus N] [--run RE] [--no-short] [--mem MiB]
                                                    # Milestone B: any Go std package's tests with testapps/skiplist/<pkg>.txt (D74); test-os.sh/test-gonet.sh wrap it
                                                    # x86_64 (TCG) is slow: raise PATINA_TIMEOUT / GOPKG_TEST_TIMEOUT when the host is busy
scripts/bench.sh [arch...] [--cpus 1,2] [--runs 3] [--novdso] [-n F] [--only L]
                                                    # Phase 5 per-syscall benchmarks (testapps/bench): table of medians per arch (D79)
scripts/test-robust.sh [aarch64] [x86_64]           # Phase 5 robustness (testapps/robust): tickless idle, sub-tick timers, fault -> signal,
                                                    # stack/page-table reclaim, OOM, signal/fd edge cases; 12 cases per arch (D77, D78, D80)
scripts/test-sysfuzz.sh [aarch64] [x86_64]          # Phase 5 syscall validation (testapps/sysfuzz): 327/343 Linux-errno cases, seeded
                                                    # random-argument fuzz at 1 and 4 vCPUs, bad rt_sigreturn frames; 17/20 cases per arch (D86, D87)
scripts/run-qemu.sh <arch> <cpio> -- patina.stats=1 -- <args>
                                                    # per-CPU timer IRQs / ticks / idle waits / switches + memory line at exit
GO=go1.26.9 scripts/build-testapp.sh <name> <arch> <out>  # build a testapp with another Go toolchain (default GO=go)
PATINA_NET=user scripts/run-qemu.sh <arch> -- patina.selftest=net   # virtio-net driver self-test (ARP against slirp)
cargo test --workspace --exclude patina-kernel      # host tests
scripts/test-vz.sh                                  # Milestone A on Apple VZ (patina run --vmm vz): hello, ctl, stop modes, NAT serve, Phase 2-4 subset
PATINA_VMM=vz <any of the above aarch64 runs>       # the same kernel + initrd under Apple VZ via scripts/run-vz.sh (D84)
cli/scripts/vztest.sh [--e2e]                       # in-process VZ driver tests; --e2e = TestVZE2E (hello, ctl, --net user HTTP) + TestVZVsock
cli/scripts/vztest.sh -test.run TestVZVsock -test.v # AF_VSOCK on VZ (D76): testapps/vsockapp, host dials the guest listener, 1 MiB echoes
scripts/test-hello.sh --app vsockapp [arch...]      # AF_VSOCK without a device (QEMU): EAFNOSUPPORT, /dev/vsock ENODEV
```

Environment for `run-qemu.sh` (and `smoke.sh`): `PATINA_MEM` (MiB, default
512), `PATINA_CPUS` (default 1), `PATINA_ACCEL` (`hvf`/`kvm`/`tcg`),
`PATINA_TIMEOUT` (seconds, default 60), `PATINA_KERNEL`, `PATINA_LOG`,
`PATINA_QEMU_EXTRA`, `PATINA_MACHINE` (x86_64: `microvm` default, `pc`,
`q35`), `PATINA_CTL_SOCK` (path of the Unix socket behind the `patina.ctl`
virtio-console port; `run-qemu.sh` always adds the virtio-serial device, the
port and a `virtio-rng-device`, like the CLI), `PATINA_CTL_WAIT=1` (QEMU
`wait=on`: boot only once a client is attached), `PATINA_CONSOLE=hvc0`
(console on virtio-console port 0 instead of the UART), `PATINA_VIRTIO`
(`1` virtio-mmio, `pci` the same devices as virtio-pci — default on x86 `pc`/`q35`, `0` none; `PATINA_VIRTIO_PCI_OPTS` e.g. `,disable-legacy=on`), `PATINA_NET=user[:H-G,...]` (a slirp virtio-net NIC, optionally forwarding host 127.0.0.1:H to guest port G). `test-hello.sh --app` passes `PATINA_PARAMS` as extra kernel
params (e.g. `patina.strace=1`). `test-runtime.sh` runs every suite at each
CPU count in `PATINA_CPUS_LIST` (default `1 2 4`) and prints one PASS/FAIL
row per case (logs in `out/test/runtime/`); `test-smp.sh` logs to
`out/test/`. `test-smp.sh` defaults `PATINA_TIMEOUT` to 180 s,
`test-runtime.sh` to 120 s. `build-kernel.sh` (and the test scripts)
honor `PATINA_PROFILE=dev|release` (default release). `run-qemu.sh` exits with the code from the
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
- [x] SMP: secondary CPUs started with PSCI `CPU_ON` in Phase 2 — **selftest-smp-a64** (see Phase 2)

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
- [x] SMP: APs started with INIT-SIPI-SIPI in Phase 2 — **selftest-smp-x86** (see Phase 2)

### Scripts and docs

- [x] `scripts/build-kernel.sh` — **build-a64**, **dev-profile**
- [x] `scripts/run-qemu.sh`: cmdline per `doc/cli-contract.md`, initrd, params/args split, exit status from the sentinel, hard timeout — **boot-hvf**, **initrd-args**, **timeout**
- [x] `scripts/smoke.sh` — **smoke-a64**
- [x] `doc/status.md`, `doc/decisions.md`, `doc/crates.md` audit notes for every kernel dependency

## Per-target matrix

`V` verified (named test) · `U` untested (code written to the spec, never
booted) · `—` not applicable.

| Target | boot | console | poweroff | initrd | SMP bring-up (Phase 2) | virtio + control channel (Phase 3) |
|---|---|---|---|---|---|---|
| QEMU `virt` aarch64, HVF | V **boot-hvf** | V PL011; V `hvc0` (**hvc0**) | V PSCI HVC | V **initrd-args** | V **selftest-smp-a64** (1/2/4/8 CPUs, GICv3, PSCI HVC); scheduler: **test-smp**, **threads-smp**, **fdtest-smp**, **test-signals-smp**, **pproftest**, **test-runtime** (1/2/4 CPUs) | V DTB discovery, GIC SPIs, rng, console port `patina.ctl` — **virtio-discovery**, **test-ctl** |
| QEMU `virt` aarch64, TCG | V **boot-tcg**, **el2-drop** | V PL011 | V PSCI HVC / SMC | V (same DTB path; **initrd-args** run on HVF) | V **selftest-smp-a64** (GICv3 1/2/4, GICv2, EL2 entry + PSCI SMC); scheduler: **test-smp** (`PATINA_ACCEL=tcg`, 4 CPUs) | V modern (v2) MMIO with `force-legacy=false` (**virtio-discovery**) |
| QEMU `microvm` x86_64, TCG | V **boot-x86** | V 16550 port 0x3f8; V `hvc0` (**hvc0**) | V isa-debug-exit; V ACPI S5 sleep-control register (**poweroff-order**) | V **initrd-args-x86** | V **selftest-smp-x86** (1/2/4/8 CPUs, MADT via BIOS-area RSDP scan); scheduler: **test-smp**, **threads-smp**, **fdtest-smp**, **test-signals-smp**, **pproftest**, **test-runtime** (1/2/4 CPUs), **tlbstress** (D52 shootdown) | V DSDT discovery (ACPI on) and cmdline entries (`acpi=off`), I/O APIC, rng, `patina.ctl` — **virtio-discovery**, **test-ctl** |
| QEMU `pc` / `q35` x86_64, TCG | V **machines-x86** | V 16550 | V isa-debug-exit; V ACPI S5 via PM1a (**poweroff-order**, pc) | V **machines-x86** | V **selftest-smp-x86** (MADT) | V virtio-pci (D70): q35 ECAM from the MCFG, pc port 0xCF8, MSI-X via the LAPIC; rng, `patina.ctl` — **virtio-pci-x86** |
| Firecracker aarch64 | U (no KVM on this host) | U 16550 MMIO | U PSCI | U | U | U DTB discovery, vsock 4242 |
| Firecracker x86_64 | U | U | U i8042 reset (last in the D64 order) | U | U (MP table / MADT) | U `virtio_mmio.device=` cmdline entries, DSDT, vsock |
| Cloud Hypervisor aarch64 | U | U PL011 | U PSCI | U | U | U virtio-pci (D70): DTB ECAM bus 0, MSI-X only via the GICv3 ITS (KVM, 64 KiB ITS pages) or GICv2m (MSHV) |
| Cloud Hypervisor x86_64 | U | U | U ACPI S5 at port 0x600 (D64) | U | U | U virtio-pci (D70): MCFG ECAM 0xe8000000 bus 0, MSI-X, 64-bit BARs above RAM |
| Apple VZ aarch64 (`patina run --vmm vz`, macOS 26) | V **vz-hello** (RAM base 0x7000_0000, unaligned initrd, DTB) | V `hvc0` = virtio-console port 0 over virtio-pci INTx, early ring replayed (**vz-hello**, **test-vz**; D83 sacrificial buffer) | V PSCI HVC (**vz-hello**) | V **vz-hello** | V **vz-runtime** (1/2/4 vCPUs), `selftest=smp` (**vz-timing**) | V ECAM DTB discovery, BAR assignment, INTx auto-selected (no ITS/v2m), rng, **vsock 4242** control channel (**test-vz**, **vz-ctl**), AF_VSOCK program sockets on the same device (**vz-vsock**, D76), virtio-net-pci + VZ NAT DHCP (**test-vz**, **vz-e2e**) |

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
| **threads-app** | `scripts/test-hello.sh --threads aarch64` (also x86_64) | `threads: GOMAXPROCS=1 NumCPU=1`, 1000 goroutines + GC workers run across several threads, then `[patina] ENOSYS 20 (epoll_create1)`, `runtime: epollcreate failed with 38`, `fatal error: runtime: netpollinit failed`, exit 2 (needed epoll/eventfd2/pipe2 and `SIGURG` delivery). Superseded by **threads-p2** (Phase 2), which passes |
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
- [x] Signal state: `rt_sigaction` for 1..64 (KILL/STOP rejected), per-thread `rt_sigprocmask`, `sigaltstack` stored and validated — **hello-strace** (114 `rt_sigaction` calls, `sigaltstack`, `rt_sigprocmask`); delivery, `rt_sigreturn` and `kill`/`tgkill` now work and are verified in Phase 2 (**test-signals**)
- [x] `exit_group` / last thread exit → `[patina] exit code N` and poweroff — **test-hello** (exit 0 and 3)
- [x] First Go binary (`testapps/hello`) prints and exits with the right code on both arches — **test-hello**, **test-hello-tcg**
- [x] `AT_RANDOM`/`getrandom` from a ChaCha20 DRBG seeded by virtio-rng (D60, replaces D35) — Phase 3 **rng-seed**
- [x] `patina run` end-to-end through the CLI — Phase 3 **test-cli**

## Phase 2 — Runtime

Test names (all at `PATINA_CPUS=1` unless noted; aarch64 = QEMU `virt`
HVF, x86_64 = QEMU `microvm` TCG, release profile unless noted). Decisions:
D42–D52 (D50 SMP scheduler core, D51 timers and CPU clocks, D52 x86_64
TLB shootdown).

| Name | Command | Observed |
|---|---|---|
| **test-hello-p2** | `scripts/test-hello.sh` | `PASS aarch64`, `PASS x86_64`; also with `PATINA_CPUS=2` (run before D50, when the second CPU stayed parked outside `patina.selftest=smp`; SMP runs are **test-smp** and **test-runtime**) |
| **fdtest** | `scripts/test-hello.sh --app fdtest` | `PASS fdtest aarch64`, `PASS fdtest x86_64`; the 19 `ok` lines: `ok os.Pipe round trip + EOF`, `ok EPIPE`, `ok 70000-byte transfer`, `ok non-blocking pipe2 (EAGAIN, capacity 64 KiB)`, `ok blocking raw read`, `ok fstat pipe S_IFIFO`, `ok fcntl F_GETFL/F_SETFL shared by dup`, `ok dup/dup3/F_DUPFD_CLOEXEC/close EBADF`, `ok EMFILE after N dups (soft limit was M)`, `ok eventfd counter/semaphore/blocking`, `ok epoll level-triggered, EEXIST/ENOENT/EINVAL`, `ok epoll edge-triggered`, `ok epoll oneshot + MOD re-arm`, `ok epoll timeout`, `ok epoll blocking wait`, `ok epoll HUP/ERR/auto-removal`, `ok epoll + eventfd`, `ok ppoll`, `ok time.Sleep/After/Ticker`; also passes with `PATINA_PROFILE=dev` on aarch64; with `PATINA_PARAMS=patina.strace=1`: `pipe2(…, 0x80000) = 0`, `epoll_create1(0x80000) = 5`, `eventfd2(0x0, 0x80800) = 6`, `epoll_ctl(0x5, 0x1, 0x6, …) = 0` |
| **threads-p2** | `scripts/test-hello.sh --app threads` | `PASS threads` on both arches: `threads: GOMAXPROCS=1 NumCPU=1`, `threads: 1000 goroutines ok, NumGC=56 HeapAlloc=509KiB`, the sleep/ticker checks, and the async-preemption starvation case passes |
| **test-signals** | `scripts/test-signals.sh aarch64 x86_64` | 12/12 PASS. `signals -self`: `signals: got terminated`, `PASS signals (handler)`, exit 0. `signals -mode nohandler -self`: `[patina] exit signal SIGTERM`, `[patina] exit code 143`. `sigstress`: `preemption: 1 spinners, main ran 73 turns in 2s`, `nil deref recovered: runtime error: invalid memory address or nil pointer dereference`, `divide by zero recovered: runtime error: integer divide by zero`, `kill: 1000/1000 SIGUSR1 received`, `tgkill: 100/100`, `nanosleep interrupted after 81ms, rem 4.92s`, `blocking pipe read restarted across signals`, `20 x time.Sleep(10ms) under SIGUSR1 storm: 201ms`, `PASS sigstress`. `sigstress -procs 2` passes. `sigstress -crash`: `[signal SIGSEGV: segmentation violation code=0x1 addr=0x0 pc=0x4ab040]`, exit 2. `sigstress -breakpoint`: `SIGTRAP: trace trap`, exit 2. Also 12/12 with `PATINA_PROFILE=dev`. Under `patina.strace=1`: `read(…) = -errno 512` ×7 then success, `nanosleep(…) = -errno 516`, `futex(…) = -errno 512`, 1528 `rt_sigreturn` calls |
| **selftest-smp-a64** | `PATINA_CPUS=N scripts/run-qemu.sh aarch64 -- patina.selftest=smp` | HVF, N = 1/2/4/8. N=4: `[patina] smp: 4 cpus (psci hvc)`, `smp: cpu 1 online (mpidr 0x1, entered at EL1)` …, `selftest smp: 3 secondaries started in 279 us`, `timer irqs cpu0:+3 cpu1:+3 cpu2:+3 cpu3:+3`, `deadline +2 ms fired after 2003 us (1 timer irq)`, `cancelled deadline: 0 timer irqs in 5 ms`, `past deadline fired after 3 us`, `selftest smp: ok (4 cpus, 15 ipis, deadline 2003 us)`. TCG `gic-version=3`, N = 1/2/4: ok (deadline 2164–2710 us). TCG `-M virt,virtualization=on`: ok (APs enter at EL2, PSCI SMC). TCG `gic-version=2`: ok (`GICD_SGIR` path) |
| **selftest-smp-x86** | `PATINA_CPUS=N scripts/run-qemu.sh x86_64 -- patina.selftest=smp` | N=2: `[patina] smp: 2 cpus (acpi madt @ 0x1fffff5d, rsdp from bios-area scan @ 0xf5560), boot cpu apic id 0`, `smp: cpu 1 online (apic id 1)`, `selftest smp: reschedule ipis 1 in 105 us, deadline irq 663 us late, ap ticks [5]`, `selftest smp: ok (2 cpus, 2 ipis, deadline 2737 us)`. N=4: `ok (4 cpus, 6 ipis, deadline 2549 us)`. N=8 and N=1: ok. `PATINA_MACHINE=pc` (MADT @ 0x1ffe2275) and `q35` (MADT @ 0x1ffe2213): ok. `PATINA_QEMU_EXTRA="-cpu max,-fsgsbase,-xsave,-smep,-smap"`: ok. Forced sources via `patina.smp.skip`: 2 → fw_cfg `etc/acpi/tables`; 3 (with `-smp 2,sockets=2`) → `mp table @ 0xf56f0`; 4 → `fw_cfg nb_cpus` |
| **test-smp** | `scripts/test-smp.sh --cpus "2 4" aarch64 x86_64` (smpstress) | aarch64, 4 CPUs: `[patina] smp: 4 cpus online`, `smpstress: GOMAXPROCS=4 NumCPU=4`, `smpstress: ping-pong 1088286 bounces, 4874240 spins in 2s`, `mutex 16 goroutines x 20000 ok in 98ms`, `sleeps: 8x1250 x Sleep(100us) 1.261s; 200 x Sleep(500us) 265ms`, `gc under load ok, NumGC 0 -> 40`, `parallel tids 4,1: 100/100 buckets each 100/100, both 100/100`, `PASS smpstress`. PASS at 2 and 4 CPUs on aarch64 and at 4 on x86_64, each run ending with the exit sentinel after the other CPUs stopped. **x86_64 at 2 CPUs failed once**: `#GP general protection (vector 13) in kernel mode on cpu 1` inside `frame::alloc_frames` (a free-list link read as garbage: a freed frame written through a stale TLB entry, the missing x86_64 TLB shootdown, D52). Also PASS with `PATINA_PROFILE=dev` at 4 CPUs on both arches and with `PATINA_ACCEL=tcg` on aarch64 at 4 CPUs (`200 x Sleep(500us) 289ms`) |
| **threads-smp** | `scripts/test-smp.sh --app threads --cpus "2 4" aarch64 x86_64` | `threads: GOMAXPROCS=4 NumCPU=4`, `threads: 1000 goroutines ok, NumGC=68 HeapAlloc=601KiB`, `PASS threads` at 2 and 4 CPUs on both arches |
| **fdtest-smp** | `scripts/test-smp.sh --app fdtest --cpus 4 --repeat 3 aarch64 x86_64` | `PASS fdtest` ×3 on both arches |
| **test-signals-smp** | `PATINA_CPUS=2` and `PATINA_CPUS=4 scripts/test-signals.sh aarch64 x86_64` | all 6 cases PASS at 2 and 4 CPUs on both arches; `sigstress` repeated 6× at 4 and at 2 CPUs on aarch64 and 4× at 4 CPUs on x86_64 |
| **pproftest** | `scripts/test-smp.sh --app pproftest --cpus "1 2 4" <arch>` | `PASS pproftest` everywhere. pprof samples / spinning thread's CPU clock / sleeping thread's CPU clock over 200 ms: aarch64 1 CPU 100 / +199.85 ms / +3.6 µs, 2 CPUs 198 / +199.97 ms / +5.3 µs, 4 CPUs 200 / +199.92 ms / +11.7 µs; x86_64 94 / +198.88 ms / +87 µs, 196 / +199.86 ms / +82 µs, 198 / +199.60 ms / +104 µs. The 5 ms `SIGEV_THREAD_ID` timer delivered 10–17 SIGUSR1 per 100 ms; SIGALRM from a 20 ms `ITIMER_REAL` arrived after 21–31 ms. `PATINA_PROFILE=dev` at 2 CPUs: 198 / 200 samples. Under `patina.strace=1`: Go's per-M `timer_create(0x3, …) = 0` (`CLOCK_THREAD_CPUTIME_ID`), `timer_settime`, `setitimer(0x2, …)` (`ITIMER_PROF`), `timer_delete` |
| **test-runtime** | `PATINA_CPUS_LIST="1 2 4" scripts/test-runtime.sh aarch64 x86_64` | 66/66 rows PASS on main 09c9368 (after the D52 TLB fix): hello, threads, smpstress, fdtest, signals handler / nohandler, sigstress, sigstress `-procs 2`, `-crash`, `-breakpoint`, pproftest × 1/2/4 CPUs × 2 arches; `test-runtime: all passed`. An earlier run (before D52) also passed 66/66 |
| **tlbstress** | `scripts/test-smp.sh --app tlbstress --cpus "2 4" --repeat 2 x86_64 aarch64` | `PASS tlbstress` 8/8 (x86_64 and aarch64, 2 and 4 CPUs, 2 runs each). Before the D52 fix the same program crashed the x86_64 kernel 5/5 at 2 or 4 CPUs within ~1 s (`#GP` in `frame::alloc_frames`, a free-list link read back as the racer's marker: a stale-TLB write after free); the shootdown worker also ran it 9/9 per arch at 1/2/4 CPUs × 3 |
| **sleep-latency** | the `smpstress` sleep phase (**test-smp**), before and after D50's one-shot deadlines | aarch64/HVF: 200 × `time.Sleep(500µs)` 2.001 s → 248 ms (1, 2 and 4 CPUs; x86_64/TCG 263–268 ms); 10 000 sleeps from 8 goroutines 12.5 s → ~1.5 s; the ~1.2 ms per-sleep floor is Go's netpoller (`epoll_pwait` with whole-millisecond timeouts) |

Runtime and scheduling:

- [x] `clone` (thread flag set only, else `EINVAL`), kernel stacks with guard pages, TLS (`FS_BASE` / `TPIDR_EL0`) — done in Phase 1 (**test-hello**, **selftest-kstack**)
- [x] `futex` WAIT/WAKE (+ `_PRIVATE`, timeouts) — Phase 1 (**hello-strace**, **selftest-sched**); now interruptible (`ERESTARTSYS` without a timeout, `ERESTART_RESTARTBLOCK` with one; D45) — **test-signals** (strace `futex = -errno 512`)
- [x] Goroutines, GC, async preemption and pprof working — **threads-p2** (1 vCPU: 1000 goroutines, 56 GCs, starvation case), **threads-smp** (2 and 4 vCPUs), **test-signals** / **test-signals-smp** (`preemption: 1 spinners, main ran 73 turns in 2s`; `-procs 2`), **pproftest**
- [x] SMP bring-up under the scheduler: `run_smp` starts the APs and every CPU enters the idle loop (`[patina] smp: N cpus online`) — **test-smp**, **test-runtime** (D50)
- [x] Per-vCPU runqueues, wake-ups placed on idle CPUs with a `Reschedule` IPI, idle CPUs in `WFI`/`HLT`, work stealing, the `on_cpu` switch protocol — **test-smp** (ping-pong between threads, contended mutex = futex wake-ups across CPUs, `parallel tids 4,1: … both 100/100`), **threads-smp**, **fdtest-smp**, **test-signals-smp** (signal kicks via `kick_remote`)
- [x] `exit_group` / exit / panic stop the other CPUs (`Stop` IPI, `STOPPED` mask, D50) — **test-smp**, **test-runtime** (every SMP run ends with the exit sentinel and a clean exit); Stop while another CPU is mid-print or stuck with IRQs masked is reasoned only
- [x] `sched_getaffinity` reports all vCPUs → `GOMAXPROCS` = `NumCPU` = vCPUs — **test-smp**, **threads-smp** (`GOMAXPROCS=4 NumCPU=4`)
- [x] Per-thread CPU time (`Thread::cpu_time_ns`, `CLOCK_THREAD_CPUTIME_ID`, the process CPU clock) — **pproftest** (spinning thread +198.9–200.0 ms, sleeping thread +3.6–104 µs over 200 ms)
- [x] x86_64 SMP memory safety: TLB shootdown IPIs with deferred frame reclaim (D52, `mm/tlb.rs`) — **tlbstress** (reproduced the stale-TLB free-list corruption before the fix, 8/8 after), **test-runtime**; aarch64 user-page invalidation broadcasts (`tlbi vale1is`) and needs no shootdown. Limits: a stopped CPU never advances its generation (pending batches stay pending after exit), remote flushes complete asynchronously (mprotect/munmap return before other CPUs flush)
- [x] More than 4 vCPUs under the scheduler: Go's `runtime` tests at 8 vCPUs, 0 failed (aarch64 3 identical runs, x86_64 one run; also `sync`, `os/signal`, `runtime/pprof` at 8 on aarch64) — **test-gopkg-8cpu** (Phase 5); also **robust-8cpu** (`test-smp.sh --cpus 8` smpstress / threads / sigstress on both arches, `test-signals.sh aarch64` and `test-robust.sh aarch64` at `PATINA_CPUS=8`)
- [x] Tickless idle: idle CPUs stop the periodic tick and arm only the deadlines they own (D77) — **test-robust** `idle` (0–42 timer IRQs per CPU over a ~7 s mostly idle run at 4 and 8 vCPUs, ~700 with a 100 Hz tick)

Signals (D42–D45):

- [x] Per-thread and process-wide pending sets, per-thread mask, coalescing standard signals — **test-signals** (`kill: 1000/1000`, `tgkill: 100/100`)
- [x] `kill` (process) and `tgkill`/`tkill` (thread); Go's `SIGURG` async preemption — **test-signals**, **threads-p2**
- [x] Linux `rt_sigframe` + `rt_sigreturn` on x86_64 (XSAVE fpstate) and aarch64 (`fpsimd_context`, kernel-mapped `rt_sigreturn` trampoline since Go sets no `SA_RESTORER`) — **test-signals** (1528 `rt_sigreturn` round trips, Go reading `siginfo`/`ucontext` for its crash line)
- [x] Go's `SA_ONSTACK` handlers on its per-M `sigaltstack` — **test-signals**
- [~] `sigaltstack` edge cases: `ENOMEM` below `MINSIGSTKSZ`, `EINVAL` for bad flags, `SS_AUTODISARM` accepted and read back — **test-robust** `default`; `EPERM` while on the stack and the auto-disarm on delivery need a handler of our own (not reachable from Go): untested
- [x] `SIG_DFL` termination: `[patina] exit signal NAME`, exit 128+signo — **test-signals** (`SIGTERM`, 143)
- [x] `SIGKILL` (always terminates, 137) — **test-robust** `sigkill` (`kill(getpid(), SIGKILL)` → `[patina] exit signal SIGKILL`, 137) and `oomfault`
- [x] Fault → `SIGSEGV` `SEGV_MAPERR` (nil dereference, both arches) — **test-signals** (recovered ×200; `-crash`: `code=0x1 addr=0x0`, exit 2)
- [x] x86 `#DE` → `SIGFPE` `FPE_INTDIV` — **test-signals** (x86_64; arm64 does not trap, Go checks explicitly)
- [x] `BRK` / `int3` → `SIGTRAP` — **test-signals** (`-breakpoint`: `SIGTRAP: trace trap`, exit 2 on both arches; D43)
- [x] `SIGBUS` `BUS_ADRALN` (aarch64 unaligned `LDAXR` crossing a 16-byte granule: `[signal SIGBUS: bus error code=0x1 addr=<the address> pc=…]`; HVF tolerates unaligned exclusives inside a granule), `SIGILL` (`UDF #0` → `sigcode=1`, `UD2` → `sigcode=2`), x86 `#GP` → `SIGSEGV` `SI_KERNEL` (non-canonical load: `code=0x80 addr=0x0`), `SEGV_ACCERR` (store to `PROT_READ`: `code=0x2`, `addr` = the address) — **test-robust** `sigbus`, `sigill`, `gp`, `accerr` (decoding matches arm64 `fault_info` / x86 `traps.c`)
- [~] `FPE_FLTINV`: QEMU TCG raises no `#XM` for unmasked SSE exceptions (robust prints `skip`), arm64 FP traps are not implemented by the hardware here — unverifiable on this host
- [x] Syscall restart: `ERESTARTSYS` under `SA_RESTART` (blocking pipe read) — **test-signals** (strace `read = -errno 512` ×7, then success)
- [x] `nanosleep` → `EINTR` with `rem` when a handler runs (`ERESTART_RESTARTBLOCK`) — **test-signals** (`nanosleep interrupted after 81ms, rem 4.92s`)
- [x] `restart_syscall` continuation when no handler runs — **test-robust** `restart` (a 200 ms `nanosleep` woken by SIGUSR1 whose disposition became `SIG_IGN` before delivery: strace `nanosleep = -errno 516`, `restart_syscall = 0`, 205–208 ms in total; an ignored SIGUSR1 is dropped at send, as Linux)
- [x] `rt_sigsuspend` (EINTR after the handler), x86 `pause`, `rt_sigpending` (a blocked timer signal shows up) — **test-robust** `default`
- [x] `epoll_pwait` `EINTR` (not restarted under Go's `SA_RESTART`) and its temporary sigmask (a blocked SIGUSR2 waits out the timeout, delivered afterwards) — **test-robust** `default`; the `ppoll` sigmask path shares `set_temp_mask` but is not exercised
- [x] `SIGPIPE` to the writer on `EPIPE` (pipe on fd ≥ 3, `signal.Notify(SIGPIPE)` receives it) and `SIG_DFL` SIGPIPE → 141 — **test-robust** `default`, `sigpipe`
- [x] x86_64 FXSAVE-only frames — **fxsave-only**: `test-signals.sh x86_64` and `test-smp.sh --app smpstress --cpus 2 x86_64` with `PATINA_QEMU_EXTRA="-cpu max,-xsave"` and with `-cpu Nehalem` (no XSAVE/AVX/FSGSBASE/SMEP/SMAP: `fp state fxsave (512 bytes)`), robust `default`/`gp`/`sigill` on Nehalem; nothing needed fixing
- [ ] `rt_sigtimedwait`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo` — `ENOSYS` by design (D42)

File descriptors (D46; design §8 had these in Phase 3, the netpoller needs them now):

- [x] fd table (lowest free, `RLIMIT_NOFILE` soft limit), `dup`/`dup3`/`F_DUPFD_CLOEXEC`, `F_GETFL`/`F_SETFL` shared by duplicates, `EMFILE` — **fdtest**
- [x] `pipe2` (64 KiB, `PIPE_BUF` atomicity, `EAGAIN`, EOF, `EPIPE`), blocking and non-blocking — **fdtest**
- [x] `eventfd2` (counter, `EFD_SEMAPHORE`, blocking) — **fdtest**
- [x] epoll: `epoll_create1`/`epoll_ctl`/`epoll_pwait`, LT, ET, `ONESHOT` + `MOD`, timeouts, HUP/ERR, auto-removal on close; Go's netpoller — **fdtest**, **threads-p2**
- [x] `ppoll` — **fdtest**
- [x] `fstat` on pipes (`S_IFIFO`) — **fdtest**
- [x] `readv`/`writev`, `pread64`/`pwrite64`/`lseek` (`ESPIPE` on pipes; offsets on a file), `epoll_pwait2`, `FIONBIO`/`FIONREAD`/`FIOCLEX`/`FIONCLEX` — **test-robust** `default`
- [x] x86_64-only entry points `poll`, `epoll_wait`, `epoll_create` (+ `EINVAL` for 0), `dup2` (incl. onto itself), `pipe`, `eventfd` — **test-robust** `default` (x86_64)

SMP and timers (arch layer, D47/D48):

- [x] aarch64 SMP bring-up via PSCI `CPU_ON` (HVC and SMC, EL1 and EL2 entry), up to 8 CPUs — **selftest-smp-a64**
- [x] x86_64 CPU enumeration (MADT via BIOS-area RSDP, fw_cfg ACPI tables, MP table, fw_cfg `NB_CPUS`) and INIT-SIPI-SIPI bring-up, up to 8 CPUs tested — **selftest-smp-x86**
- [x] IPIs (Reschedule, Stop): GICv3 SGIs, GICv2 SGIs, xAPIC fixed IPIs — **selftest-smp-a64**, **selftest-smp-x86**
- [x] Per-CPU periodic tick on every CPU and per-CPU one-shot `set_deadline` (fires ≥ deadline, cancel, past deadline) — **selftest-smp-a64**, **selftest-smp-x86**
- [x] Sub-tick deadlines: the sleep list arms the per-CPU one-shot timer (`sched::arm_deadline`); sleeps and timeouts are never early (D50) — **sleep-latency** (200 × `Sleep(500µs)` 2.001 s → 248 ms), **test-smp**
- [x] `setitimer`/`getitimer`, `timer_create`/`settime`/`gettime`/`getoverrun`/`delete` (`SIGEV_THREAD_ID`, `SI_TIMER` / `SI_KERNEL`), CPU-clock timers (D51) — **pproftest** (SIGALRM after 21–31 ms, 10–17 SIGUSR1 per 100 ms from a 5 ms timer, Go's per-M `timer_create(0x3, …)` in strace)
- [x] pprof CPU profiles (Go's per-M `CLOCK_THREAD_CPUTIME_ID` SIGPROF timers and `ITIMER_PROF`) — **pproftest** (94–200 samples per run at 100 Hz)
- [x] `alarm` (x86_64; Linux rounding), `getrusage` (self and thread grow by the spin), `times`, `clock_nanosleep` `TIMER_ABSTIME` on `MONOTONIC` and `REALTIME` (never early, sub-tick on average), on the thread CPU clock (`EINVAL`), `ITIMER_VIRTUAL`, sub-tick `timer_settime`/`setitimer(ITIMER_REAL)` (D77), overrun accounting (fixed in D77: it read ≤ 1) — **test-robust** `default`; the process-CPU-clock `ENOTSUP` is untested
- [x] `getrandom`/`AT_RANDOM` seeded from `cpu::hw_random` (RNDR / RDSEED / RDRAND), logged as `[patina] random: seeded from …` (D50) — every **test-smp** log shows the line; HVF on this host has no RNDR, only TCG `-cpu max` does. Since Phase 3 the generator is a ChaCha20 DRBG seeded from virtio-rng as well (D60, **rng-seed**)
- [~] x2APIC IPIs, APIC ids > 254, > 8 CPUs, PSCI SMC on real firmware — implemented, untested
- [~] Firecracker x86_64 + aarch64, KVM / Cloud Hypervisor: SMP and scheduler code written to the spec, never booted (needs a KVM host)

## Phase 3 — OS

Test names (aarch64 = QEMU `virt` HVF, x86_64 = QEMU `microvm` TCG with
ACPI on, release profile, `PATINA_CPUS=1` unless noted). Decisions:
D53–D57 (VFS, devfs/procfs, FIFOs, path conventions, Tier-3 stubs),
D58–D64 (in-tree virtio, discovery and IRQ routing, RNG, console port and
hvc0, vsock, control channel, x86_64 poweroff order), D65 (os test run),
D66 (`patina run`).

| Name | Command | Observed |
|---|---|---|
| **fileio** | `scripts/test-hello.sh --app fileio aarch64 x86_64` (also with `PATINA_CPUS=4`) | `PASS fileio aarch64`, `PASS fileio x86_64`; 48 `ok` lines per arch: write/read/append+seek/stat/rename/truncate/mkdir+readdir/symlink/large file (1 MiB)/not exist/O_EXCL/O_TRUNC/EISDIR/ENOTEMPTY/ENOTDIR/rename over + across dirs/link + nlink/chmod/chown/chtimes/`os.Executable()==/app`/getwd+chdir+relative open/ReadDir of `/ /dev /proc /etc`/`/etc/hosts`/`/proc/self/maps`/`/proc/cpuinfo`/`/dev/null` `/dev/zero` `/dev/urandom`/FIFO (`Mkfifo` + goroutine)/umask/offset shared by dup/O_APPEND from two fds/seek past EOF hole/truncate grow/CreateTemp/RemoveAll/WalkDir/`os.Root` (openat2 → ENOSYS fallback)/Fstat/Flock/io.Copy file→file (copy_file_range → ENOSYS fallback)/Statfs; no `ENOSYS`/`fatal` lines |
| **go-startup-reads** | `PATINA_PARAMS=patina.strace=1 scripts/test-hello.sh --app fileio aarch64` | `openat path "/sys/kernel/mm/transparent_hugepage/hpage_pmd_size"` → fd 3, `read(0x3, …) = 8`; `openat path "/proc/self/cgroup"` → `-ENOENT` (deliberate: Go assumes no cgroup CPU limit) |
| **loader-errors-p3** | the four Phase 1 **loader-errors** cases re-run through the VFS | 127 `/app not found in the initrd`; 126 `/app: not an ELF file (bad magic)`; 126 wrong-arch message; 127 `initrd is not a valid newc cpio archive (cpio: BadMagic at offset 0)` |
| **virtio-discovery** | boot logs of any run (`[patina] virtio:` lines) | aarch64 `virt`: `mmio @ 0xa003c00 irq 78 device-id 4 (rng) [dtb, virtio-mmio v1 (legacy), …, edge irq]`, `mmio @ 0xa003e00 irq 79 device-id 3 (console)`; x86_64 `microvm` ACPI on: `acpi: bios-area rsdp: 2 ioapic(s), …, dsdt 420 bytes, 2 virtio-mmio device(s), _S5 Some(5)`, `mmio @ 0xfeb02e00 irq 47 device-id 3 (console) [acpi dsdt, …, level irq]`, `mmio @ 0xfeb02c00 irq 46 device-id 4 (rng)`; `microvm` with `acpi=off`: `mmio @ 0xfeb00e00 irq 12 … (console) [cmdline, …, edge irq]`, `0xfeb00c00 irq 11 (rng)`; `pc`/`q35`: `virtio: no devices`, `ctl: no transport`, exit 0. Modern (v2) MMIO: aarch64 with `-global virtio-mmio.force-legacy=false` |
| **rng-seed** | any boot log | `[patina] random: seeded from virtio-rng (+RNDR)` (TCG) / `(+boot counter)`; D60 |
| **test-ctl** | `scripts/test-ctl.sh aarch64 x86_64` | 16/16 PASS (ready-ping, stop-handler, stop-nohandler, stop-forced, dump, usr1-time, hvc0, panic × 2 arches). Lines: `{"ev":"ready","go":"go1.27.1","cpus":2,"mem_mb":512}` (x86_64 reports 511: the e820 map), `{"ev":"pong","id":42,"uptime_ms":104}`, `{"ev":"exit","code":0,"forced":false}`, `{"ev":"exit","code":143,"signal":"SIGTERM","forced":false}`, `{"ev":"exit","code":137,"forced":true}` 1.01 s after `stop` with `grace_ms` 1000, `{"ev":"error","id":6,"msg":"signal not injectable","field":"sig"}`, `{"ev":"log","level":"warn","msg":"ENOSYS 9999 (?)"}`, `{"ev":"panic","msg":"selftest"}` then the dump and exit 255; `dump` → goroutine dump and exit 2; `time` to 2000-01-01 → `SIGUSR1 at 946684800`; the channel survives disconnect + reconnect and a late-connecting host still gets `ready` |
| **hvc0** | `PATINA_CONSOLE=hvc0 scripts/run-qemu.sh <arch> …` (the `hvc0` case of **test-ctl**) | the whole boot log, the program output and `[patina] exit code N` on virtio-console port 0 while `patina.ctl` works on port 1, both arches |
| **poweroff-order** | `PATINA_MACHINE=pc`/`microvm` without `isa-debug-exit` | `[patina] poweroff: acpi s5 via pm1a_cnt (io 0x604, SLP_TYP 0)` (pc), `acpi s5 via sleep control register (mem 0xfea00200, SLP_TYP 5)` (microvm); with the device, isa-debug-exit as before (D64) |
| **selftests-p3** | `PATINA_CPUS=4 … patina.selftest=smp` / `timer`, both arches | `smp: ok (4 cpus, …)`, `timer: ok (5 ticks in 52 ms)` after the IOAPIC/IRQ changes |
| **test-runtime-p3** | `PATINA_CPUS_LIST="1 2 4" scripts/test-runtime.sh aarch64 x86_64` on main 149968b (VFS + virtio + control channel + os harness merged) | 66/66 rows PASS, `test-runtime: all passed`; in the same session **test-ctl** 16/16, **fileio** at `PATINA_CPUS=4` on both arches and **test-os** at 1 and 4 vCPUs on both arches all passed |
| **test-os** | `PATINA_CPUS=1 scripts/test-os.sh aarch64 x86_64` and `PATINA_CPUS=4 …` (Go 1.27.1 `go test -c os`, `-test.short`, `$GOROOT/src/os` packed at `/src/os` with `patina.cwd=/src/os`, skip list `testapps/skiplist/os.txt`) | `os aarch64 (cpus=1): 244 passed, 37 skipped (14 by our list), 0 failed, exit 0`; the same four numbers for x86_64 and at 4 vCPUs on both (aarch64 4-vCPU repeated 4×). The 23 self-skips: 6 need `go build`, 7 `-test.short`, 5 because we are root, `TestStatConsole` (Windows), `TestLargeWriteToConsole` (flag), `TestMkdirAllAtSlash` (builders only), `TestTTYClose` (`/dev/tty` ENXIO by design), `TestHostname` (execs `hostname`). Our 14: 4 "network: Phase 4" (`TestLargeCopyViaNetwork TestSpliceFile TestGetPollFDAndNetwork TestSendFile`), 10 "needs os/exec (no execve, D57)". Without `-test.short`: 249 passed, 32 skipped, 0 failed (x86_64 2 vCPUs, aarch64 4 vCPUs). ENOSYS lines seen: `socket` (skipped net tests), `pidfd_open` (Go falls back). ~25 s per run on HVF, ~30 s on TCG |
| **test-cli** | `cd cli && PATINA_E2E=1 go test ./internal/run -run E2E -v` (builds the CLI and testapps, `patina build` → `patina run` on QEMU, `patina ctl` from a second process; `PATINA_E2E_ARCH` narrows it) on main fabbb22 | 20/20 PASS in 17 s: per arch hello-args (exit 3, `argv: ["/app" "3" "a" "b"]`, `-v` ready line), stop-handler (0, `PASS signals (handler)`, ctl prints the exit event), stop-nohandler (143, `"signal":"SIGTERM"`), stop-forced (`-mode ignore`, `--grace 1s` → 137 `forced:true` in ~1.3 s), ping (pong + uptime), dump (`SIGQUIT: quit`, goroutine dump, exit 2), signal-usr1, timeout (`--timeout 3s` → 124), interrupt-nohandler (SIGINT → 143), interrupt-twice (→ 137 < 3 s). `go test ./...` and `go vet ./...` in `cli/` green; `scripts/test-hello.sh` still passes with hello's new `env:` line. By hand: `--cpus 1/2`, `--mem 256`, `--strace` (231/237 `[strace]` lines), `--env FOO=bar`, `patina ps` |

VFS and files (D53–D57):

- [x] VFS: initrd unpacked into a writable in-memory tree (`patina-vfs` behind one IRQ-safe `SpinLock`, file pages borrowed from the initrd until written, `/app` opened through it) — **fileio**, **loader-errors-p3**, every Phase 2 suite
- [x] Regular files with a per-description offset, `O_APPEND`/`O_TRUNC`/`O_EXCL`/`O_NONBLOCK`/`O_CLOEXEC`/`FD_CLOEXEC`/`O_DIRECTORY`/`O_NOFOLLOW`/`O_PATH`, directories with `getdents64`, cwd (`chdir`/`fchdir`/`getcwd`), umask — **fileio**
- [x] devfs (`null zero full urandom random console stdin stdout stderr fd tty`), procfs-lite (`self/exe cwd maps status stat fd/`, `cpuinfo`, `meminfo`, `sys/kernel/*`, `mounts`, `uptime`, `version`), `/sys/kernel/mm/transparent_hugepage/hpage_pmd_size`, writable `/tmp`, generated `/etc/{resolv.conf,hosts,nsswitch.conf,passwd,group}` unless the initrd supplies them — **fileio**, **go-startup-reads**
- [x] Pipes, eventfd, epoll over a common `File` trait — Phase 2 (**fdtest**, D46); named FIFOs (D55) — **fileio**
- [x] Tier-2 file syscalls (§7): `pread64 pwrite64 readv writev lseek newfstatat/fstat statx getdents64 mkdirat unlinkat renameat2 readlinkat symlinkat linkat ftruncate fchmod fchown utimensat fsync faccessat2 dup dup3 getcwd chdir getuid geteuid getgid getegid`, plus `fchdir fstatfs statfs umask mknodat truncate fallocate flock fadvise64 close_range` and the x86_64 legacy entry points — **fileio**; `copy_file_range`/`sendfile`/`splice`/`openat2` are silent `ENOSYS` (Go falls back) — **fileio** (`io.Copy`, `os.Root`)
- [x] Tier-3 stubs (D57): `wait4/waitid → ECHILD`, `execve/fork/vfork → ENOSYS`, `prctl`, `getpgrp/setpgid/getsid`, `set*id → 0`, `capget/capset`, `sched_*`, `sysinfo`, `getcpu`, `mlock*`, xattr → `EOPNOTSUPP` — implemented; exercised by **test-os** (see its row)
- [~] Not exercised: FIFO opens interrupted by a signal, `O_NONBLOCK` FIFO opens, `RENAME_EXCHANGE` from the kernel side (host-tested only), `statx` on non-VFS files, device nodes from the initrd bound by `rdev`, `ip=`-derived `/etc/hosts`/`resolv.conf` contents

Devices and control channel (D58–D64):

- [x] virtio-mmio (legacy and modern) with an in-tree `Transport` trait + split virtqueues (D58); discovery from the DTB (aarch64), `virtio_mmio.device=` cmdline entries (Firecracker x86_64; microvm `acpi=off`), the ACPI DSDT `LNRO0005` scan (microvm with ACPI, Firecracker ≥ 1.8), and a `0xfeb00000` probe as the last resort (D59) — **virtio-discovery** (DTB, DSDT and cmdline paths; the probe path untested)
- [x] IRQ routing: GIC SPIs on aarch64; x86_64 I/O APIC (MADT, level/edge from the DSDT) with LAPIC EOI, device IRQs on CPU 0 — **test-ctl** at `PATINA_CPUS=2`, **selftests-p3**
- [x] virtio-rng feeding a ChaCha20 DRBG (BLAKE2s entropy pool, periodic reseed; `getrandom`, `AT_RANDOM`, `/dev/urandom`) — **rng-seed**, host tests (RFC 8439/7693 vectors)
- [x] virtio-console multiport: `patina.ctl` found by name through the control-queue handshake, PORT_OPEN tracked; port 0 as `hvc0` console with an early ring flushed when the device comes up — **test-ctl**, **hvc0**
- [x] virtio-vsock listener on port 4242 (D62): first run on Apple VZ (Phase 5): CONNECT handshake, `ready`/`exit`/`log`/`pong` events, stop/signal/dump/time ops, reconnects — **test-vz**, **vz-ctl**, **vz-e2e** (no driver bug surfaced; the ctl misses on fast programs were the CLI's 100 ms dial, D85). Firecracker/Cloud Hypervisor's vsock-over-Unix-socket path remains unverified
- [x] Control channel (D63): one kernel thread; `ping`→`pong(uptime_ms)`, `signal` (SIGTERM INT HUP USR1 USR2 QUIT), `stop` (SIGTERM + grace → forced 137), `time` (steps `CLOCK_REALTIME`, VFS timestamps follow), `dump` (SIGQUIT); events `ready` (go version from `.go.buildinfo`, cpus, mem_mb), `exit` (code, signal, forced; flushed before poweroff), `panic`, `log` (ENOSYS); pending buffer until a host connects; one connection at a time — **test-ctl**
- [x] x86_64 poweroff order isa-debug-exit → ACPI S5 (hardware-reduced sleep control register, else PM1a) → i8042 reset (D64) — **poweroff-order** (QEMU paths); Cloud Hypervisor S5 at port 0x600 and Firecracker i8042 untested
- [~] Firecracker (cmdline discovery, 16550 MMIO, i8042), Cloud Hypervisor (virtio-pci: Phase 4): code written to the spec, no host to run them. Apple VZ (`hvc0`, vsock, virtio-pci INTx): verified in Phase 5 (**test-vz**, D83–D85)

CLI and `os` tests:

- [x] `patina run` / `patina ctl` end-to-end on QEMU, both arches (D66: chardev `wait=on`, exit-code precedence timeout 124 → `exit` event → `panic` 255 → sentinel → isa-debug-exit → killed 137) — **test-cli**
- [~] `patina run` on Firecracker and Cloud Hypervisor: CLI drivers exist (tested against fakes), the kernel side (vsock over a Unix socket, cmdline discovery, S5/i8042 poweroff) is unverified on this host. Apple VZ: verified (**test-vz**, **vz-e2e**)
- [x] `os` package tests pass with a 14-entry skip list (D65): kernel additions for it were `patina.cwd=<dir>` (start directory), `/proc/sys/fs/pipe-max-size`, `uname` nodename = hostname, and `mkinitrd.sh` directory mappings — **test-os**

## Phase 4 — Net

Test names (aarch64 = QEMU `virt` HVF, x86_64 = QEMU `microvm` TCG unless
noted, release profile). Decisions: D67 (smoltcp stack, loopback mux, net
task), D68 (inet sockets, syscalls, listen pools, dual stack), D69
(virtio-net), D70 (virtio-pci, PCI discovery, MSI-X/ITS/v2m/INTx), D71
(AF_UNIX), D72 (DHCP, effective config, resolv.conf/hosts, test-net.sh),
D73 (net package tests, netlink, sendfile).

| Name | Command | Observed |
|---|---|---|
| **selftest-net** | `PATINA_NET=user scripts/run-qemu.sh <arch> -- patina.selftest=net` | driver-only ARP round trips against slirp: `[patina] virtio-net: mac 52:54:00:12:34:56, queues rx 128 / tx 32, link up (mmio @ 0xa003a00, hdr 10 B)`, `selftest net: arp reply 10.0.2.2 is-at 52:55:0a:00:02:02 after 287 us`, `selftest net: ok (64/64 replies, irqs 29)`, exit 0; both arches; also TCG, `PATINA_CPUS=2`, modern mmio (`-global virtio-mmio.force-legacy=false`, 12-byte header), `-M microvm,acpi=off` (cmdline discovery), and over PCI (`PATINA_MACHINE=q35`: `pci 0000:00:03.0 irq 306 … msi-x irq`; aarch64 `PATINA_VIRTIO=pci`: v2m `irq 82` on hvf, ITS `irq 8194` on TCG) |
| **sockets** | `scripts/test-hello.sh --app sockets aarch64 x86_64`; `scripts/test-smp.sh --app sockets --cpus "2 4" aarch64 x86_64`; `PATINA_MACHINE=q35 … x86_64` | `PASS sockets` on both arches (21 `ok` lines: TCP echo over 127.0.0.1, [::1] and a dual-stack `:0` listener, 50 concurrent connections, 8 MiB each way with sha256, UDP echo + connected UDP, ECONNREFUSED, EOF/EPIPE after peer close, deadlines, CloseWrite, SO_REUSEADDR rebind, MSG_PEEK, SO_ERROR, TCP_NODELAY, nonblocking connect → EINPROGRESS + writable, accept4 flags, IPV6_V6ONLY, FIONREAD, http 1 MiB body); 2/4 CPUs, q35/PCI and `PATINA_PROFILE=dev` pass; with `patina.strace=1` no `ENOSYS` lines (Go's `IPPROTO_MPTCP` probe gets `EPROTONOSUPPORT`) |
| **unixsock** | `scripts/test-hello.sh --app unixsock aarch64 x86_64`; `scripts/test-smp.sh --app unixsock --cpus "2 4" --repeat 2 aarch64 x86_64` | `PASS unixsock` both arches (36 `ok` lines: path and abstract names, `ModeSocket`, ENXIO open, ENOENT/ECONNREFUSED dials, unlink-while-listening, 50 connections, 8 MiB each way, EOF/EPIPE, deadlines, socketpair STREAM/DGRAM/SEQPACKET, SCM_RIGHTS over socketpair and `net.UnixConn` incl. MSG_CMSG_CLOEXEC and MSG_CTRUNC, SO_PEERCRED {1,0,0}, SO_PASSCRED + SCM_CREDENTIALS, unixgram ReadFrom/WriteTo, 64 KiB datagram, bounded queue, backlog EAGAIN/blocking connect, autobind, http over unix); 8/8 at 2/4 CPUs; no `ENOSYS` under strace |
| **test-net** | `scripts/test-net.sh aarch64 x86_64` (Milestone A on QEMU; `PATINA_NET_EXTERNAL=1` adds a real DNS lookup) | 14/14 PASS, repeated 3×: per arch **serve** (`patina run --net user --publish P:8080`, `curl` → `hello from patina`, 20 concurrent GETs, `/big` 4 MiB sha256 match, `patina ctl stop` → `{"ev":"exit","code":0,"forced":false}`, `PASS httphello (graceful shutdown)`, `patina run` exit 0), **nohandler** (`-nohandler`: `{"ev":"exit","code":143,"signal":"SIGTERM","forced":false}`, exit 143), **selftest** (loopback GET `/`, `/big`, POST `/echo`, `LookupHost` of `localhost` and the guest hostname via /etc/hosts; `ok dns example.com -> …` through slirp's 10.0.2.3 with `PATINA_NET_EXTERNAL=1`), **dhcp** (`ip=dhcp` over slirp: `[patina] net: dhcp lease 10.0.2.15/24 via 10.0.2.2, dns 10.0.2.3 (2 ms)`; 5–7 ms on x86_64 TCG), **dhcp-nonic** (`ip=dhcp` without a NIC: warning line + ctl `log` event, program still runs), **dhcp-nolease** (dead link: `dhcp: no lease after 10 s, continuing without an address`, boot continues), **pci** (x86_64 `-M q35` / aarch64 `PATINA_VIRTIO=pci`: virtio-net-pci `hdr 12 B`, curl, stop via `ctlclient.py` → exit 0) |
| **test-gonet** | `scripts/test-gonet.sh aarch64 x86_64` (Go 1.27.1 `go test -c net`, `-test.short`, `$GOROOT/src/net` at `/src/net`, `PATINA_NET=user` + the CLI's static `ip=`, skip list `testapps/skiplist/net.txt`) | `net aarch64 (cpus=1): 254 passed, 55 skipped (1 by our list), 0 failed, exit 0`; the same for x86_64 and at `PATINA_CPUS=4`. The one listed skip is `TestSplice` (needs os/exec). The 54 self-skips: 31 "no external network in -short", other `-short` skips, the cgo resolver (3), `-ipv6`/`-tcpbig`/`-dnsflood` flags, `/etc/services`, the pty test, MPTCP, raw-IP subcases (SOCK_RAW → `EPERM`). Only `ENOSYS` line: `pidfd_open`. Without `-short` (2 CPUs): 250 passed, 3 failed (`TestIPv4MulticastListener`/`TestIPv6MulticastListener`: no multicast, `TestSendfileWithLargeFile`: a 2 GiB file in the RAM-backed `/tmp`) |
| **test-gonet-http** | `scripts/test-gonet.sh --pkg net/http aarch64` (and x86_64) | `net/http aarch64 (cpus=1): 611 passed, 10 skipped (0 by our list), 0 failed`; x86_64 the same; without `-short` (aarch64, 2 CPUs) 616 passed, 5 skipped, 0 failed |
| **test-os-p4** | `scripts/test-os.sh aarch64 x86_64` with the 4 network tests removed from `testapps/skiplist/os.txt` | `os <arch> (cpus=1): 248 passed, 33 skipped (10 by our list), 0 failed, exit 0` on both arches (`TestLargeCopyViaNetwork`, `TestSpliceFile`, `TestGetPollFDAndNetwork`, `TestSendFile` now run, incl. their unix subtests) |
| **p4-regression** | on the final Phase 4 tree: `scripts/test-ctl.sh aarch64 x86_64`, `PATINA_VIRTIO=pci scripts/test-ctl.sh aarch64`, `scripts/test-hello.sh --app fileio|fdtest`, `scripts/test-signals.sh`, `PATINA_CPUS_LIST="1 4" scripts/test-runtime.sh aarch64 x86_64`, `cargo test --workspace --exclude patina-kernel` (187 host tests), clippy both targets 0 warnings | test-ctl 16/16 (+ 8/8 over PCI); fileio/fdtest/signals PASS; `PATINA_CPUS_LIST="1 4" scripts/test-runtime.sh aarch64 x86_64` → `test-runtime: all passed` (44 rows: 11 cases × 1/4 CPUs × 2 arches); host tests 187/0 |

Network stack and sockets (D67–D69, D71–D73):

- [x] virtio-net driver (in-tree, D69): one RX/TX pair, no offloads, `F_MAC`/`F_STATUS`, 10/12-byte header, IRQ-driven through `net::device::NetDevice` — **selftest-net** (mmio legacy/modern, DSDT/cmdline/DTB discovery, PCI with MSI-X on both arches), every **test-net** case
- [x] smoltcp 0.14.0 `Interface` + `SocketSet` behind one lock, loopback by a mux device (ARP/NDP answered to self), net task driven by the NIC IRQ `Event` and `poll_delay` deadlines, inline polls from syscalls (D67) — **sockets**, **test-net**, **test-gonet**
- [x] Loopback 127.0.0.1/8 and ::1/128; eth0 link-local fe80::/64; static `ip=` address + default route — **sockets**, **test-net** (`[patina] net: virtio-net hwaddr …, addrs 127.0.0.1/8 ::1/128 fe80::…/64 10.0.2.15/24, gateway 10.0.2.2`)
- [x] `ip=dhcp` (smoltcp DHCPv4 client, 10 s bound before user mode, renewals in the net task), `/etc/resolv.conf` and `/etc/hosts` from the effective config (lease, else `ip=`, else 10.0.2.3) (D72) — **test-net** dhcp / dhcp-nonic / dhcp-nolease / selftest
- [x] Socket syscalls (Tier 2): `socket socketpair bind listen accept4 (accept) connect getsockname getpeername setsockopt getsockopt sendto recvfrom sendmsg recvmsg shutdown`, plus `read/write/readv/writev`, `FIONREAD`/`SIOCOUTQ`, epoll readiness (IN/OUT/RDHUP/HUP/ERR), blocking with EINTR/restart (D45), `MSG_PEEK/DONTWAIT/NOSIGNAL/TRUNC/WAITALL`, SIGPIPE on EPIPE, `SO_ERROR` for async connect, `SO_REUSEADDR/REUSEPORT/KEEPALIVE/LINGER/RCVBUF/SNDBUF/RCVTIMEO/SNDTIMEO`, `TCP_NODELAY/MAXSEG/KEEP*/USER_TIMEOUT`, `IPV6_V6ONLY`, IP/IPv6 TOS/TTL accepted — **sockets**, **test-gonet**, **test-gonet-http**
- [x] AF_INET/AF_INET6 TCP (listen pools: 4 listening smoltcp sockets per listener + an accept queue bounded by the backlog, refilled per packet; 64 KiB buffers each way) and UDP (connected and unconnected), dual stack (`[::]` listeners accept IPv4 as `::ffff:a.b.c.d`; Go's init probes succeed), ephemeral ports 32768–60999, `EADDRINUSE`/`EADDRNOTAVAIL`/`ECONNREFUSED`/`ECONNRESET`/`ETIMEDOUT`/`ENETUNREACH` — **sockets**, **test-net** (20 concurrent requests, 4 MiB body), **test-gonet**
- [x] `AF_UNIX` stream, datagram and seqpacket (D71): filesystem paths as `S_IFSOCK` nodes, abstract names, autobind, socketpair, SCM_RIGHTS (253 fds, MSG_CMSG_CLOEXEC, MSG_CTRUNC), SCM_CREDENTIALS/SO_PASSCRED, SO_PEERCRED = pid 1/uid 0/gid 0, 208 KiB buffers — **unixsock**, **test-gonet** (the 34 unix tests), **test-os-p4**
- [x] Read-only `AF_NETLINK`/`NETLINK_ROUTE` (`RTM_GETLINK`/`RTM_GETADDR` dumps for `lo` and `eth0`; `net.Interfaces()`), `/proc/net/igmp{,6}`, `sendfile` (pread + write), `splice`/`tee` → `EINVAL` so Go falls back (D73) — **test-gonet**, **test-os-p4**
- [~] Not implemented: multicast (options → `ENOPROTOOPT`; 2 non-`-short` net tests fail), ICMP errors to UDP sockets, UDP fragmentation (> MTU → `EMSGSIZE`), `TCP_INFO`, `sendmmsg`/`recvmmsg`, `SOCK_RAW` (`EPERM`), fd garbage collection for SCM_RIGHTS cycles, resolv.conf rewrite on a DHCP lease change after boot, DHCP hostname/domain options (untested: slirp sends none)
- [~] IPv6 beyond loopback/link-local: no SLAAC/RA, no IPv6 default route (global IPv6 destinations → `ENETUNREACH`); fine for Go's probes and the tests above
- [~] Firecracker/Cloud Hypervisor tap networking (`ip=dhcp` / `--ip`): the kernel side is the same code as the QEMU runs; the driver paths (virtio-mmio on Firecracker, virtio-pci + MSI-X on Cloud Hypervisor) are unverified on this host

- [x] virtio-pci transport + PCI discovery (D70): ECAM (DTB / MCFG) or port 0xCF8, BAR sizing and assignment (QEMU `virt` without firmware), modern capabilities, MSI-X (x86 LAPIC; aarch64 GICv3 ITS LPIs and GICv2m SPIs), INTx through the DTB `interrupt-map` (shared lines) — **virtio-pci-a64** (`PATINA_VIRTIO=pci scripts/test-ctl.sh aarch64`, hvf = v2m, `PATINA_ACCEL=tcg` = ITS, `PATINA_CPUS=4`; `PATINA_VIRTIO=pci PATINA_CONSOLE=hvc0 scripts/test-hello.sh --app fileio aarch64`, also with `PATINA_PARAMS=patina.pci.irq=intx`), **virtio-pci-x86** (`PATINA_MACHINE=q35|pc scripts/test-ctl.sh x86_64`, `PATINA_MACHINE=q35 PATINA_CONSOLE=hvc0 scripts/test-hello.sh --app fileio x86_64`); Cloud Hypervisor paths untested

CLI and Milestone A:

- [x] CLI `build` / `run` / `ctl` with `--net user --publish` on QEMU, both arches — **test-net**, **test-cli** (Phase 3)
- [x] **Milestone A on QEMU**, both arches: `net/http` hello served to host `curl` through `patina run --net user --publish`, concurrent requests and a 4 MiB body, `patina ctl stop` → clean `{"ev":"exit","code":0}` (signal.Notify variant) and 143 + `SIGTERM` (no-handler variant), `-selftest` over loopback with DNS through slirp, DHCP and the q35/PCI variant — **test-net**
- [x] **Milestone A on Apple VZ** (Phase 5, D83–D85): `net/http` hello served to host `curl` at the guest's VZ NAT address (DHCP, no port forwarding), concurrent requests, 4 MiB body, DNS through the NAT resolver, `patina ctl stop` → clean `{"ev":"exit","code":0}` and 143 + `SIGTERM`, vsock control channel — **test-vz**, **vz-e2e** (see Phase 5)
- [ ] Milestone A on Firecracker and Cloud Hypervisor (both arches): the CLI drivers exist (Phase 3, tested against fakes) and the kernel paths are written to doc/cli-contract.md (virtio-mmio + vsock on Firecracker; virtio-pci/MSI-X + vsock on Cloud Hypervisor), but neither has booted here (no KVM on this host). Specifically unverified: vsock over the VMM's Unix socket (the driver itself now runs on VZ), Cloud Hypervisor's ITS with 64 KiB table pages and 64-bit BARs, Firecracker's `virtio_mmio.device=` net device, tap DHCP against a real server

## Phase 5 — Hardening

Milestone B (D74): Go 1.27.1 `go test -c` binaries, `-test.short`, aarch64
= QEMU `virt` HVF, x86_64 = QEMU `microvm` TCG, release kernel. Command for
every cell: `scripts/test-gopkg.sh <pkg> --arch <arch> [--cpus N]` (logs in
`out/test/gopkg/`; the x86_64 and 8-vCPU runs used `GOPKG_NO_KERNEL_BUILD=1
PATINA_TIMEOUT=3600..7200 GOPKG_TEST_TIMEOUT=55m..115m` because the host was
shared with other QEMU runs). Counts are top-level tests: passed / skipped
(of which by our skip list) / failed. "—" = not run (not in the plan).

| Name | Package | aarch64, 1 vCPU | aarch64, 4 vCPUs | x86_64, 1 vCPU | x86_64, 4 vCPUs |
|---|---|---|---|---|---|
| **test-gopkg** | `runtime` | 378 / 148 (11) / 0 | 385 / 141 (11) / 0 (3 runs identical; `--no-short`, 2 GiB: 399 passed, 1 failed — `TestPanicSystemstack`, needs exec, since listed) | 379 / 150 (11) / 0 | 386 / 143 (11) / 0 |
| **test-gopkg** | `os` | 248 / 33 (10) / 0 | 248 / 33 (10) / 0 | 248 / 33 (10) / 0 | 248 / 33 (10) / 0 |
| **test-gopkg** | `net` | 254 / 55 (1) / 0 | 254 / 55 (1) / 0 | 254 / 55 (1) / 0 | 254 / 55 (1) / 0 |
| **test-gopkg** | `os/signal` | 18 / 3 (2) / 0 | 18 / 3 (2) / 0 | 18 / 3 (2) / 0 | 18 / 3 (2) / 0 |
| **test-gopkg** | `time` | 180 / 4 (0) / 0 | 180 / 4 (0) / 0 | 180 / 4 (0) / 0 | — |
| **test-gopkg** | `sync` | 51 / 2 (2) / 0 | 51 / 2 (2) / 0 | 51 / 2 (2) / 0 | — |
| **test-gopkg** | `syscall` | 25 / 28 (10) / 0 | 25 / 28 (10) / 0 | 25 / 28 (10) / 0 | 25 / 28 (10) / 0 |
| **test-gopkg** | `io` | 72 / 0 / 0 | 72 / 0 / 0 | 72 / 0 / 0 | — |
| **test-gopkg** | `bufio` | 88 / 0 / 0 | 88 / 0 / 0 | 88 / 0 / 0 | — |
| **test-gopkg** | `context` | 46 / 0 / 0 | 46 / 0 / 0 | 46 / 0 / 0 | — |
| **test-gopkg** | `net/http` | 612 / 9 (0) / 0 | 611 / 10 (0) / 0 (before the go.mod change) | 612 / 9 (0) / 0 | — |
| **test-gopkg** | `runtime/pprof` | 44 / 3 (0) / 0 | 44 / 3 (0) / 0 | 46 / 1 (0) / 0 | — |
| **test-gopkg** | `os/user` | 12 / 0 / 0 | 12 / 0 / 0 | 12 / 0 / 0 | — |

8 vCPUs (`--cpus 8`; QEMU `-smp 8`):

| Name | Package | aarch64, 8 vCPUs | x86_64, 8 vCPUs |
|---|---|---|---|
| **test-gopkg-8cpu** | `runtime` | 385 / 141 (11) / 0, 3 runs with identical pass/skip sets; `--no-short --mem 4096`: 399 / 127 (11) / 0 (with the default 2 GiB `--no-short` ran the guest out of memory, see below) | 386 / 143 (11) / 0, 2 runs (before and after the select/utime commits) |
| **test-gopkg-8cpu** | `sync` | 51 / 2 (2) / 0 | — |
| **test-gopkg-8cpu** | `os/signal` | 18 / 3 (2) / 0 | — |
| **test-gopkg-8cpu** | `runtime/pprof` | 46 / 1 (0) / 0 | — |

No x86_64-only failure and no 8-vCPU failure: no skip-list entry was
added, and the x86_64 legacy paths, XSAVE frames under preemption and the
D52 shootdown held at 8 vCPUs. `runtime/pprof` now reports 46 passed / 1
skipped (`TestMapping`) on both arches; an aarch64 1-vCPU re-run on this
branch gives the same, so the 44 / 3 cells above are older numbers, not a
difference between arches. `runtime
--no-short` at 8 vCPUs with 2 GiB: about 20 parallel tests (`TestGcPacer`,
the `TestSmhasher*` set, large `TestMemmove*`) exhausted guest RAM
(`[patina] out of memory filling user page`, then Go's `fatal error:
unexpected signal` — the OOM-to-signal path, not a crash in our code); with
4 GiB it passes and exits with 3.8 GiB free, so this is capacity, not a
leak. The `-short` default needs well under 2 GiB at 8 vCPUs.

Regression on the tree with the `select`/`utime`/`time` commits:
`scripts/test-gopkg.sh syscall os/signal --arch aarch64` (0 failed, counts
as above), `scripts/test-signals.sh aarch64 x86_64` (12/12),
`PATINA_CPUS_LIST="1 4" scripts/test-runtime.sh x86_64` (`all passed`,
22 rows), `scripts/test-hello.sh --app fdtest|fileio aarch64 x86_64`,
x86_64 `syscall` and `os` at 1 vCPU and `runtime` at 8 vCPUs (counts as
above), host tests, clippy both targets 0 warnings.

`ENOSYS` lines across all runs: `pidfd_open` (D57; Go's os/exec probe, falls
back) and `clone3` (syscall tests that ask for a namespace or pidfd; Go
falls back to `clone`, which cannot create processes, D57). The 4-vCPU
column of the other packages was run before the final `go.mod` harness
change (time then had 179 passed, 5 skipped).

### Apple Virtualization.framework (`patina run --vmm vz`)

macOS 26 on Apple Silicon, the ad-hoc signed CLI (`cli/scripts/build.sh`),
release kernel, 2 vCPUs / 512 MiB unless noted. Decisions D83 (the VZ
console start quirk, `VIRTIO_NET_F_MTU`), D84 (`PATINA_VMM=vz`,
`patina image pack`, timing check), D85 (NAT networking, guest address,
gateway ARP, NIC tx backpressure, fast control attach). The full VZ
device layout is in doc/cli-contract.md.

| Name | Command | Observed |
|---|---|---|
| **vz-hello** | `patina run out/vz/hello.patina --vmm vz -v -- 3 a b` (`patina build --target vz-aarch64`) | boot log on hvc0 from `[patina] early: final page tables live` on (RAM 0x7000_0000, initrd right after the image, DTB at 0x7040_0000), `gic: v3 dist @ 0x10000000, redist @ 0x10010000`, `virtio: pci 0000:00:05.0 irq 69 device-id 3 (console) [ecam dtb, modern virtio-pci, vendor 0x1af4, apple host, intx irq]` (+ 06.0 vsock irq 70, 07.0 rng irq 71, 01.0 net irq 65 with `--net user`), `virtio-console: single port, ports: 0(console)`, `console: hvc0 (virtio-console port 0)`, `virtio-vsock: guest cid 3`, `ctl: virtio-vsock port 4242`, `random: seeded from virtio-rng (64 bytes)`, `patina: control channel attached (vz vsock port 4242)`, `control: ready: go1.27.1, 2 cpus, 512 MiB`, `PASS hello (exit 3)`, `control: exit code 3`, `[patina] exit code 3`, `vz: guest stopped the VM`; `patina run` exit 3. 40/40 runs attach the channel and deliver `ready` + `exit`, ~125–185 ms wall; 200 + 40 consecutive runs with 0 silent boots after D83 (before: ~15% printed nothing) |
| **vz-ctl** | `PATINA_VMM=vz scripts/test-ctl.sh aarch64` | 8/8 (ready-ping, stop-handler, stop-nohandler, stop-forced, dump, usr1-time, hvc0 — `[patina] ctl: virtio-vsock port 4242` —, panic → 255); the early `log` event is checked on serial only (a late proxy client gets just `ready`) |
| **test-vz** | `scripts/test-vz.sh` | 13/13 PASS (the 13th, `vsock`, is **vz-vsock**, D76): hello-exit (3), console (banner + sentinel), ctl (ping/pong, SIGUSR1, `time`, fixed-epoch time op → `9466848xx`), ctl-dump (goroutine dump, 2), stop-handler → 0, stop-nohandler → 143 + `"signal":"SIGTERM"`, stop-forced (`-mode ignore`, `--grace 1s`) → 137 `forced:true`, net-serve (`--net user`: `[patina] net: dhcp lease 192.168.64.182/24 via 192.168.64.1, dns 192.168.64.1`, `patina: guest address 192.168.64.182 (gateway 192.168.64.1)`, GET / = `hello from patina`, 20 concurrent GETs, `/big` 4 MiB sha256, `ctl stop` → `{"ev":"exit","code":0,"forced":false}`, run exit 0), threads, signals-self, signals-nohandler-self, sockets at 2 vCPUs |
| **vz-e2e** | `PATINA_NET_EXTERNAL=1 cli/scripts/vztest.sh --e2e` (`TestVZE2E`, cli/vz_integration_test.go) | hello-exit ×3 (rc 3, attach, ready, exit event), stop-handler/nohandler/forced, ping, net-serve (address line + `state.json` `net`, 5 GETs with #2–4 < 500 ms, 20 concurrent, `/big` sha256 ×2: 4 MiB in 13–21 ms, ~200–310 MB/s, first byte 0.3–1.5 ms; GET / connect 2.6–7 ms), net-nohandler (143), net-selftest (`-dns example.com` → `ok dns example.com -> …` via 192.168.64.1); ping RTT from the host 0.3–1.3 ms (INTx delivery) |
| **vz-runtime** | `PATINA_VMM=vz PATINA_CPUS_LIST="1 2 4" scripts/test-runtime.sh aarch64` | 33/33 rows PASS (hello 0/3, threads, smpstress, fdtest, signals handler/nohandler, sigstress, sigstress-procs2/-crash/-breakpoint, pproftest × 1/2/4 vCPUs), `test-runtime: all passed`; `PATINA_VMM=vz PATINA_CPUS=N scripts/test-hello.sh --app fileio\|sockets\|unixsock aarch64` 9/9 at 1/2/4 vCPUs. smpstress on VZ: 8×1250×Sleep(100µs) 1.55–1.60 s, 200×Sleep(500µs) 249–264 ms (QEMU hvf: ~1.5 s / 248 ms) |
| **vz-gopkg** | `PATINA_VMM=vz scripts/test-gopkg.sh os net --arch aarch64 --cpus 2` (also 1 and 4) | `os aarch64 (cpus=2): 248 passed, 33 skipped (10 by our list), 0 failed`, `net aarch64 (cpus=2): 254 passed, 55 skipped (1 by our list), 0 failed` — the QEMU counts — with the net run over a real VZ NAT lease (`virtio-net: mac …, queues rx 128 / tx 32, link up, mtu 1500 (pci 0000:00:01.0, hdr 12 B)`) |
| **vz-vsock** | `cli/scripts/vztest.sh -test.run TestVZVsock -test.v` (cli/internal/vmm/vz_vsock_darwin_arm64_test.go; also in `vztest.sh --e2e` and the `vsock` case of **test-vz**) | AF_VSOCK (D76), testapps/vsockapp at 1/2/4 vCPUs: `guest cid 3 (VZ-assigned, from config space) = IOCTL_VM_SOCKETS_GET_LOCAL_CID = getsockname`; `ok socket(AF_VSOCK, SOCK_DGRAM) -> no such device`, SEQPACKET → `socket type not supported`, protocol 6 → `protocol not supported`, `bind port 4242 -> address already in use`, `bind cid 2 -> cannot assign requested address`; `ok loopback-echo cid 1 262144 bytes`, `cid 3 65536 bytes`; `blocking connect host port 1 (0s) -> connection reset by peer` and the non-blocking + `SO_ERROR` form; host `connectToPort:5001` (no listener) → `Connection reset by peer` in 0–16 ms; `control channel on 4242: ping -> pong` while the program's sockets run; three concurrent host connections echo 1 MiB each way (one with its host reader stalled 300 ms: VZ then stalls the other two as well, ~310–330 ms; unstalled ~7–10 ms per MiB each way); `PASS vsockapp`, exit 0. 270 consecutive boots (90 × 1/2/4 vCPUs) with 0 failures |
| **vz-timing** | `testapps/sleeper` and `patina.selftest=smp\|timer` on VZ vs QEMU hvf at 1/2/4 vCPUs, 3 runs each (D84) | no VZ anomaly: kernel to `main` 3–4 ms, `Sleep(1s)` 1000.3–1001.0 ms everywhere, secondaries up in 75–95 µs (2 vCPUs) / 236–257 µs (4), +2 ms deadline at 2003–2005 µs, `timer: ok (5 ticks in 51–52 ms)`; WFI-idle wake from its own timer is ~20–25% late on both VMMs (host-side; after main's tickless idle, D77, the same applies at 10 ms: `nanosleep(10ms)` avg 12.4–13.0 ms on VZ and QEMU hvf alike, 200 × `Sleep(500µs)` 322–354 ms on both); at 1 vCPU a sleeper competing with a spinner wakes up to a quantum late on both (scheduler, noted) |

Not verified on VZ: console rx (VZ attaches no guest input), `--cpus` > 4
under the scheduler, memory sizes other than 512 MiB / 1 GiB, more than
one NAT VM concurrently (the macOS NAT bridge delays the first connection
after it is created). The stray 1-byte segment smoltcp sends after every
accepted handshake (D82) is unexplained.

- [x] Milestone B harness `scripts/test-gopkg.sh` (D74): per-package defaults (RAM, timeouts, extra files), skip lists `testapps/skiplist/<pkg>.txt`, summary/failure rules of D65; `test-os.sh`/`test-gonet.sh` are wrappers — **test-gopkg** (reproduced test-os-p4 248/0 and test-gonet 254/0)
- [x] **Milestone B on aarch64** (QEMU, 1 and 4 vCPUs): the design §13 packages (`runtime`, `os`, `net`, `os/signal`, `time`, `sync`, `syscall`) plus `io`, `bufio`, `context`, `net/http`, `runtime/pprof`, `os/user`, 0 failed; 36 listed tests, all "needs os/exec" or "GODEBUG re-exec" — **test-gopkg**
- [x] **Milestone B on x86_64** (QEMU microvm, TCG): all 13 packages at 1 vCPU and `runtime`, `os`, `net`, `syscall`, `os/signal` at 4 vCPUs, 0 failed, same 36 listed tests as aarch64 — **test-gopkg**
- [x] **8 vCPUs**: `runtime` on aarch64 (3 runs, identical) and x86_64, `sync`, `os/signal`, `runtime/pprof` on aarch64, 0 failed; `runtime --no-short` at 8 vCPUs passes with 4 GiB — **test-gopkg-8cpu**
- [x] `select` (x86_64) and `pselect6` (both arches) were `ENOSYS`: now over `File::poll` with Linux semantics (nfds clamped, `EBADF` for an unopened fd, `POLLIN/OUT/EX` sets, `ERESTARTNOHAND`, time left written back); Go's `syscall.Select` uses them — **fdtest** `ok select` (strace shows `select` on x86_64, `pselect6` on aarch64)
- [x] x86_64 `utime` and `time` were `ENOSYS` (Go's `syscall.Utime` issues `SYS_UTIME` on amd64) — **fileio** `ok utime (x86_64 SYS_UTIME)`, `ok time (x86_64 SYS_TIME)`
- [x] `recvmsg` reports `MSG_CMSG_CLOEXEC` back in `msg_flags` — **test-gopkg** syscall `TestSCMCredentials`
- [x] Per-thread credentials (`set*uid`/`set*gid`/`setfs*`/`setgroups`/`getgroups`/`getres*` with Linux's rules and capability fix-ups, `patina_abi::cred` host-tested), `PR_SET_KEEPCAPS`/`PR_GET_KEEPCAPS`, `capget` reports the sets, `/proc/1/status` `Uid`/`Gid`/`Groups`/`CapPrm`/`CapEff`, `/proc/1/task/<tid>/{status,stat,comm}` — **test-gopkg** syscall `TestSetuidEtc`, `TestAllThreadsSyscall`; os/signal `TestAllThreadsSyscallSignals`
- [x] `fcntl` record locks (`F_GETLK`/`F_SETLK`/`F_SETLKW`, `F_OFD_*`): validated like Linux, granted — **test-gopkg** syscall `TestFcntlFlock` (parent half; the child half needs exec and the test is listed)
- [x] `umount2` → `EINVAL`/`ENOENT` instead of `ENOSYS` — **test-gopkg** syscall (`TestUnshareMountNameSpace` cleanup; no `umount2` ENOSYS line any more)
- [x] `AF_VSOCK` stream sockets for the program (D76): `socket/bind/listen/accept4/connect` (blocking and non-blocking + `SO_ERROR`), send/recv/shutdown/close with credit flow control, epoll readiness, loopback (`VMADDR_CID_LOCAL`, own CID), `ECONNRESET` for a refused connect, `/dev/vsock` `IOCTL_VM_SOCKETS_GET_LOCAL_CID`, port 4242 reserved for the control channel — **vz-vsock** (Apple VZ), **vsock-none** (`scripts/test-hello.sh --app vsockapp aarch64 x86_64` on QEMU, no vsock device: `ok socket(AF_VSOCK) without a device -> address family not supported by protocol`, `/dev/vsock` open → `no such device`, PASS both arches; QEMU **test-ctl** 16/16, `test-net.sh aarch64` 7/7, `test-runtime.sh aarch64` 33/33 unchanged), core host tests `cargo test -p patina-inet vsock` (28). Unverified: Firecracker/Cloud Hypervisor vsock, guest→host connections to a host listener (the CLI registers none), `SOCK_SEQPACKET`/`SOCK_DGRAM` (refused)
- [ ] Still open in Phase 5: console-port control fallback, `--strace`, multi-queue net.

### vDSO

Decision D75. `scripts/test-hello.sh --app vdsobench [arch...]` (add
`PATINA_PARAMS="patina.novdso=1 patina.env=VDSOBENCH_EXPECT=novdso"` for the
no-vDSO baseline, `PATINA_PARAMS=patina.strace=1` to check the timed loops
for syscalls); `cargo test -p patina-vdso` for the image checks.

| Name | Command | Observed |
|---|---|---|
| **vdsobench** | `scripts/test-hello.sh --app vdsobench aarch64 x86_64`, the same with `PATINA_PARAMS="patina.novdso=1 patina.env=VDSOBENCH_EXPECT=novdso"`, with `PATINA_CPUS=4`, and with `PATINA_PARAMS=patina.strace=1` | `PASS vdsobench` in all four configurations on both arches. ns per `time.Now()` (1e6 calls, 1 vCPU): **aarch64 63.4 with the vDSO / 333.5 without (5.3×)**, **x86_64 361.9 / 3004 (8.3×)**; `crypto/rand.Read` of 32 B: aarch64 120 / 569 ns, x86_64 1139 / 3837 ns; 4 vCPUs: 63.6 / 437.6 ns per `time.Now()`. `runtime.nanotime` lies within a `clock_gettime(CLOCK_MONOTONIC)` syscall bracket 1000/1000 times (exact math, same formula as the kernel), `time.Now()` within 1 ms of `CLOCK_REALTIME`, a 100 ms sleep measures 101–104 ms, `/proc/self/maps` shows `7ffff8000000-7ffff8001000 r-xp … [vdso]` and `7ffff8001000-7ffff8002000 r--p … [vvar]` (none with `patina.novdso=1`). strace: 0 `clock_gettime`/`gettimeofday`/`getrandom` syscalls inside the 1e6 `time.Now` loop and the 1e5 `rand.Read` loop; one 32-byte key fetch at the first `rand.Read` and one more after a forced kernel reseed |
| **p5-vdso-regression** | on the vDSO tree: `scripts/test-hello.sh aarch64 x86_64`, `scripts/test-signals.sh aarch64 x86_64`, `scripts/test-hello.sh --app fileio aarch64 x86_64`, `PATINA_CPUS_LIST="1 4" scripts/test-runtime.sh aarch64 x86_64`, `scripts/test-os.sh aarch64`, `scripts/test-ctl.sh aarch64 x86_64`, host tests, clippy both targets | all PASS: hello, 12/12 signals cases, fileio, `test-runtime: all passed` (1/4 CPUs, both arches), `os aarch64 (cpus=1): 248 passed, 33 skipped (10 by our list), 0 failed`, test-ctl 16/16 (incl. `usr1-time`), `cargo test --workspace --exclude patina-kernel` 0 failures, clippy 0 warnings |
| **vdso-image** | `cargo test -p patina-vdso` | 7 host tests: both images are clean `ET_DYN` objects (one R+X PT_LOAD at 0, PT_DYNAMIC, no relocations, no writable sections), Go's symbols resolve through `DT_HASH` with the version check of `runtime/vdso_linux.go`, Go's precomputed SysV hashes match, `__vvar_page` = the mapped vvar offset, the image's ChaCha20 = RFC 8439 |

- [x] In-tree vDSO built with the pinned toolchain only (`crates/vdso/build.rs`: `rustc --emit=obj` + `rust-lld -shared`, both arches), embedded through the `patina-vdso` crate; `scripts/build-kernel.sh` unchanged — **vdso-image**, every QEMU run
- [x] Mapped at `0x7fff_f800_0000` (`[vdso]`, R+X) + vvar page (`[vvar]`, R), `AT_SYSINFO_EHDR`; `patina.novdso=1` turns it off — **vdsobench**
- [x] `clock_gettime` (REALTIME, MONOTONIC, MONOTONIC_RAW, BOOTTIME, *_COARSE, TAI; others → syscall from the vDSO), `gettimeofday`, `time`, `clock_getres`; Go's `nanotime`/`walltime` make no syscalls — **vdsobench** (strace)
- [x] vvar seqlock; the control channel's `time` op updates the vvar realtime offset — **test-ctl** `usr1-time` (Go's `time.Now()` after the step goes through the vDSO)
- [x] vDSO `getrandom` (Linux 6.11 contract: params query, per-thread opaque state, ChaCha20 with fast key erasure, key refill on kernel reseed via `rng_generation`) — **vdsobench** (output checks, strace shows the refill after a reseed)
- [~] x86_64 TSC assumed synchronized/invariant across CPUs (true under TCG; not checked on KVM hosts); vDSO `getrandom` re-entry fallback (`in_use`), vvar refault after `madvise(DONTNEED)`/`munmap` of the vDSO: written, not exercised

### Benchmarks

Decision D79. `scripts/bench.sh aarch64 x86_64 --cpus 1,2 --novdso` runs
`testapps/bench` (Go 1.27.1, release kernel, `PATINA_MEM=512`) 3× per
configuration and prints the **median of 3**. Host: Apple M1 Max (10
cores), macOS 26.6.2, QEMU 11.1.2; aarch64 = `virt` + HVF, x86_64 =
`microvm` + **TCG** (an emulator: its absolute numbers are not comparable
with aarch64 or with real x86 hardware, only its ratios mean something).
Run 2026-10-14 while other agents' QEMU guests shared the host, so single
runs vary by ~10–30 %; the medians are the honest number. Latency rows:
lower is better; MB/s = 10^6 bytes/s. The `novdso` columns are the clock
rows rerun with `patina.novdso=1` (other rows "-").

| Benchmark | unit | aarch64 1 vCPU | aarch64 1, novdso | aarch64 2 vCPUs | aarch64 2, novdso | x86_64 1 vCPU | x86_64 1, novdso | x86_64 2 vCPUs | x86_64 2, novdso |
|---|---|---|---|---|---|---|---|---|---|
| `getpid-raw` (RawSyscall: bare entry/exit) | ns | 57 | - | 59 | - | 574 | - | 474 | - |
| `getpid-syscall` (syscall.Syscall: + Go's entersyscall) | ns | 76 | - | 77 | - | 915 | - | 749 | - |
| `clock_gettime-syscall` (raw, MONOTONIC) | ns | 132 | 136 | 134 | 135 | 1144 | 1174 | 1157 | 1026 |
| `time.Now` (vDSO) | ns | 60 | 266 | 63 | 332 | 286 | 2478 | 266 | 2211 |
| `getrandom32-syscall` (32 B) | ns | 387 | 388 | 385 | 388 | 2783 | 2816 | 2491 | 2307 |
| `crypto-rand-read32` (vDSO getrandom) | ns | 121 | 427 | 122 | 455 | 1114 | 3655 | 908 | 3394 |
| `futex-pingpong` (2 locked threads, raw WAIT/WAKE) | µs/round trip | 0.65 | - | 0.90 | - | 8.66 | - | 9.03 | - |
| `chan-pingpong` (goroutines, unbuffered chans) | µs/round trip | 0.23 | - | 0.21 | - | 4.09 | - | 3.85 | - |
| `mutex-handoff` (sync.Mutex + Cond, strict alternation) | µs/round trip | 0.22 | - | 0.22 | - | 4.06 | - | 4.13 | - |
| `pipe-pingpong` (2 locked threads, blocking pipes, 1 B) | µs/round trip | 1.01 | - | 1.40 | - | 19.3 | - | 27.0 | - |
| `pipe-throughput` (64 KiB writes, 1 s) | MB/s | 3306 | - | 3567 | - | 396 | - | 899 | - |
| `netpoll-pipe-pingpong` (os.Pipe: epoll wakeup) | µs/round trip | 1.75 | - | 1.80 | - | 30.2 | - | 26.8 | - |
| `tcp-rtt` (127.0.0.1, 1 B, TCP_NODELAY) | µs/round trip | 4.33 | - | 4.51 | - | 72.9 | - | 74.2 | - |
| `tcp-throughput` (64 KiB writes, 2 s) | MB/s | 693 | - | 688 | - | 146 | - | 144 | - |
| `unix-rtt` (AF_UNIX stream, 1 B) | µs/round trip | 1.87 | - | 1.96 | - | 34.1 | - | 33.5 | - |
| `unix-throughput` (64 KiB writes, 2 s) | MB/s | 2823 | - | 3223 | - | 400 | - | 730 | - |
| `thread-create-exit` (LockOSThread'd goroutine exits: clone + exit) | µs | 3.79 | - | 6.60 | - | 71.8 | - | 74.2 | - |
| `mmap-munmap-1M` (anon, touch one page) | µs | 1.41 | - | 1.54 | - | 13.1 | - | 42.9 | - |
| `signal-rtt` (kill(self, SIGUSR1) → signal.Notify chan) | µs | 3.17 | - | 6.89 | - | 41.3 | - | 40.0 | - |

| Name | Command | Observed |
|---|---|---|
| **bench** | `scripts/bench.sh aarch64 x86_64 --cpus 1,2 --novdso` (3 runs each) | every run prints `PASS bench` and exits 0 on both arches, 1 and 2 vCPUs, with and without the vDSO; no `ENOSYS` line; table above. No HVF row crosses the pathology thresholds of D79 (futex > 50 µs, pipe ping-pong > 100 µs, TCP < 200 MB/s, signal > 100 µs) |

- [x] Per-syscall benchmarks (design §14): `testapps/bench` (std only, `-n` scale, `-only`, `-list`) + `scripts/bench.sh` (medians, `--novdso`) — **bench**
- [~] Known costs, explained in D79, not fixed: TCP loopback moves ~4× less data than AF_UNIX and has ~2.3× its round trip (smoltcp under one lock + the net task, D67); with 2 vCPUs, cross-CPU wakeups (Reschedule IPI to a CPU idling in WFI) roughly double `signal-rtt` and `thread-create-exit` on HVF; x86_64 `mmap-munmap-1M` at 2 vCPUs is 3× the 1-vCPU cost because `munmap` sends a `TlbFlush` IPI (aarch64 uses broadcast `tlbi …is` instead), which TCG makes expensive

### Go version matrix

Decision D79, design §13/§15. go1.26.9 (`go install
golang.org/dl/go1.26.9@latest && go1.26.9 download`, outside the repo) next
to the default go1.27.1. `GO=go1.26.9 scripts/build-testapp.sh …` picks the
toolchain (default `go`, unchanged behaviour). Each app was run with
`patina.strace=1` on both arches, 1 vCPU; the ENOSYS set is the union of
`[patina] ENOSYS` lines and strace lines ending `= -ENOSYS` (the silent
by-design ones).

| Name | Command | Observed |
|---|---|---|
| **gomatrix-1.26** | per arch: `GO=go1.26.9 scripts/build-testapp.sh <app> …`, `run-qemu.sh … -- patina.strace=1 -- <args>` for hello (`3 a b`, exit 3), threads, fdtest, fileio, signals (`-self`), sockets, unixsock, httphello (`-selftest`), bench (`-n 0.01`); the same with go1.27.1 | **18/18 PASS with go1.26.9 and 18/18 with go1.27.1** (`go version` of each binary checked). ENOSYS set identical for both versions and arches: only `copy_file_range` (fileio, silent by design, Go falls back to read/write); no `[patina] ENOSYS` line at all. The distinct-syscall sets per app are the same for both versions except timing noise (`sched_yield`, and `getpid`+`tgkill` from a preemption signal, appear in one version's run or the other's) |
| **gomatrix-os-1.26** | `scripts/test-os.sh aarch64` with `$HOME/sdk/go1.26.9/bin` first in `PATH` (test-gopkg.sh runs plain `go test -c`; the `GO` knob does not reach it, a PATH shim does, no harness change) | `os aarch64 (cpus=1): 247 passed, 33 skipped (10 by our list), 0 failed, exit 0`; test binary `go1.26.9`; ENOSYS: `pidfd_open` only (as with 1.27). 247 vs 248 is the test set: 1.27 adds `TestFileFdWithConcurrentIO`, `TestReadDirFD`, `TestRootSymlinkNormalization` and drops `TestRootOpenatFallback` |

- [x] Two-version matrix (go1.26.9, go1.27.1): same ENOSYS set, no behavioural difference — **gomatrix-1.26**, **gomatrix-os-1.26**
- [x] Go 1.27.1 syscall numbers (`runtime/sys_linux_{amd64,arm64}.s`, `internal/runtime/syscall/linux`, `internal/syscall/unix`, `syscall`, `os`, `net`, `internal/poll`, `crypto/rand`, `time`) diffed against `kernel/src/syscall/mod.rs`: nothing on a normal path is missing; the unimplemented ones are listed in D79

### Robustness

Decisions D77 (tickless idle, sub-tick wall timers, overrun fix), D78
(commit accounting, OOM), D80 (kernel stack and page-table reclaim).
`scripts/test-robust.sh [aarch64] [x86_64]` runs testapps/robust per arch
(logs in `out/test/robust/`; vCPUs from `PATINA_CPUS`, `idle` uses at
least 4, `reclaim-4cpu` 4, `oom`/`oomfault` `PATINA_MEM=256`).

| Name | Command | Observed |
|---|---|---|
| **test-robust** | `scripts/test-robust.sh aarch64 x86_64` (also `PATINA_CPUS=4`) | 24/24 PASS (12 cases per arch: default, idle, restart, reclaim, reclaim-4cpu, sigill, accerr, sigkill, sigpipe, oom, oomfault, sigbus on aarch64 / gp on x86_64). Sub-tick timers (100 sequential 500 µs expiries, never early): `timer_settime` 63.5 ms (aarch64) / 138–173 ms (x86_64 TCG), `CLOCK_REALTIME` absolute timer and `setitimer(ITIMER_REAL)` the same; abs `clock_nanosleep` 130–160 µs late on aarch64, ~1.2 ms on TCG; `timer_getoverrun 19` after 40 ms blocked at 2 ms (was 1); `ITIMER_VIRTUAL` after 24–40 ms of CPU; `time.Sleep(1s)` 1.0004–1.004 s on idle 4 vCPUs |
| **tickless-idle** | `test-robust.sh` case `idle` (`patina.stats=1`, 4 vCPUs: 3 × `Sleep(1s)` then 3 s idle, ~7 s uptime) | timer IRQs per CPU: aarch64 `1, 33, 10, 0`, x86_64 `14, 42, 2, 26`; at 8 vCPUs (aarch64) `4, 34, 1, 0, 0, 0, 7, 0` — a 100 Hz tick would give ~700 each. The busiest CPU is the one running sysmon / the timer goroutine |
| **reclaim** | `test-robust.sh` cases `reclaim` (1 vCPU) and `reclaim-4cpu` | kernel-held frames (MemTotal − MemFree − VmRSS) over 5 × 2000 thread exits after a warm-up round: aarch64 +5 / −3, x86_64 +6 / −10 (4 vCPUs: deferred shootdown path); before D80 every stack stayed pooled (2000 × 8 frames). 1 GiB `MAP_NORESERVE` touched every 2 MiB: 513 page-table frames, all returned by `munmap` (delta 0). `patina.selftest=vm`: `1 frames held by page tables` (was 7) |
| **oom** | `test-robust.sh` cases `oom` / `oomfault` (`PATINA_MEM=256`) | `[patina] memory: commit limit 226 MiB (15 MiB kernel reserve)`; the third 64 MiB chunk: `[patina] mmap: 65536 KiB refused, commit limit reached (184400 of 232344 KiB committed, D78)`, Go prints `fatal error: runtime: out of memory`, exit 2 (both arches). Before: `out of memory filling user page`, `SIGSEGV code=0x2`, `fatal error: fault`. `oomfault`: `[patina] out of memory: no free frame for user page 0x7eff8836d000 (0 of 64211 frames free, 12158 of 58086 pages committed); killing the program (SIGKILL)`, `[patina] exit signal SIGKILL`, 137 |
| **robust-8cpu** | `PATINA_CPUS=8`: `test-smp.sh --cpus 8 --app smpstress` / `threads` / `sigstress` on `aarch64 x86_64`, `test-signals.sh aarch64`, `test-robust.sh aarch64` | all PASS: `[patina] smp: 8 cpus online`, `GOMAXPROCS=8 NumCPU=8`, smpstress `200 x Sleep(500us) 252 ms` (aarch64) / `285 ms` (x86_64), 6/6 signal cases, 12/12 robust cases |
| **fxsave-only** | `PATINA_QEMU_EXTRA="-cpu max,-xsave"` and `"-cpu Nehalem"`: `test-signals.sh x86_64`, `test-smp.sh --app smpstress --cpus 2 x86_64`; robust `default`/`gp`/`sigill` on Nehalem | all PASS; `[patina] early: cpu: no fsgsbase (FS_BASE msr), no smep/smap, fp state fxsave (512 bytes)` on Nehalem; `AT_HWCAP2` 0 there (Linux: `HWCAP2_FSGSBASE` only when enabled) |
| **p5-robust-merge** | on main after merging the robustness branch: `scripts/test-robust.sh aarch64 x86_64`, `PATINA_CPUS_LIST="1 4" scripts/test-runtime.sh aarch64 x86_64`, `scripts/test-smp.sh --cpus "2 4 8" aarch64 x86_64`, `scripts/test-gopkg.sh runtime --cpus 4`, `scripts/test-gopkg.sh time sync os/signal`, `scripts/test-hello.sh --app pproftest` / `--app vdsobench`, `scripts/test-ctl.sh aarch64 x86_64`, `scripts/test-net.sh aarch64 x86_64`, host tests, clippy both targets; plus a scratch program on `PATINA_MEM=256` doing anonymous `mmap` of 64 MiB / 1 GiB / 4 GiB | all PASS: robust 24/24 (idle timer IRQs per CPU aarch64 `10, 35, 0, 0`, x86_64 `31, 42, 2, 2`; kstack reclaim kernel-held delta +4 over 10000 thread exits at 1 vCPU, +43 / +7 at 4 vCPUs; page tables delta 0 after `munmap`), `test-runtime: all passed` (44 rows), smpstress 6/6 (2/4/8 vCPUs), `runtime aarch64 (cpus=4): 385 passed, 141 skipped (11 by our list), 0 failed`, `runtime x86_64 (cpus=4): 386 passed, 143 skipped (11 by our list), 0 failed`, time 180/4/0, sync 51/2/0, os/signal 18/3/0 on both arches, pproftest and vdsobench both arches, test-ctl 16/16, test-net 14/14, 200 host tests, clippy 0 warnings. Scratch `mmap`: 64 MiB succeeds (`Committed_AS` 48632 → 114168 kB of `CommitLimit` 232660 kB, back after `munmap`), 1 GiB and 4 GiB → `ENOMEM` on both arches |

- [x] Tickless idle (D77) — **tickless-idle**, **test-robust**; pproftest, sleep latency and the signal suite unchanged (**p5-robust-regression**)
- [x] Sub-tick POSIX/itimer wall-clock timers, overrun accounting fix (D77) — **test-robust** `default`
- [x] Fault → signal paths: SIGBUS `BUS_ADRALN`, SIGILL, `#GP` → `SI_KERNEL`, `SEGV_ACCERR` with the right `si_addr`/`si_code` — **test-robust**
- [x] Kernel stacks and user page tables returned to the frame allocator (D80) — **reclaim**
- [x] `mmap` beyond RAM → ENOMEM through commit accounting; fault-time OOM → SIGKILL with a clear line (D78) — **oom**
- [x] > 4 vCPUs (8) — **robust-8cpu**
- [x] x86_64 FXSAVE-only CPUs — **fxsave-only**
- [~] `FPE_FLTINV`, `sigaltstack` `EPERM`/auto-disarm on delivery, `ppoll`'s temporary sigmask, `clock_nanosleep` on the process CPU clock: not reachable on this host or from Go

### Syscall validation

Decisions D86 (user buffers checked before data is consumed, the short-count
divergences) and D87 (kernel heap exhaustion: charged buffers, low
watermark, fallible allocation). `scripts/test-sysfuzz.sh [aarch64]
[x86_64]` runs testapps/sysfuzz per arch (logs in `out/test/sysfuzz/`):
`cases` (deterministic argument combinations, each checked against the
errno Linux returns, the Linux source function cited next to each),
`fuzz-s<seed>-{1,4}cpu` (random syscalls from the arch table with random
and edge arguments — NULL, unmapped, guard and kernel-half addresses, huge
lengths, 32-bit-truncated and negative fds, random flags — from locked
threads; fuzz.go documents the exclusion list; only "the kernel survived
and the program exited 0" is asserted) and the `sigreturn-*` / `lastpage`
crash modes (hand-built rt_sigreturn frames: accepted ones run
`exit_group(42)`, rejected ones die by SIGSEGV; a syscall from the last
user page). A kernel panic, `[patina] fatal: user`, timeout or missing exit
sentinel fails a case.

| Name | Command | Observed |
|---|---|---|
| **test-sysfuzz** | `scripts/test-sysfuzz.sh aarch64 x86_64` (seeds 1 2 3, 20000 calls each at 1 vCPU/2 workers and 4 vCPUs/4 workers) | all passed: aarch64 17/17 (`PASS sysfuzz cases (327)`, 6 fuzz runs, 9 sigreturn modes, lastpage by signal), x86_64 20/20 (`PASS sysfuzz cases (343)`, 6 fuzz runs, 12 sigreturn modes, lastpage 43: mmap refused). Fuzz also survived seeds 4–14 at 4 vCPUs on aarch64 during development. Before the fixes: the cases run panicked the kernel at the 16 TiB `mincore` (`memory allocation of 4294967296 bytes failed`), with it skipped 36 cases failed on aarch64 / 37 on x86_64 (listed below), `sigreturn-tf` hung (test-frame bug, fixed in the test) and `lastpage` mapped the top page on x86_64; every fuzz seed survived the unfixed kernel |
| **exhaust** | Worker-side throwaway program at `PATINA_MEM=256` (aarch64): `RLIMIT_NOFILE` raised to 1048576, then pipes written once, UDP sockets, listening TCP sockets, epolls, and tmpfs writes after committing most of RAM | before D87 each panicked the kernel (`memory allocation of N bytes failed`); now `write` ENOMEM after 2875 pipes, ENOBUFS after 2787 UDP sockets, ENFILE at 4096 listeners, ENOMEM after 734186 epolls, ENOSPC for tmpfs; each exits 0. A `sysfuzz -mode exhaust` regression is reserved but not written (see the report) |
| **limits** | `scripts/test-sysfuzz.sh` rows `limits-1cpu`/`limits-4cpu` (`sysfuzz -mode limits` at `PATINA_MEM=256`): `RLIMIT_NOFILE` raised to 1048576, then pipes (written once), UDP sockets, epolls, 64 MiB writable mappings and tmpfs writes until each is refused; every refusal must be a clean errno and `getpid` must still answer; all released; `-limits-strict` (`SYSFUZZ_LIMITS_STRICT=1`) additionally requires MemFree back within 8 MiB of baseline | aarch64: pipes ENOMEM after 2866, UDP ENOBUFS after 2778, epolls ENOMEM after 681936, mappings ENOMEM after 2×64 MiB, tmpfs ENOSPC after 125 MiB, exit 0 at 1 and 4 vCPUs; x86_64: 2849 / 2762 / 684933 / 2×64 MiB / 125 MiB, same errnos. After release MemFree is 6760 kB (aarch64) / 7716 kB (x86_64) against a ~245 MB baseline: ~237 MiB stays in the kernel heap (D87), so strict mode is off until heap frames are returned |
| **p5-sysfuzz-merge** | on the merged branch: `scripts/test-sysfuzz.sh aarch64 x86_64`, `PATINA_CPUS_LIST="1 4" scripts/test-runtime.sh aarch64 x86_64`, `scripts/test-gopkg.sh runtime --cpus 4`, `scripts/test-gopkg.sh os net syscall`, `scripts/test-net.sh`, `scripts/test-ctl.sh`, `scripts/test-robust.sh` on both arches, `scripts/test-vz.sh`, host tests, clippy both targets | all PASS: test-runtime `all passed` (22 rows per arch), `runtime aarch64 (cpus=4): 385 passed, 141 skipped (11 by our list), 0 failed`, `runtime x86_64 (cpus=4): 386 passed, 143 skipped (11 by our list), 0 failed`, os 248/33(10)/0, net 254/55(1)/0, syscall 25/28(10)/0 on both arches, test-net 7/7 per arch, test-ctl 8/8 per arch, test-robust 12/12 per arch, test-vz 13/13 (incl. vsock), 233 host tests, clippy 0 warnings |

Bugs fixed (one commit each; the cases that caught them, as they failed on
the unfixed kernel):

- `mincore` over a huge `PROT_NONE` reservation allocated len/4096 bytes
  with IRQs off under the address-space lock and panicked the kernel; now
  a bounded chunked walk (`mincore 16 TiB into 2 MiB`).
- `read`/`readv`/`pread64`/`getdents64`/`recvfrom`/`recvmsg`/`epoll_wait`
  consumed pipe/socket data or edge-triggered events before the copy-out
  failed with EFAULT (`read huge count`, `read into hole`, `read into
  read-only page`, `read after faults keeps data`, `pipe empty after
  failed writev`, `epoll_pwait kernel events before fd`); `write`/`pwrite64`
  filled the pipe ring before noticing a count beyond the user half
  (`write huge count`); partial buffers now give Linux's short count
  (`pread64/pwrite64/getrandom up to the guard page`, `read /dev/zero up
  to the guard page`).
- iovec/msghdr import: unchecked segments and the wrong errnos
  (`writev kernel segment`, `writev huge segment`, `sendmsg iovlen 1025`
  → EMSGSIZE).
- Linux errno order: `pread64 negative offset bad fd`, `epoll_ctl bad
  event before fd`, `ppoll bad sigsetsize first`, `prlimit64 kernel new
  first`, `futex wait bad timeout before alignment`, `mprotect len 0 kernel
  addr`, `mprotect growsdown on plain vma`, `madvise kernel` → ENOMEM,
  `madvise len 0`, `utimensat fd NULL path with flags`.
- Argument checks: `mmap unknown prot bit` (ignored), `mremap new_len >
  TASK_SIZE` → EINVAL, `timer_settime NULL new` → EINVAL, `prlimit64
  NOFILE above nr_open` → EPERM, `futex wake kernel` → EFAULT, `msync bad
  flags` / `async|sync` / `unaligned` / `hole`, `mlock hole`, `mlockall 0`,
  `mlock2 bad flags`, `eventfd2 bad flags`; x86_64 user space now ends at
  `TASK_SIZE_MAX` (`lastpage` → mmap refused).
- Kernel heap exhaustion (D87): socket buffers, pipe rings, AF_UNIX
  queues, netlink replies and tmpfs pages are charged against the D78
  commit limit; object-creating syscalls check a low watermark; every
  user-sized buffer is allocated fallibly; VMAs capped at 65530; the fd
  table fills in linear time (**exhaust**).
- Ephemeral TCP/UDP ports were identical on every boot (the port table was
  seeded before the RNG); the first inet `socket()` reseeds it.

- [x] Deterministic Linux-errno syscall cases on both arches — **test-sysfuzz** `cases`
- [x] Random-argument syscall stress survives at 1 and 4 vCPUs — **test-sysfuzz** `fuzz-*`
- [x] Malformed `rt_sigreturn` frames and a syscall from the last user page — **test-sysfuzz** `sigreturn-*`, `lastpage`
- [x] Kernel heap exhaustion answers with errnos, never a panic — **limits** (in-tree: `sysfuzz -mode limits`, both arches, 1 and 4 vCPUs); **exhaust** was the earlier worker-side program
- [~] Memory returns after release: `sysfuzz -mode limits -limits-strict` fails today (~237 MiB held after release, D87); flips to verified when heap frames are returned to the frame allocator

