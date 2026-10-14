# Decisions and deviations

Choices not spelled out in `doc/patina-design.md`, or that deviate from it.
One entry per decision, appended in order. Format:

```
## D<n>: <title>
*<YYYY-MM-DD> · Phase <n>*

<one paragraph: what, and why>
```

## D1: Shared memory-layout constants; position-independent aarch64 load
*2026-10-14 · Phase 0*

`PHYS_OFFSET = 0xffff_8000_0000_0000` (direct map) and
`KERNEL_VA_BASE = 0xffff_ffff_8000_0000` (kernel link address) have the same
values on both arches so the arch-neutral `mm` code (`kernel/src/mm/mod.rs`)
can share them. x86_64 needs the kernel in the top 2 GiB for the `kernel`
code model; aarch64 simply reuses the numbers. The aarch64 `Image` header
uses `text_offset = 0` and flags = LE | 4K | placement-anywhere (0xa), so the
VMM loads it at any 2 MiB aligned address. The kernel is linked at
`KERNEL_VA_BASE`; the boot code finds its physical load address
PC-relatively and maps `KERNEL_VA_BASE` to it. If the image is not 2 MiB
aligned the boot code hangs on purpose (there is no console yet to report
it), rather than running with a broken mapping.

## D2: TTBR0 reserved for user mode
*2026-10-14 · Phase 0*

On aarch64 TTBR0 holds only a boot-time identity map of the image, needed
to survive the instruction that turns the MMU on. Once the kernel runs in
the higher half and the final tables are built, TTBR0 is pointed at an
empty table ("TTBR0 parked" in the boot log). All kernel mappings live in
TTBR1, so TTBR0 is free for the user address space in Phase 1.

## D3: Heap = `buddy_system_allocator::Heap` behind the in-tree IRQ-safe SpinLock
*2026-10-14 · Phase 0*

The kernel heap (`kernel/src/mm/heap.rs`) wraps `buddy_system_allocator`'s
`Heap<32>` in `crate::sync::SpinLock`, which masks local interrupts while
held, instead of using the crate's `LockedHeap` (needs the `use_spin`
feature and is not IRQ-safe, so an allocation in an interrupt handler could
deadlock). The heap starts at 8 MiB (smaller on tiny VMs) and grows on
demand: on OOM it pulls at least 2 MiB (a power of two big enough for the
request) from the frame allocator and retries once. Lock order is
heap → frames; the frame allocator never allocates from the heap.

## D4: In-tree buddy frame allocator
*2026-10-14 · Phase 0*

Physical frames come from an in-tree binary buddy allocator
(`kernel/src/mm/frame.rs`) rather than the crate's `FrameAllocator`, which
stores its free sets in heap collections and so needs a heap before the heap
can exist. Free lists are intrusive (the nodes live in the free frames,
reached through the direct map), and 1 byte of metadata per frame (carved
from the first usable RAM region large enough) makes the buddy check O(1).
Orders run up to `MAX_ORDER = 18` (1 GiB blocks).

## D5: Kernel uses FP/SIMD on aarch64
*2026-10-14 · Phase 0*

The pinned target `aarch64-unknown-none` allows rustc to emit FP/SIMD
instructions in kernel code (memcpy, struct copies, formatting), so boot
sets `CPACR_EL1.FPEN` (and clears the CPTR_EL2 traps when entered at EL2).
Consequence for Phase 1: user FP/SIMD state must be saved and restored on
every kernel entry, not lazily on context switch, or the kernel must move to
`aarch64-unknown-none-softfloat`, which is not in the pinned toolchain today.
Tracked as a Phase 1 prerequisite in `doc/status.md`.

## D6: SCTLR_EL1 user-facing bits and PAN set at boot
*2026-10-14 · Phase 0*

`SCTLR_EL1` is set with DZE (EL0 `dc zva`, used by Go's `memclr`), UCT (EL0
`CTR_EL0` reads), nTWI and nTWE (EL0 `WFI`/`WFE` not trapped) when the MMU
is enabled, so Phase 1 does not have to revisit it. PAN is turned on
(SPAN cleared, `PSTATE.PAN` set) by `cpu::enable_pan` after checking
`ID_AA64MMFR1_EL1.PAN`, per design §4; the boot log reports `PAN on` or
`PAN unavailable`.

## D7: Kernel image mapped with 4 KiB pages; writable alias via the direct map
*2026-10-14 · Phase 0*

The final tables (`paging::init_kernel_tables`) map the image with 4 KiB
pages and per-section permissions: `.text` R-X, `.rodata` read-only,
`.data`/`.bss`/boot stack RW + XN, and the page below the boot stack
(`__stack_guard`) unmapped as a guard. The direct map covers all RAM as
Normal RW + XN, which includes the kernel image, so a writable (but
non-executable) alias of `.text` exists through `PHYS_OFFSET`. Accepted for
Phase 0; carving the image out of the direct map can come later.

## D8: Boot page-table pools and the DTB window
*2026-10-14 · Phase 0*

The asm boot code builds its tables from a 16-page pool in `.bss.boot_pt`
(`BOOT_PT_PAGES`); Rust-side tables use a 64-page static pool
(`paging::POOL_PAGES`) until `frame::init` runs (11 pages used on QEMU
`virt` with 512 MiB). The boot tables use 2 MiB blocks: the image as RWX
(identity in TTBR0, `KERNEL_VA_BASE` in TTBR1) and two RW direct-map blocks
starting at the 2 MiB block containing `x0`, so the DTB is readable at
`PHYS_OFFSET + x0` wherever the VMM placed it (mid-RAM on QEMU, end of RAM
on Firecracker), before the memory map is known.

## D9: `map_device` mappings are replayed into the final tables
*2026-10-14 · Phase 0*

The console UART is mapped with `paging::map_device` while still on the
boot tables. Every device mapping (up to `MAX_DEVICE_MAPS = 32`) is
recorded and replayed into the final tables by `init_kernel_tables`, so
MMIO mapped early stays valid across the switch and callers never need to
know which tables were live.

## D10: Console selection on aarch64
*2026-10-14 · Phase 0*

`console=ttyAMA*` selects a PL011 and `console=ttyS*` a 16550 (preferring
the DTB `stdout-path` node if it is of that type, else the first UART node
of that type). `console=hvc0` (virtio-console, not driven yet), no
`console=`, or a type absent from the DTB falls back to the DTB
`stdout-path`, then to the first UART node. This extends design §3.3
("stdout-path on arm64") so the CLI's `console=` is honored on arm64 too.
Two `[patina] early:` lines (console choice; final page tables / PAN) print
before the canonical banner.

## D11: Phase 0 cmdline parser treats raw `k=v` after `--` as kernel params
*2026-10-14 · Phase 0*

Per `doc/cli-contract.md`, `scripts/run-qemu.sh` always emits
`console=<dev> patina.epoch=<secs> [params] -- [args]`. The in-kernel parser
(`kernel/src/cmdline.rs`) is a Phase 0 stand-in: tokens before the first
`--` are kernel params, and raw `key=value` tokens after it are also params
(the CLI percent-encodes `=` in real args, and microvm appends
`virtio_mmio.device=` after `--`), which is why
`run-qemu.sh aarch64 -- patina.selftest=panic` works. Args are
percent-decoded and argv[0] is `/app`. In Phase 1 the kernel switches to
the host-tested `crates/cmdline` (`patina-cmdline`).

## D12: QEMU device set for Phase 0
*2026-10-14 · Phase 0*

`scripts/run-qemu.sh` uses the CLI contract's base flags
(`-nodefaults -no-user-config -display none -no-reboot
-chardev stdio,id=con0,signal=off -serial chardev:con0`, `-M virt,gic-version=3`
on aarch64, `-M microvm` + `isa-debug-exit` on x86_64). The virtio-serial
`patina.ctl` port and `virtio-rng-device` from the contract are left out
until Phase 3, when the kernel has virtio drivers to use them.

## D13: run-qemu exit status comes from the serial sentinel
*2026-10-14 · Phase 0*

`run-qemu.sh` always derives its exit status from the last
`[patina] exit code N` serial line, never from QEMU's status (PSCI
SYSTEM_OFF exits QEMU with 0; `isa-debug-exit` cannot report 0). A timeout
returns 124 even if the sentinel was printed, so a poweroff that never
happens stays visible; 125 means QEMU ended without a sentinel.

## D14: PSCI conduit from the DTB
*2026-10-14 · Phase 0*

PSCI calls use HVC by default and switch to SMC when the DTB `/psci` node's
`method` is `"smc"` (QEMU `virt` with `virtualization=on`, i.e. when the
kernel is entered at EL2 and HVC would trap to itself). Implemented in
`kernel/src/arch/aarch64/power.rs` (about 40 lines) instead of the `smccc`
crate.

## D15: No dedicated exception stack yet
*2026-10-14 · Phase 0*

The aarch64 exception handlers run on SP_EL1, the same stack as the
interrupted kernel code. A kernel stack overflow therefore faults again
inside the vector and recurses instead of producing a clean register dump.
Design §4 calls for per-CPU exception stacks; this lands in Phase 1/2
together with per-thread kernel stacks. Tracked in `doc/status.md`.

## D16: Workspace glob members
*2026-10-14 · Phase 0*

The root `Cargo.toml` uses `members = ["kernel", "crates/*"]` and
`default-members = ["crates/*"]`, so a bare `cargo build`/`cargo test`
builds only the host-testable crates and the kernel is built explicitly per
target. cargo 1.99 refuses to load a workspace whose member glob matches
nothing, which was the case until the first crate under `crates/` landed.
Now moot; recorded so the glob is not mistaken for the cause if it recurs.

## D17: Boot self-test hooks stay in the kernel
*2026-10-14 · Phase 0*

`patina.selftest=panic|fault|alloc` (`kernel/src/selftest.rs`) let the
smoke harness exercise the panic handler, the exception path with register
dump, and the frame/heap allocators on a real boot. They cost a few hundred
bytes and run only when requested, so they stay in release builds; they
are the named tests in `doc/status.md`.

## D18: Kernel panic exits with code 255
*2026-10-14 · Phase 0*

A kernel panic prints `[patina] panic: <message> at <file>:<line>`, then
`[patina] exit code 255`, and powers off (`panic::PANIC_EXIT_CODE`).
Design §10.2 reserves 255 for kernel panics; the `{"ev":"panic"}`
control-channel event is added in Phase 3 when the channel exists.

## D19: x86_64 kernel window maps physical 0, image linked at +2 MiB
*2026-10-14 · Phase 0*

On x86_64 the top-2 GiB window is Linux-style: `KERNEL_VA_BASE + pa → pa`
for the first GiB, the image is linked at `KERNEL_VA_BASE + 0x20_0000` and
loaded at physical 0x20_0000 (the ELF `p_paddr` is physical, `p_vaddr` the
high VA, so PVH loaders in QEMU/Firecracker/Cloud Hypervisor place it
correctly). `arch::kernel_load_phys()` therefore returns the fixed
`KERNEL_LOAD_PHYS = 0x20_0000`, whereas on aarch64 `KERNEL_VA_BASE` maps the
(variable) load address itself; `mm::kernel_virt_to_phys` has a per-arch
`cfg` for this. The ELF `e_entry` is unused; the `.note.Xen`
`XEN_ELFNOTE_PHYS32_ENTRY` note carries the physical 32-bit entry
(`_start32`), stored as an 8-byte desc like Linux (QEMU reads a u64,
linux-loader a u32).

## D20: x86_64 QEMU machine is `microvm`; `pc`/`q35` via `PATINA_MACHINE`
*2026-10-14 · Phase 0*

PVH boot works on `-M microvm` with QEMU 11.1.2, so the design's choice
stands and no fallback to `pc` was needed. `scripts/run-qemu.sh` takes
`PATINA_MACHINE=pc|q35` as an override (both boot and exit cleanly); `-cpu
host` is used only when `PATINA_ACCEL` is `kvm`/`hvf`, otherwise `-cpu max`.
`microvm` has no ACPI PM port and no i8042, so without `isa-debug-exit` a
guest cannot power itself off; the script always adds the device.

## D21: x86_64 mapping granularity and direct-map coverage
*2026-10-14 · Phase 0*

The kernel image is mapped with 4 KiB pages (`.text` R-X, `.rodata` and the
note R+NX, data/bss/stack RW+NX, boot-stack guard page unmapped). The direct
map uses 2 MiB pages where aligned and 4 KiB at the edges, RW+NX+global; 1 GiB
pages are not used because not every VMM CPU model advertises them. It covers
memmap RAM, ACPI and NVS entries, the whole low 1 MiB (RSDP scan, future AP
trampoline) and the initrd range — the latter explicitly because QEMU/qboot
places the initrd's last page inside an e820 "reserved" entry. After
`frame::init` all 256 kernel-half PML4 slots are pre-populated so Phase 1 user
address spaces can copy the upper half. The boot-time identity map of the low
4 GiB lives only in the asm boot tables and is dropped at the CR3 switch in
`early_main`, before `frame::init`; no Rust code relies on it. `map_device`
calls made before the final tables exist only record ranges below 4 GiB
(already reachable through the boot direct map) and are replayed uncached
(PCD|PWT) into the final tables; LAPIC/IOAPIC (0xfee00000/0xfec00000) are not
in the direct map and must be mapped with `map_device`.

## D22: `hvm_start_info` handling
*2026-10-14 · Phase 0*

The cmdline (NUL-terminated, copied into a static 4 KiB buffer because the
heap does not exist yet), module 0 (the initrd), `rsdp_paddr` and the memmap
(start_info v1+) are read through the boot direct map; structures above 4 GiB
are rejected. A v0 start_info or one with no memmap entries panics with
"PVH start_info has no memmap ...: cannot discover RAM" instead of guessing
RAM (untested; some Firecracker versions may need an e820/CMOS fallback
later). On this QEMU `rsdp_paddr` is 0 on `microvm`, `pc` and `q35`, so a
later phase must scan 0xE0000–0xFFFFF for the RSDP; `microvm`'s memmap has no
ACPI-type entries.

## D23: x86_64 poweroff order
*2026-10-14 · Phase 0*

`arch::power::poweroff` tries, in order: (1) `isa-debug-exit` — probe
`inb(0xf4) != 0xff` (QEMU's device reads as 0, an absent port as 0xff), then
`outb(0xf4, code)`; QEMU exits with status `(code << 1) | 1`, which the
scripts ignore in favour of the serial sentinel; (2) ACPI PM1a S5 for
`pc`-class machines, `outw(0x604, 0x2000)`, with no table parsing; (3) i8042
reset `outb(0x64, 0xfe)`, which is how Firecracker terminates a guest; (4)
`cli; hlt` loop. Cloud Hypervisor's ACPI shutdown (needs FADT/DSDT) and
`microvm`'s GED sleep register are out of scope. The port-I/O `asm!` blocks
are deliberately not `nomem` so they act as compiler barriers.

## D24: x86_64 GDT/TSS/IDT layout fixed for Phase 1
*2026-10-14 · Phase 0*

Selectors: 0x08 kernel code64, 0x10 kernel data, 0x18 user code32 (DPL3, the
`SYSRET` base), 0x20 user data (DPL3), 0x28 user code64 (DPL3), 0x30 TSS
(16-byte descriptor); `STAR` should be `[47:32] = 0x08`, `[63:48] = 0x18|3`.
The TSS is static with `iomap_base = 104`, `rsp0 = 0` (set per thread in
Phase 1) and IST1 = a 16 KiB static double-fault stack; only `#DF` uses an
IST. The IDT has 256 gates, 0–31 present (DPL0, selector 0x08), 32–255 not
present; `TrapFrame` is 176 bytes and an `iretq` return path exists. CR4 =
PAE|PGE|OSFXSR|OSXMMEXCPT, EFER = LME|NXE, CR0 = PE|MP|NE|WP|PG;
`flush_tlb_all` toggles CR4.PGE on the local CPU only. `cpu::id()` returns 0
until per-CPU GS lands.

## D25: ABI layout check is generated from Linux 7.2.10 UAPI headers (§13)

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

## D26: `FUTEX_CMD_MASK` keeps the pre-7.2 value

Linux 7.2 added `FUTEX_ROBUST_UNLOCK` (512) and `FUTEX_ROBUST_LIST32` (1024)
and strips them in `FUTEX_CMD_MASK`. patina does not implement them, so its
mask still clears only `FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME`: an op that
carries a robust flag decodes to an unknown command and gets `ENOSYS`, as it
would on Linux < 7.2, instead of silently running the base op. Go does not use
these flags. The layout check compares the mask with the 7.2 value minus the
two flags.

## D27: Arch/core split for user mode: `trap.rs` hooks and the `ctx`/`uspace`/`timer` contract
*2026-10-14 · Phase 1*

The arch layer owns entry/exit assembly, the `TrapFrame` layout, user page
tables, context switch and the timer; it calls five hooks in
`kernel/src/trap.rs` (`syscall`, `user_fault`, `user_exception`,
`timer_tick`, `before_return_to_user`) and implements the interface
documented in `kernel/src/arch/mod.rs` (`ctx::{TrapFrame, FpState,
KernelContext, switch, enter_user}`, `uspace::UserTables`, `timer::{init,
now_ns}`, `cpu::{set_kernel_stack, hwcap}`). The arch-neutral core
(syscalls, address space, scheduler) lives entirely behind those hooks, so
both arches were built in parallel against `patina.selftest=user|userfault|
timer|switch` before any loader existed. Hook interrupt state is part of
the contract: `syscall`/`user_fault`/`user_exception` run with IRQs
enabled, `timer_tick` and `before_return_to_user` with IRQs masked. The
kernel never dereferences a user virtual address (PAN on aarch64, SMAP on
x86_64 would fault): all user memory goes through the address space and the
direct map, so a kernel-mode fault is always a bug and stays fatal.

## D28: aarch64 user FP/SIMD state is saved eagerly in the trap frame
*2026-10-14 · Phase 1*

Resolves D5. The kernel keeps the NEON-capable `aarch64-unknown-none`
target (the soft-float target is not in the pinned toolchain), so every
entry from EL0 saves q0–q31, FPCR and FPSR into the 832-byte `TrapFrame`
on the kernel stack and every return to EL0 restores them. `FpState` is a
zero-sized marker on aarch64 and `fp_save`/`fp_restore` are no-ops;
`KernelContext` also saves d8–d15 (AAPCS64 callee-saved) because kernel
code uses SIMD across `switch`. Cost is ~70 instructions per entry; a lazy
scheme (trap on first FP use) can replace it later without changing the
contract. x86_64 is the opposite: `x86_64-unknown-none` is soft-float, so
user FP state is only saved on context switch, with XSAVE (XCR0 =
x87|SSE|AVX, 832 bytes in a 1024-byte 64-aligned `FpState`) when
CPUID advertises XSAVE+AVX and FXSAVE otherwise; AVX-512 is never enabled
in XCR0, so Go will not use it. Consequence: on x86_64 the scheduler must
call `fp_save`/`fp_restore` around every thread switch.

## D29: aarch64 kernel stack overflow runs on a per-CPU overflow stack (closes D15)
*2026-10-14 · Phase 1*

The EL1h sync vector checks the stack before touching it (TPIDRRO_EL0 is
used as scratch, which is safe because EL1 sync is always fatal): if `sp`
is not 16-byte aligned or any of three `AT S1E1W` probes at `sp-16`,
`sp-4096` and `sp-8192` fails, it switches to a 16 KiB per-CPU static
overflow stack (`cpu::MAX_CPUS = 8`) and dumps from there. Three probes are
needed because a one-page guard below a page-granular stack is passed
through 832 bytes at a time; a single probe at `sp-8K` landed in mapped
memory below the guard and recursed. Kernel stacks therefore need no
alignment, only an unmapped guard page, which rules out carving them from
the 2 MiB-block direct map: thread stacks get their own 4 KiB-mapped VA
range. x86_64 keeps IST1 for `#DF`.

## D30: GICv3 with a GICv2 fallback; virtual timer PPI 27 at 100 Hz
*2026-10-14 · Phase 1*

The aarch64 tick uses the EL1 virtual timer (CNTV_*) at `trap::TICK_HZ`
(100 Hz), re-armed in the handler, with the distributor/redistributor
bases taken from the DTB (`arm,gic-v3`; `arm,cortex-a15-gic` falls back to
a GICv2 driver so `-M virt,gic-version=2` under TCG still works — HVF only
supports GICv3). `timer::early_init` (boot) sets CNTKCTL_EL1.EL0VCTEN so
EL0 can read CNTVCT (Go's `cputicks`). `irq::register(intid, handler)` is
the hook for Phase 2 SGIs/IPIs and device IRQs; `gic::init_cpu` finds a
CPU's redistributor by MPIDR for AP bring-up.

## D31: EL0 `MRS` of ID registers is emulated; `HWCAP_CPUID` advertised
*2026-10-14 · Phase 1*

Go's arm64 runtime reads `MIDR_EL1` from user mode when `AT_HWCAP` has
`HWCAP_CPUID` (as Linux sets). patina advertises it and emulates the
resulting EC 0x18 trap in `exceptions::emulate_mrs`: `MIDR_EL1` and
`ID_AA64ISAR0_EL1` are passed through, `ID_AA64PFR0_EL1` is sanitised to the
FP/AdvSIMD/GIC fields Linux exposes, and every other register in the ID
group reads as zero (unknown encodings are an illegal instruction). On CPUs
without FEAT_IDST the access arrives as EC 0x00 and is treated as an
illegal instruction. `AT_HWCAP` itself is
`patina_abi::aarch64::hwcap::from_id_aa64isar0` plus FP/ASIMD/CPUID.

## D32: x86_64 user-mode CPU setup
*2026-10-14 · Phase 1*

SYSCALL/SYSRET with `swapgs` and a per-CPU block reached through
`KERNEL_GS_BASE` (`percpu.rs`: self pointer, kernel stack top, user rsp
scratch, cpu id); STAR = kernel CS 0x08 / SYSRET base 0x18|3 (user data
0x20, user code64 0x28 per D24), SFMASK = IF|TF|DF|AC|NT. `sysretq` is used
only when the frame is safe (user CS/SS, rcx == rip, r11 == rflags,
canonical rip, TF/RF clear), otherwise `iretq`; a fresh `enter_user` frame
always takes `iretq`. FSGSBASE is enabled when CPUID has it (QEMU `-cpu
max` does) and `AT_HWCAP2` gets `HWCAP2_FSGSBASE`; else the FS_BASE MSR is
used. The FS base lives in the `TrapFrame` and is read/written on every
entry/exit (uniform `tls()`/`set_tls()`; Phase 2 may skip unchanged
values). SMEP and SMAP are enabled when available. Interrupts are enabled
inside syscalls and user exceptions (after CR2 is read). `AT_HWCAP` is
CPUID.1:EDX as Linux's `ELF_HWCAP`.

## D33: x86_64 timer: xAPIC periodic timer calibrated against PIT channel 0
*2026-10-14 · Phase 1*

The LAPIC is driven as xAPIC MMIO at `IA32_APIC_BASE` (x2APIC MSRs only if
firmware already enabled it), timer vector 0xec in periodic mode (divide
16), spurious vector 0xff, LINT0/error LVTs masked, IOAPIC untouched. Both
the LAPIC timer and the TSC are calibrated in one ~10 ms window on PIT
channel 0 (mode 0, latch reads): channel 2 cannot be used on `microvm`
(no port 0x61 gate). TCG measures TSC ≈ 1 GHz. CPUID 0x15 returns zeros
under TCG, so that fallback is untested. The 8259 PICs are remapped to
0x20/0x28 and fully masked at boot: `microvm` leaves them uninitialised
(vector base 0), so the PIT's IRQ 0 arrived as `#DE` the first time IRQs
were enabled. The tick handler sends EOI before calling `trap::timer_tick`
so the hook may switch threads.

## D34: `PROT_NONE` user pages keep their frame in an invalid descriptor
*2026-10-14 · Phase 1*

`UserTables::protect` to no permissions writes an invalid descriptor that
still carries the frame address plus a software bit, so `translate` still
reports the frame (flags = `USER` only), `unmap` returns it, and a later
`protect` back to R/W/X restores the mapping without a new frame. Accesses
fault as `NotPresent`. User page tables are never freed (one process per
VM), everything runs under ASID 0 / without PCID, and `activate` does a full
TLB flush; Phase 2 may add ASIDs.

## D35: `AT_RANDOM` and `getrandom` come from a non-cryptographic PRNG until virtio-rng
*2026-10-14 · Phase 1*

There is no entropy source before the virtio-rng driver (Phase 3).
`kernel/src/random.rs` is a SplitMix64 generator seeded from the boot
counter and `patina.epoch`, with the clock mixed in on every call; it feeds
the 16 `AT_RANDOM` bytes (Go's hash seeds) and `getrandom`. It is NOT
cryptographic: `crypto/rand` output from a Phase 1 kernel must not be
trusted. Phase 3 replaces the generator's input with virtio-rng and keeps
the interface.

## D36: User address-space layout and mmap policy
*2026-10-14 · Phase 1*

One `AddressSpace` (`kernel/src/mm/vm.rs`): an ordered VMA map (merged when
adjacent and identical), a `BTreeSet` of resident pages, and the arch
`UserTables`. ELF segments are filled eagerly (design §4); everything else
is lazy zero-fill on first touch. The main stack is an 8 MiB VMA ending at
`0x7fff_f000_0000`; anonymous mappings without a usable hint are placed
top-down below `MMAP_BASE = 0x7f00_0000_0000`, never below
`MMAP_MIN_ADDR = 0x10000`; a hint is honoured when the range is free and
page-aligned (Go's `0x00c0_0000_0000 | i << 32` arena hints), and
`MAP_FIXED` replaces whatever is there. Only anonymous mappings exist
(`MAP_SHARED|MAP_ANONYMOUS` is treated as private: there is no fork);
file mappings get `ENODEV`. `PROT_NONE` reservations cost no frames;
`madvise(DONTNEED|FREE)` frees frames and keeps the VMA; `HUGEPAGE`,
`NOHUGEPAGE`, `COLLAPSE`, `RANDOM`, `NORMAL` are accepted and ignored;
`mremap` supports in-place growth and `MREMAP_MAYMOVE` (frames are
re-mapped, not copied), `MREMAP_FIXED`/`DONTUNMAP` → `EINVAL`; `brk`
returns the fixed break (end of the highest ELF segment) like the raw Linux
syscall; `mincore` reports real residency. The resident-page set makes
munmap/mprotect/madvise over Go's multi-GiB `PROT_NONE` reservations cost
only the pages actually present. The kernel reaches user memory only
through `AddressSpace::{copy_from_user, copy_to_user, read_val, write_val,
read_cstr, zero_user}`, which fault pages in like a user access and copy
through the direct map (D27).

## D37: Phase 1 syscall-layer conventions
*2026-10-14 · Phase 1*

`kernel/src/syscall/` dispatches on `patina_abi::current::nr` constants;
handlers return `Result<isize, Errno>`. Unknown numbers get `ENOSYS` and one
`[patina] ENOSYS <nr> (<name>)` line per number (design §7);
`patina.strace=1` prints `[strace] <tid> <name>(<args>) = <ret>` inline
(it interleaves with the program's output). Phase 1 stubs that Go's startup
tolerates: every path lookup is `ENOENT` (logged once per path; the VFS is
Phase 3), fds 0–2 are the console (`write` 1/2, `read` 0 → EOF,
`fstat`/`newfstatat(AT_EMPTY_PATH)` report a character device,
`fcntl(F_GETFL)` → `O_RDWR`, `ioctl` → `ENOTTY`), `uname` reports
`Linux 6.6.0-patina`, `prlimit64` answers NOFILE 1024/1048576 and STACK
8 MiB, `sched_getaffinity` reports the vCPUs the scheduler runs on (one in
Phase 1), CPU-time clocks return time since boot, `kill`/`tgkill` to a
valid target return 0 and log once per signal number that delivery is
Phase 2 (Go's sysmon sends `SIGURG` for async preemption and would print
errors on `ENOSYS`), `rt_sigaction`/`rt_sigprocmask`/`sigaltstack` store
state exactly (KILL/STOP rejected, mask never blocks them) but nothing is
delivered, and `rt_sigreturn` is fatal (exit 139). Kernel-mode user memory
access never touches user VAs (D27, D36).

## D38: Uniprocessor scheduler with per-CPU structure; kernel is not preemptible
*2026-10-14 · Phase 1*

`kernel/src/task/sched.rs` keeps one `PerCpu` (current thread, round-robin
`VecDeque` runqueue, idle context = the boot stack, zombie slot,
`need_resched`, quantum) reached through `sched::this_cpu()`; Phase 2
indexes it by `cpu::id()`. The 100 Hz tick (`trap::timer_tick`) never
switches directly: it expires sleepers, counts the 2-tick (20 ms) quantum
and sets `need_resched`; the switch happens in `trap::before_return_to_user`,
on block, yield or exit, or at an explicit `preempt_point`. Thread state,
wake reason and a per-block sequence number share one atomic word so
`try_wake(seq, reason)` is a single CAS and stale/double wake-ups are
harmless — the property SMP wakers need. The idle loop runs with IRQs
masked: x86 `sti; hlt; cli` (sti shadow), aarch64 `wfi` relying on WFI
waking for a pending-but-masked IRQ (as Linux's `cpu_do_idle`), verified on
HVF and TCG. FP state is saved into the outgoing thread and restored for
the incoming one only if it has run before; fresh threads load their start
state in the entry trampoline; a clone child gets `fp_save` of the parent's
live registers (no-op on aarch64, where the frame carries FP). Signal
delivery (Phase 2) hooks into `before_return_to_user` after the reschedule
and needs a third `WakeReason` for `EINTR`.

## D39: Kernel stacks: 32 KiB + guard page in a dedicated window; thread 0 is a normal thread
*2026-10-14 · Phase 1*

`task/kstack.rs` hands out 32 KiB stacks with one unmapped guard page below
each, mapped page-by-page with `arch::paging::map_range` in the window at
`KSTACK_BASE = 0xffff_a000_0000_0000` (1 TiB; PML4 slot 320 on x86_64,
pre-populated by `populate_kernel_pml4` so every user PML4 sees it; TTBR1 on
aarch64). Slots come from a bump pointer and freed slots go to a free list
with their frames still mapped (no kernel unmap/TLB shootdown needed; the
frames are not returned to the allocator). Thread 0 gets a slot like every
other thread; the boot stack becomes CPU 0's idle context. A thread cannot
free the stack it runs on, so a dead thread's stack is released by the next
context that runs (`PerCpu` zombie slot). `patina.selftest=kstack` overflows
a thread stack: aarch64 reports it on the overflow stack (D29), x86_64 as
`#DF` on IST1.

## D40: Wait queues, sleep list and futexes
*2026-10-14 · Phase 1*

`wait::block(guard, queue, key, bits, deadline)` enqueues the current thread
under the caller's lock, releases that lock with IRQs still masked
(`SpinLockGuard::unlock_irqs_masked`) and then schedules, so no wake-up can
be lost between the check and the sleep. The sleep list (`task/sleep.rs`)
is ordered by deadline and expired from the tick, so `nanosleep`,
`clock_nanosleep` and futex timeouts have 10 ms resolution in Phase 1
(Go's sysmon `usleep(20)` sleeps a whole tick); Phase 2 adds one-shot
deadlines per CPU and hangs `timer_create`/`setitimer` off the same list.
Futexes (`task/futex.rs`) are 64 buckets keyed by user virtual address (one
address space); `FUTEX_WAIT`/`WAIT_BITSET` read the word through uaccess
under the bucket lock, `FUTEX_WAKE`/`WAKE_BITSET` wake up to `val`
(`val <= 0` wakes one, as Linux), `FUTEX_CLOCK_REALTIME` only with
`WAIT_BITSET`, bitset 0 or a misaligned address → `EINVAL`, requeue/PI ops →
`ENOSYS` (D26). Lock order: futex bucket → address space → frame allocator;
bucket → sleep list → runqueue.

## D41: `clone` accepts Go's flag set only; thread exit semantics
*2026-10-14 · Phase 1*

`clone` (`syscall/thread.rs`) requires exactly
`CLONE_VM|FS|FILES|SIGHAND|SYSVSEM|THREAD` plus any of
`SETTLS|PARENT_SETTID|CHILD_SETTID|CHILD_CLEARTID|DETACHED`; anything else
is `EINVAL` (design §5). Go passes `0x50f00` on arm64 and `0xd0f00`
(+`CLONE_SETTLS`, the FS base) on amd64; argument order comes from
`patina_abi::current::clone_args`. The child is the parent's `TrapFrame`
with return value 0, the given stack and TLS, a copied signal mask, a
disabled sigaltstack, and the parent's FP state; tids start at 2;
`clone3` → `ENOSYS`; a `SETTLS` value outside user space → `EPERM`
(a non-canonical FS base would fault the kernel's `wrfsbase`). `exit`
zeroes `clear_child_tid`, futex-wakes it, and if no live user thread
remains ends the process with that code; `exit_group` ends the process at
once (Phase 2 must stop the other CPUs first). `tgkill` validates the tid
against the thread table and still only logs (delivery is Phase 2), so Go's
`SIGURG` async preemption is a no-op in Phase 1.

## D42: Signal delivery model
*2026-10-14 · Phase 2*

`kernel/src/task/signal.rs` (replaces the store-only signal state of
D37/D41). Handlers are `Process::sigactions`. The process-wide pending set
is the statics `SHARED` (a `PendingQueue` in a `SpinLock`) and its bit
mirror `SHARED_BITS`, not a `Process` field, because there is one process
per VM. Each thread has `sigq` plus the bit mirror `sig_pending`, and
`Thread::blocked` (an `AtomicU64` written only by the thread itself)
replaces the Phase 1 `SigState.mask`. The mirrors are written under the
queue lock and read without it, so `deliver` (called from
`trap::before_return_to_user` after the reschedule, IRQs masked) returns
after a handful of relaxed atomic loads when nothing is pending: that is the
SIGURG async-preemption path. Standard signals coalesce; RT signals queue
one `SigInfo` each, up to `MAX_QUEUED = 1024` per queue. A signal whose
disposition ignores it (`SIG_IGN`, or `SIG_DFL` with default action
ignore/continue: CHLD, URG, WINCH, CONT) is dropped at send time unless the
target blocks it; for process signals, unless every user thread blocks it
(Linux `sig_ignored`). Dequeue order: thread-private synchronous signals
(SEGV/BUS/ILL/TRAP/FPE/SYS) first, then the lowest private one, then the
lowest shared one. At most one handler frame is built per return to user
mode. SIGKILL always terminates (137). `SIG_DFL` STOP/TSTP/TTIN/TTOU are
treated as ignored: there is no job control in a one-process VM. Unlike
Linux, pid 1 is not `SIGNAL_UNKILLABLE`: design §6 asks for `SIG_DFL`
termination, so a terminating signal prints `[patina] exit signal NAME`
before the `[patina] exit code 128+signo` sentinel (`crate::exit_signal`).
That name is also what the future control-channel `exit` event's `signal`
field will carry. `kill` queues on the process with `SI_USER` (`si_pid` =
the pid, 1); `tgkill`/`tkill` queue on the thread with `SI_TKILL`. A write
that fails with `EPIPE` sends SIGPIPE to the writing thread
(`syscall/fs.rs`, as Linux `send_sig`). `rt_sigtimedwait`, `rt_sigqueueinfo`
and `rt_tgsigqueueinfo` are `ENOSYS` (Go does not use them). The API is
`send_to_thread`, `send_to_tid`, `send_to_process` and
`force_fault`/`force_sig`. `send_to_process` kicks a running thread that
does not block the signal first, then one in an interruptible wait, then any
(Linux `complete_signal`). Lock rules: `sigactions` and `sigq`/`SHARED` are
leaf locks, taken one at a time. The `THREADS` lock is held only to clone
the thread list (`task::threads()`), never while kicking. Delivery runs with
IRQs masked and takes the address-space lock to write the frame.

## D43: Fault → signal mapping
*2026-10-14 · Phase 2*

User faults and exceptions become forced signals on the faulting thread
(`trap::user_fault` / `trap::user_exception` → `signal::force_fault`, as
Linux `force_sig_fault`). If the thread blocks the signal or the process
ignores it, the disposition is reset to `SIG_DFL` and the signal is
unblocked, so it terminates. A `SIG_DFL` fault, or any `SI_KERNEL` signal,
keeps the Phase 1 diagnostics: `[patina] fatal: SIGSEGV (si_code …, addr …)
at pc …`, the register dump and the VMA table, then `exit signal` and exit
128+signo. A page fault the address space cannot resolve is SIGSEGV
`SEGV_MAPERR` when no VMA covers the address and `SEGV_ACCERR` when one
does. x86_64: `#UD` → SIGILL `ILL_ILLOPN` (si_addr = rip); `int3` → SIGTRAP
`SI_KERNEL` with rip after the `int3`; `#DE` → SIGFPE `FPE_INTDIV` at rip;
`#GP` → SIGSEGV `SI_KERNEL`; `#SS` → SIGBUS `SI_KERNEL`; `#DB` → SIGTRAP
`TRAP_TRACE`; `#AC` → SIGBUS `BUS_ADRALN` (no address); any other user
vector → SIGILL. aarch64: undefined instruction (EC 0x00/0x0e, and an `MRS`
that is not emulated, D31) → SIGILL `ILL_ILLOPC` at pc; `BRK` → SIGTRAP
`TRAP_BRKPT` with pc at the `BRK`; data, PC and SP alignment faults → SIGBUS
`BUS_ADRALN`; any other EC → SIGILL. FP exception traps (x86 `#MF`/`#XM`,
arm64 EC 0x2c) are SIGFPE `FPE_FLTINV` on both arches: Linux decodes FPSR or
the x87/MXCSR status to pick the exact code, and patina does not (Go never
unmasks FP exceptions). The frame gets the hardware details through
`arch::sigframe::fault_info` / `exception_info` (x86 `cr2`/`err`/`trapno`;
arm64 `fault_address` and `esr_context`). One consequence: Go's
`runtime.Breakpoint` ends as Go's own throw (`SIGTRAP: trace trap`, exit 2),
not exit 133, because Go has a handler installed and SIGTRAP is `_SigThrow`
in Go's sigtable.

## D44: Signal frames and `sigaltstack`
*2026-10-14 · Phase 2*

x86_64 (`arch/x86_64/sigframe.rs`) builds `rt_sigframe` as Linux
`arch/x86/kernel/signal_64.c` does. Below the 128-byte red zone (or at the
top of the alternate stack) comes the fpstate, 64-byte aligned, then
`{pretcode, ucontext, siginfo}` with `rsp ≡ 8 (mod 16)`, all written with
one `copy_to_user`. `uc_flags = UC_FP_XSTATE | UC_SIGCONTEXT_SS |
UC_STRICT_RESTORE_SS`. The fpstate is the live XSAVE image with
`fpx_sw_bytes` (`xfeatures` = XCR0 = x87|SSE|AVX, `xstate_size` from CPUID
0xD.0:EBX), `XSTATE_BV |= FP|SSE`, and `FP_XSTATE_MAGIC2` after the state. A
CPU without XSAVE gets a plain 512-byte FXSAVE image and no `UC_FP_XSTATE`;
this is untested because QEMU `-cpu max` has XSAVE. The handler starts with
the FPU in its init state. `rt_sigreturn` falls back to an FXSAVE-only
restore when the software bytes or magic2 are bad. It treats an MXCSR with
reserved bits set as a bad frame, takes only the `FIX_EFLAGS` bits of
RFLAGS, forces user CS/SS and sets `orig_rax = -1`; a non-canonical rip is
also a bad frame. Every bad frame forces SIGSEGV. A handler without
`SA_RESTORER` cannot be delivered on x86_64: frame setup fails and SIGSEGV
is forced, with a log line. Go always sets `SA_RESTORER` on amd64. aarch64
(`arch/aarch64/sigframe.rs`): `{siginfo, ucontext}` is 16-aligned with an
AAPCS64 frame record above it. `__reserved` always holds `fpsimd_context`
(taken from the trap frame, D28), plus `esr_context` for forced fault and
exception signals (`fault_address` = FAR for aborts, 0 otherwise), then the
terminator. `rt_sigreturn` requires a 16-aligned sp, PSTATE = EL0t (AArch64)
with DAIF clear, and well-formed records. **Deviation:** Go does not set
`SA_RESTORER` on arm64; it relies on the vDSO's `__kernel_rt_sigreturn`.
patina has no vDSO, so on the first such delivery the kernel maps one R-X
page at `SIGTRAMP_HINT = 0x7fff_f000_0000` (just above the main stack, D36;
elsewhere if that address is taken) holding `mov x8, #139; svc #0`. It
cleans and invalidates the caches for that page and puts the page's address
in x30. `sigaltstack` follows Linux `do_sigaltstack`. The raw flags are
stored (`SS_AUTODISARM` kept); it returns `EPERM` while running on the stack
and `ENOMEM` below `MINSIGSTKSZ`; `oss` flags are computed from the current
sp. `SS_AUTODISARM` is honoured: the stack is disabled once a frame is
placed on it. `rt_sigreturn` restores `uc_stack` and ignores errors. The
arch contract `arch::sigframe::{setup_frame, restore_frame, rewind_syscall,
fault_info, exception_info}` is documented in `kernel/src/arch/mod.rs`.

## D45: Syscall interruption and restart
*2026-10-14 · Phase 2*

This adds the third `WakeReason`, `Interrupted` (D38). The thread's sched
word packs a 2-bit wake reason and an "interruptible" bit next to the state
and block sequence. Both that word and the pending bits are accessed SeqCst,
so either the sender sees the blocker as interruptible (and `sched::kick`
wakes it), or the blocker sees the pending bit before it sleeps.
`wait::block_interruptible` and `sleep_until_interruptible` take the same
arguments as `block` and `sleep_until`, which stay non-interruptible.
Blocking syscalls return Linux's internal codes. `ERESTARTSYS` (512)
restarts only if the handler has `SA_RESTART`; pipe and console read/write
return it (a partial transfer still returns the byte count), as does futex
wait without a timeout. `ERESTARTNOHAND` (514) is `EINTR` if a handler runs
and is re-executed otherwise; `ppoll`/`poll`, `rt_sigsuspend`, `pause` and
absolute `clock_nanosleep` return it. `ERESTART_RESTARTBLOCK` (516) plus
`Thread::restart_block` is continued through `restart_syscall`; `nanosleep`,
relative `clock_nanosleep` and futex wait with a timeout return it.
`ERESTARTNOINTR` (513) is handled, but nothing returns it yet. `epoll_pwait`
returns plain `EINTR` and is never restarted, as on Linux.

`syscall::dispatch` records the code in `Thread::restart`, and `deliver`
resolves it exactly like Linux's arm64 `do_signal`. With a handler it
returns EINTR or rewinds; without one, every code rewinds.
`sigframe::rewind_syscall` moves pc back over `svc`/`syscall`, restores
`orig_x0`/`orig_rax`, and for 516 sets x8/rax to `restart_syscall`.
`nanosleep` writes `rem`. `ppoll` and `poll` restart with the original
relative timeout (Linux keeps the deadline in a restart block), and `ppoll`
never writes the remaining time back. A sigmask passed to `epoll_pwait` or
`ppoll` (and `rt_sigsuspend`'s mask) is installed with `set_temp_mask`. If
the wait is interrupted, the temporary mask stays until the handler runs,
and the frame's `uc_sigmask` is the original mask (`Thread::saved_mask`,
Linux `saved_sigmask`). Otherwise the original is restored on return. Codes
512–516 never reach user mode, although `patina.strace=1` shows them
(`-errno 512`). `sched::kick` wakes an interruptible blocker; for a thread
running on another CPU it calls `sched::kick_remote`, the SMP hook (D50).

## D46: fd table and the `File` trait
*2026-10-14 · Phase 2*

Design §8 puts pipes, eventfd and epoll in Phase 3. They landed in Phase 2
because Go's netpoller needs them for timers (D49). The module docs of
`kernel/src/fs/file.rs` are the contract that Phase 3's VFS files, `/dev`
nodes and sockets implement. There is one `File` per open file description.
`OpenFile` adds the status flags (access mode, `O_NONBLOCK`, `O_APPEND`) and
is shared by `dup`/`F_DUPFD`, so `F_SETFL` is seen through duplicates; it
changes only NONBLOCK and APPEND. The fd table (`fs/fdtable.rs`) maps fd →
(`Arc<OpenFile>`, cloexec). It allocates the lowest free fd, bounded by the
live `RLIMIT_NOFILE` soft limit (`proc::nofile_limit()`). `OpenFile`s, and
so `File::release`, are dropped only after the fd-table lock is released and
never under the VFS lock (the patina-vfs `Handle` rule). File operations
never block: the syscall layer retries after `wait_ready`. That registers a
`PollWaiter` via `poll(Some(hook))` under the file's lock and blocks
interruptibly unless the file is already ready. Files keep a `PollList` of
`Weak<dyn PollHook>`; they call `collect` under their lock and `fire()`
after releasing it. Epoll never holds its own lock while calling into a
file. Lock order: fd table → (released) → file → leaf; epoll lock → wait
queue / sleep list; `PollWaiter` → sleep list. The pure parts (fd slot
allocation, pipe ring, eventfd counter) are in the in-tree, host-tested
crate `crates/fsutil`, which has no third-party dependencies.

Pipes use a 64 KiB ring allocated on the first write; writes of up to
`PIPE_BUF` (4096) bytes are all or `EAGAIN`; the write end is `OUT` while at
least 4096 bytes are free and `ERR` with no readers, the read end `HUP` with
no writers; `FIONREAD` works and `O_DIRECT` → `EINVAL`. `fstat`: pipes are
`S_IFIFO|0600`, eventfd and epoll `S_IFREG|0600` anonymous inodes, the
console `S_IFCHR|0620` with rdev 5:1; fds 1 and 2 share one `O_RDWR` console
description and fd 0 is a read-only one at EOF. Epoll (`fs/epoll.rs`) keys
items by (fd, `OpenFile` address) and holds a `Weak<OpenFile>`, so an item
goes away when its description is released (pruned lazily). Level-triggered
items are re-queued after each report, edge-triggered ones wait for the next
notification, `EPOLLONESHOT` clears the event bits until `EPOLL_CTL_MOD`,
and ERR/HUP are always in the mask. Epoll deviations: an epoll fd inside an
epoll → `EINVAL` (Linux allows nesting), `EPOLLEXCLUSIVE` is accepted but
every waiter is woken, and `poll()` on an epoll fd reports `IN` whenever its
ready list is non-empty (may be a false positive). `pselect6` is `ENOSYS`
(logged); `membarrier`, `set_robust_list` and `rseq` are `ENOSYS` without a
log line. `readv` reads once into a bounce buffer of at most 64 KiB and
scatters it; `writev` writes each iovec in turn (atomic per iovec only).
Timeouts are tick-granular and never early until sub-tick deadlines reach
the sleep list (D51). Syscalls added: `pipe2`/`pipe`, `eventfd2`/`eventfd`,
`epoll_create1`/`epoll_create`/`epoll_ctl`/`epoll_pwait`/`epoll_pwait2`/
`epoll_wait`, `dup`/`dup2`/`dup3`, `F_DUPFD(_CLOEXEC)`, `F_GETFD`/`F_SETFD`,
`ppoll`/`poll`, `readv`/`writev`, `pread64`/`pwrite64`/`lseek` (`ESPIPE` on
pipes), and `ioctl` `FIONBIO`/`FIONREAD`/`FIOCLEX`/`FIONCLEX`. The non-`2`
variants and `dup2`/`poll`/`epoll_wait` exist on x86_64 only.

## D47: aarch64 SMP bring-up, IPIs and per-CPU timer
*2026-10-14 · Phase 2*

`arch/aarch64/{smp,boot,gic,timer}.rs`. CPUs come from DT `/cpus`, using
enabled nodes with `enable-method = "psci"` or no enable-method (spin-table
CPUs are skipped). The boot CPU (by MPIDR) is logical CPU 0, the rest follow
in DT order, capped at `MAX_CPUS = 8`. CPU_ON is fid 0xC4000003 for
`arm,psci-0.2`/`1.0` nodes, else the node's `cpu_on` property. Bring-up is
serial through the single `AP_BOOT` slot (cpu id, TTBR1 root, the empty
TTBR0 table, stack top). The slot is cleaned to PoC with `dc cvac; dsb sy`
before `CPU_ON(mpidr, PA of __ap_entry, PA of AP_BOOT)`. PSCI errors are
logged by name and that CPU is skipped. A CPU that is not online within 1 s
stops further bring-up, because a late CPU would read a rewritten slot. AP
stacks come from `task::kstack::alloc()` and are never freed, except after a
failed CPU_ON. `__ap_entry` shares `__el2_to_el1` and `__setup_mair_tcr`
with the boot CPU. It sets TPIDR_EL1 (the cpu id) before the MMU is on, so
the overflow-stack path (D29) works. It turns the MMU on with the boot
identity map (kept intact in `.bss.boot_pt`) in TTBR0, jumps to the link
address, parks TTBR0 on the empty table and sets SP and VBAR. Then
`ap_rust_entry` enables PAN and EL0 counter access, runs `gic::init_cpu` and
`timer::init_cpu`, sets the online bit and calls `entry(cpu)` with IRQs
masked.

IPIs are SGI 0 (Reschedule) and SGI 1 (Stop). GICv3 sends through
`ICC_SGI1R_EL1` by affinity: TargetList = 1 << (Aff0 % 16), RS = Aff0 / 16
(RS ≠ 0 is untested). GICv2 sends through `GICD_SGIR` with the target's
CPU-interface mask, which each CPU reads from its banked `GICD_ITARGETSR0`;
SGIs stay Group 0 there. `send_ipi_others` loops over the online CPUs (no
broadcast). GICv2 EOI must write back the raw IAR, which carries the SGI
source CPU in bits 12:10, so `gic::ack` returns `(intid, token)`. Timer:
each CPU keeps `next_tick` on a fixed `start + k*period` grid (missed ticks
are skipped; no drift) and an optional one-shot `deadline`. CNTV_CVAL holds
the earlier of the two, and both expiries call `trap::timer_tick`.
`cpu::hw_random` uses RNDR when `ID_AA64ISAR0_EL1.RNDR` reports it (TCG
`-cpu max`: yes; HVF on this M-series host: none). Unverified: SMC on real
firmware, spin-table CPUs, more than 8 CPUs.

## D48: x86_64 SMP enumeration, AP trampoline, IPIs and LAPIC one-shot timer
*2026-10-14 · Phase 2*

`arch/x86_64/{smp,acpi,fwcfg,gdt,percpu,lapic,timer}.rs`. Enumeration runs
in `early_main` on the boot page tables, before `frame::init`, because an
e820 map may report the ACPI table pages as RAM. The first source in this
chain that yields CPUs wins: the MADT via `start_info.rsdp_paddr` → via an
RSDP scan (first KiB of the EBDA, then 0xE0000–0xFFFFF) → via fw_cfg
`etc/acpi/rsdp` + `etc/acpi/tables` (table pointers read as offsets into the
blob, checksums skipped, a 64 KiB static buffer; pc/q35 pad the blob to 128
KiB) → an MP table scan (EBDA, top KiB of base memory, 0x9FC00, 0xF0000) →
fw_cfg `NB_CPUS` (APIC ids assumed 0..n) → one CPU. The test knob
`patina.smp.skip=N` skips the first N sources. MADT types 0 and 9 are read,
enabled entries only, deduplicated and capped at `MAX_CPUS = 32`; CPU 0 is
the boot CPU by CPUID APIC id. Observed: QEMU `microvm` passes `rsdp_paddr`
0, but qboot installs ACPI and the BIOS-area scan finds the RSDP at 0xf5560
(pc/q35 SeaBIOS: 0xf52e0). SeaBIOS/qboot MP tables list one processor per
package, so a single-socket `-smp N` would report 1 CPU. That is why fw_cfg
comes before the MP table (Firecracker lists every CPU). The trampoline
lives at fixed low pages. Code and data are at 0x8000 (SIPI vector 8). Its
own page tables at 0x9000/0xA000/0xB000 identity-map 0–2 MiB (one executable
2 MiB page) and copy the kernel PML4's upper 256 entries. The path is 16-bit
→ 32-bit → long mode (CR4 PAE|PGE|OSFXSR|OSXMMEXCPT, EFER LME|NXE, CR0 WP) →
`__ap_entry64` → kernel CR3 + AP stack → `ap_main`. These pages are safe to
use because the whole low 1 MiB stays reserved.

For each AP the BSP pre-allocates a 32 KiB guarded kernel stack, an IST1
stack, a heap GDT/TSS (leaked) and a heap `PerCpu`. It then sends INIT
assert, waits 10 ms, sends INIT deassert (xAPIC only), sends two SIPIs 200
µs apart, and waits up to 1 s for the online flag. Bring-up is serialised; a
timeout stops it, since the AP may still be using the trampoline. xAPIC ids
above 254 are skipped, and MTRRs are not copied. `cpu::init_features` and
`entry::init_syscall` run unchanged on each AP; the self-test asserts that
CR4 and EFER equal the BSP's. Vectors: timer 0xec, Reschedule 0xf0, Stop
0xf1, spurious 0xff. IPIs use fixed delivery to the physical APIC id (xAPIC
ICR high/low with bounded delivery-status spins; x2APIC MSR 0x830), with
IRQs masked around the send. Timer (replaces D33's periodic mode): the LAPIC
timer is one-shot on every CPU (no TSC-deadline mode). Each interrupt
re-arms it for min(next 100 Hz grid point, deadline). `trap::TICKS` counts
deadline interrupts too. `set_deadline(None)` only clears the deadline: the
interrupt already armed still fires and re-arms. `cpu::hw_random` is RDSEED
with retries, else RDRAND. Unverified: x2APIC, KVM/Firecracker/Cloud
Hypervisor, APIC ids above 254, calling `start_secondaries` twice, MTRRs.

## D49: Phase 2 test programs and scripts
*2026-10-14 · Phase 2*

`scripts/test-hello.sh --app <name> [arch...]` builds `testapps/<name>` and
passes when it prints `PASS <name>` and exits 0; `PATINA_PARAMS` adds kernel
params (e.g. `patina.strace=1`), and `--threads` remains as the mode that
only shows output. `scripts/test-signals.sh [arch...]` runs six cases per
arch: `signals -self`, `signals -mode nohandler -self` (exit 143),
`sigstress`, `sigstress -procs 2`, `sigstress -crash` and `sigstress
-breakpoint` (exit 2 each). `testapps/fdtest` covers pipes, dup/fcntl,
`EMFILE`, eventfd, epoll (LT/ET/ONESHOT/HUP/auto-removal), ppoll and Go
timers. `testapps/sigstress` covers SIGURG async preemption (GOMAXPROCS
spinners with no function calls, so main runs only if they are preempted);
nil dereference and integer divide by zero, each recovered 200 times;
`-crash` (unrecovered SIGSEGV, Go's `code=0x1 addr=0x0` line) and
`-breakpoint`; `kill` and `tgkill` SIGUSR1, counted (1000 and 100);
`nanosleep` interrupted with `EINTR` and `rem`; a blocking pipe read
restarted across signals (`SA_RESTART`); and `time.Sleep` under a SIGUSR1
storm. `signals -self` busy-waits instead of sleeping because it was written
before the netpoller worked; that is harmless and keeps it independent of
epoll.

Background: Go's runtime initialises its netpoller (`epoll_create1`,
`eventfd2`, `epoll_ctl`) on first timer use. That is why Phase 1's threads
test died at `netpollinit`.

## D50: SMP scheduler core
*2026-10-14 · Phase 2*

`kernel/src/task/sched.rs`; its module docs are the contract. This closes
the Phase 2 items of D38–D41. `static CPUS: [PerCpu; MAX_CPUS]` is indexed
by `cpu::id()`. Each `PerCpu` holds the current thread, a FIFO runqueue (a
`SpinLock` plus an atomic length that stealers read without the lock), the
idle context (the boot stack on CPU 0, the AP's entry stack elsewhere), a
`prev` hand-off slot, `need_resched`, `slice_start`, `run_start` and the
one-shot deadline it has armed. The kernel stays non-preemptible, so a
thread changes CPU only across `schedule()`, i.e. across anything that
blocks. `this_cpu()` is therefore stable in IRQ-masked code and within a
syscall until it first blocks; code after a block re-reads it. Bring-up:
`start_thread0` activates the user tables and calls `sched::run_smp(t0)`.
That runs `timer::init` and `arch::smp::start_secondaries(ap_main)`, waits
up to 100 ms for the last AP's `ONLINE` bit, prints `[patina] smp: N cpus
online`, queues thread 0 on CPU 0 and becomes CPU 0's idle loop. `ap_main`
activates the user tables on its CPU (`activate_user_tables`, when a process
exists), sets its `ONLINE` bit and enters the idle loop. `sched::run()`,
used by the kernel self-tests, starts no secondaries and stays on CPU 0.

Placement: `wake` and `make_runnable` queue the thread on the waker's CPU,
with one exception. A waker that is running a thread claims an idle CPU if
there is one (the thread's last CPU if it is idle, else the lowest idle one)
by clearing its bit in `IDLE` with `fetch_and`, queues the thread there and
sends that CPU a `Reschedule` IPI. A waker in its idle loop's context (a
tick on an idle CPU) keeps the thread local. New threads follow the same
rule, so Go's Ms spread over idle CPUs. A CPU whose runqueue is empty sets
its `IDLE` bit and re-checks its queue (a waker either sees the bit or
enqueued before the re-check). It then steals from the longest other
runqueue, with `try_lock` only, taking the oldest thread whose `on_cpu` is
clear. On contention, or when only still-switching threads are queued there,
it retries up to 10 000 times and then waits in `cpu::idle_wait`.

The `on_cpu` protocol (as Linux): a thread that blocks on CPU A can be woken
by CPU B before A has saved its context. The CPU that switches a thread in
sets `on_cpu`. `finish_switch`, run on the new stack after every switch,
takes the outgoing thread from `PerCpu::prev` and clears it. A waker whose
`try_wake` CAS succeeded spins (IRQs masked) until `on_cpu` is clear and
then enqueues the thread exactly once. The spin is in the waker and not at
switch-in because spinning at switch-in can deadlock when two CPUs each pop
the other's outgoing thread. `schedule()` requeues `prev` only if it is
still `Running` (preemption, yield), and only on its own runqueue. A dead
thread's `Arc` sits in `prev` across the switch away from it;
`finish_switch` on that CPU frees its kernel stack and drops it (this
replaces D39's zombie slot). `clone` (D41) now writes
`PARENT_SETTID`/`CHILD_SETTID` before `sched::make_runnable(child)`, and
`spawn_user_thread` no longer enqueues, so a child running on another CPU
cannot read its tid word first. `kick_remote(t)` (D45) sends a `Reschedule`
IPI to `t`'s CPU if `t` is `Running` on another CPU; it reads `Thread::cpu`
after the state. It runs after the sender's SeqCst pending-bit store, so a
thread that migrated meanwhile went through a switch and checks the bits on
its way back to user mode. `sched_getaffinity` returns
`sched::online_mask()`, so `runtime.NumCPU()` and GOMAXPROCS equal the vCPU
count. With more than one CPU online, `patina.strace=1` prints `[strace]
<tid>@<cpu>`.

Time: the slice is `QUANTUM_NS` = 20 ms, measured with `timer::now_ns`
rather than by counting ticks, because a one-shot deadline interrupt also
calls `trap::timer_tick`. `Thread::cpu_time_ns` is accumulated at every
context switch (user and system time together).
`sched::cpu_time_ns(&Thread)` is the accounted part, `current_cpu_time_ns()`
adds the caller's running slice, and `thread_cpu_time_ns(&Thread)` adds any
thread's running slice lock-free (`t.cpu` → `CPUS[c].run_start`, retried if
the thread switched meanwhile). A reader that hits the few instructions of a
switch-in can overcount once by the previous thread's slice. Deadlines: the
sleep list stays one global list (D40). `sleep::add` calls
`sched::arm_deadline(d)`, which arms this CPU's one-shot if `d` is earlier
than what it has armed. Every CPU's tick expires sleepers and re-arms its
one-shot to the global earliest deadline, so some CPU wakes at that
deadline: sleeps and timeouts are now sub-tick and never early (D46's
tick-granular timeouts are gone). Idle CPUs still take the 100 Hz periodic
tick; idle is not tickless. Measured on aarch64/HVF: 200 ×
`time.Sleep(500µs)` took 2.001 s before and 248 ms after (1, 2 and 4 CPUs;
x86_64/TCG 263–268 ms), and 10 000 sleeps from 8 goroutines went from 12.5 s
to about 1.5 s. The remaining floor of about 1.2 ms per sleep is Go's
netpoller, which waits in `epoll_pwait` with whole-millisecond timeouts.

Stop: `stop_other_cpus()` runs first in `exit_group`, `crate::exit`,
`crate::exit_signal` and the panic handler. The first caller becomes
`EXIT_OWNER`, sends `Stop` to every other CPU and waits up to 10 ms for the
`STOPPED` bits of the CPUs running the scheduler (`trap::ipi(Stop)` →
`sched::ipi_stop` sets the bit and halts). A later caller on another CPU
halts (`halt_this_cpu`, also counted as stopped); the owner calling again
returns at once. A CPU takes the IPI only with IRQs unmasked, and every
`SpinLock` masks IRQs, so a stopped CPU never holds a lock. The exit
sentinels are printed with `console::panic_write_fmt`, which force-unlocks
the console after a bounded wait. A panic on an AP stops the others, CPU 0
included. Lock order: futex bucket → address space → frame allocator;
wait-queue lock (futex bucket / file / `PollWaiter` / epoll) → sleep list →
per-CPU runqueue. No CPU holds two runqueue locks at once (the stealer only
`try_lock`s the victim's), and nobody waits on `on_cpu` while holding a
runqueue lock. `Thread::sig`/`sigq` and the console lock are leaves;
`task::signal` calls `sched::kick` only after dropping its queue lock.
`IDLE`, `ONLINE`, `STOPPED`, `on_cpu` and `need_resched` are atomics.

`random.rs`: `seed_boot` mixes up to four `cpu::hw_random()` words (RNDR on
aarch64, RDSEED else RDRAND on x86_64) into the SplitMix64 state together
with the boot counter and `patina.epoch`. It logs `[patina] random: seeded
from RNDR` (or `RDSEED/RDRAND`), or `... from boot counter only` when the
CPU has none. HVF `-cpu host` on this host has no RNDR, so it seeds from the
boot counter only; TCG `-cpu max` has RNDR and RDSEED. The generator is
still NOT cryptographic: D35 stands until virtio-rng. Unverified: Stop or
panic while another CPU is mid-print or stuck with IRQs masked (reasoned
only), more than 4 vCPUs under the scheduler (the arch self-test ran 8),
Firecracker and Cloud Hypervisor. On x86_64 the scheduler is not yet
memory-safe: frames freed by one CPU can still be reached through another
CPU's stale TLB entries (D52).

## D51: Timers and CPU clocks
*2026-10-14 · Phase 2*

`kernel/src/task/timers.rs` (model, expiry and lock order in its module
docs) and `syscall/time.rs`. There is one table per process: POSIX timers by
id plus the three itimers. Timer ids count from 0, skip ids in use and wrap
at `i32::MAX`, as Linux 6.x; more than `MAX_TIMERS` = 4096 timers is
`EAGAIN`. Every timer is an `Arm`: an absolute expiry in its own clock (0 =
disarmed) and a reload interval. Clocks: wall time (`now_ns`, plus
`patina.epoch` for `CLOCK_REALTIME`); a thread's CPU time (`Weak<Thread>`
read through `sched::thread_cpu_time_ns`, D50); and the process's CPU time,
the sum over live user threads plus `DEAD_CPU_NS`, which `exit` adds each
thread's time to, clamped so it never goes backwards. `resolve_clock`
accepts the static clock ids and the Linux-encoded per-thread and
per-process ids (`patina_abi::time::decode_cpuclock`). The thread must be a
live user thread of the process, and the process id must be 0 or the pid;
anything else is `EINVAL`. Timers reject `_RAW` and `_COARSE` clocks, as
Linux does.

Expiry runs from the tick on every CPU. `timers::tick()` is one `ARMED` load
while nothing is armed. Otherwise it takes `TABLE` with `try_lock`, so a CPU
that loses the race skips that tick. Periodic timers are re-armed by whole
intervals, and the skipped intervals become the overrun of the one signal
sent, as on Linux. The signals are collected under the lock and sent after
dropping it (`send_to_process` / `send_to_thread`). When a send is refused
because the timer's previous signal is still pending, the pass relocks and
adds 1 + missed to the timer's accumulated overrun, which the next queued
signal reports. `timer_settime` and `setitimer` run the same pass at once,
so an already-expired value fires immediately. Granularity is the 10 ms tick
plus any one-shot deadline tick: never early, up to one tick late. Timers do
not arm deadlines themselves; that is a follow-up through
`sched::arm_deadline`. (Done in D77: wall-clock timers are sub-tick deadlines;
D77 also fixed the overrun accounting described below.) Lock order: `TABLE` → `THREADS` (process CPU clock);
no signal is sent under `TABLE`.

Notifications: `SIGEV_SIGNAL` signals the process, `SIGEV_THREAD_ID` a live
user thread (else `EINVAL`), `SIGEV_NONE` nothing. A NULL `sevp` means
SIGALRM to the process with the timer id as value. `SIGEV_THREAD` and
unknown modes are `EINVAL`, and the signal must be in 1..=64. If writing the
id back fails, the timer is removed and the call returns `EFAULT`. POSIX
timers send `SI_TIMER` (timer id, overrun, sigval). Itimers send
`SI_KERNEL`: `ITIMER_REAL` → SIGALRM on the monotonic clock,
`ITIMER_VIRTUAL` → SIGVTALRM and `ITIMER_PROF` → SIGPROF, both on the
process CPU clock. `ITIMER_VIRTUAL` therefore counts total CPU time, since
there is no user/system split. Go's `validSIGPROF` accepts `SI_KERNEL` from
`setitimer` and `SI_TIMER` from its per-M
`timer_create(CLOCK_THREAD_CPUTIME_ID, SIGEV_THREAD_ID)`, which is why both
paths exist.

Syscalls: `timer_create`, `timer_settime` (with `TIMER_ABSTIME`),
`timer_gettime`, `timer_getoverrun`, `timer_delete`; `getitimer` and
`setitimer` (a NULL new value disarms); x86_64 `alarm` with Linux's
rounding; `getrusage` for `RUSAGE_SELF`, `RUSAGE_THREAD` and
`RUSAGE_CHILDREN` (`ru_utime` is the CPU time, every other field 0);
`times()` (all of it as user time). `clock_gettime` and `clock_getres` (1
ns) work on the CPU clocks; `clock_nanosleep` on a thread CPU clock is
`EINVAL` and on the process CPU clock `ENOTSUP`. Deviations:
`timer_getoverrun` returns the overrun recorded when the timer last fired
with a target, even if that signal was then refused (Linux: the last
delivered one). A timer on an RT signal can queue several copies, where
Linux keeps one entry and counts overruns. `timer_delete` leaves an
already-queued signal queued (as Linux), and a tick on another CPU that
already collected an expiry may still send one last signal. CPU timers of
exited threads are disarmed, and a signal for a `SIGEV_THREAD_ID` target
that has exited is dropped.

## D52: x86_64 TLB shootdown and deferred frame reclaim
*2026-10-14 · Phase 2*

x86_64 `UserTables::unmap`/`protect` only `invlpg` the local TLB, so once
D50 runs user threads on every vCPU a frame freed on one CPU could still
be written through another CPU's stale translation after the frame
allocator reused it (seen as a corrupted buddy free-list link in
`frame::alloc_frames`, `#GP` in kernel mode; `testapps/tlbstress`
reproduces it in one run). A synchronous shootdown that waits for acks
would deadlock: the address-space lock is an IRQ-masking spinlock and
other CPUs spin on it with IRQs masked (futex bucket → space, D40), so
they could never take the IPI. The fix (`kernel/src/mm/tlb.rs`) is
asynchronous with deferred reclaim, RCU-style: a global `FLUSH_GEN` and a
per-CPU `CPU_GEN[c]` = the newest generation CPU `c` has fully flushed
for. A vm operation that frees user frames (munmap, `MAP_FIXED` over
resident pages, `madvise(DONTNEED|FREE)`, mremap shrink/move) collects
them in one `Shootdown`; on drop it bumps `FLUSH_GEN` → g, queues
`(g, frames)` on `DEFERRED`, reloads CR3 locally, sets its own `CPU_GEN`
to g and sends `IpiKind::TlbFlush` (x86 vector 0xf2; aarch64 SGI 2,
handled but never sent) to the other online CPUs without waiting. The
receiver reads `FLUSH_GEN` *before* its full flush and `fetch_max`es it
into `CPU_GEN` after (reading after the flush could claim a batch whose
old entries were re-cached in between). `reclaim()` frees every batch with
`g <= min(CPU_GEN[c] for c in online_mask())`; it uses `try_lock`, first
brings a lagging caller up to date, and runs at the end of every vm
operation, from every CPU's tick and in `vm::fault_in` before reporting
ENOMEM (not from `frame::alloc`: the heap calls it with the heap lock
held and reclaim drops `Vec`s). Permission reductions (mprotect revoking
rights on resident pages, mremap move) send the IPI with nothing deferred;
lazy fills and permission upgrades send nothing; one IPI per syscall, not
per page. The arch contract gained `uspace::NEEDS_SHOOTDOWN` (true on
x86_64, false on aarch64 whose `tlbi …is` broadcasts synchronously, so
frames are freed at once there) and `uspace::flush_tlb_local()`. With
`online_cpus() <= 1` frames are freed immediately and no IPI is sent,
exactly as in Phase 1. Lock order: address space → `DEFERRED` → heap /
frame allocator. Cost per munmap/madvise at > 1 CPU: one SeqCst bump, one
short `DEFERRED` lock, one local CR3 reload, N−1 ICR writes and one CR3
reload per receiver; frees are delayed until the slowest CPU takes the
IPI (microseconds, at most until its next tick). Known limits: a stopped
CPU (exit/panic) never advances its `CPU_GEN`, so batches queued after a
stop stay pending (the `frames at exit` line can be slightly lower at > 1
CPU); the pending list is unbounded while some CPU keeps IRQs masked;
mprotect/munmap/mremap return before remote CPUs flush, so for a few
microseconds a remote thread can still write through the old entry into a
frame that is still held (lost write, no corruption), which Go does not
rely on. Verified only under QEMU TCG (x86_64) and HVF (aarch64).

## D53: VFS integration and locking
*2026-10-14 · Phase 3*

The root filesystem is the one `patina_vfs::Fs` behind a global IRQ-safe
`SpinLock` (`kernel/src/fs/vfs.rs`). `fs::rootfs::init` creates it and
unpacks the initrd with `populate_from_cpio_static`, so file pages stay
borrowed from the initrd until written. The loader then looks `/app` up
in the tree and takes its contents as one `&'static [u8]`
(`Fs::static_contents`, no copy). A malformed archive is still exit 127
"initrd is not a valid newc cpio archive". A missing `/app` is still 127,
and a non-regular `/app` 126. The file data limit is half of RAM (the
tmpfs default). It is enforced as `ENOSPC` and reported by `statfs`
(`TMPFS_MAGIC`, bsize 4096). Each syscall step does its whole
resolve-and-modify sequence in one critical section. That is what makes
`O_CREAT|O_EXCL`, `rename` and `O_APPEND` atomic. User memory is never
touched under the lock: paths are copied in first and results copied out
after. Outside a critical section an inode reference is a `VHandle`
(`Handle<KFs>`, where `KFs` is a ZST `FsRef` over the global). Code that
gets a reference under the lock (`Fs::open`, `get_ref`) returns the raw
inode number and wraps it with `Handle::adopt` after unlocking. No
`VHandle`, `Arc<OpenFile>` or `Arc<dyn File>` is dropped inside
`vfs::with`. Dirfds (`namei::Base`) and `/proc/self/fd` targets are held
as `Arc<OpenFile>` locals that outlive the closure.

The process cwd is a raw counted reference in `Process::cwd`, changed only
under the FS lock (`namei::set_cwd`), and `Process::umask` starts at 022.
The lock order is: fd table → (released); a file's own lock (a regular
file's offset) → FS → `Process::cwd` / heap. `File` grew additive default
methods: `getdents64`, `truncate`, `vnode` (the backing inode, for
`fchmod`/`fchdir`/dirfds), `is_opath` and `proc_name` (the
`/proc/self/fd` link text). Regular files (`vfile::RegFile`) are not
pollable (`EPERM` from `epoll_ctl`, always ready for `ppoll`) and give
`ENOTTY` for ioctls. `SEEK_DATA`/`SEEK_HOLE` treat the file as one data
extent, as tmpfs does. Deviation: `pwrite` on an `O_APPEND` description
writes at the offset given (Linux appends).

## D54: devfs, procfs-lite and generated files
*2026-10-14 · Phase 3*

Special nodes carry a `vfs::KSpecial(Arc<dyn DevNode>)`, which the kernel
gets back by downcasting through `Special::as_any`. `DevNode::open(ino,
flags)` builds a free-standing `File`. `namei` wraps it in a `NodeFile`,
so `fstat` reports the inode and `/proc/self/fd` the path. Character
devices without a handle (for example `mknod` entries in the initrd) are
bound by `rdev` at open time from a registry (`vfs::register_chrdev`).
`/dev` has `null` 1:3, `zero` 1:5, `full` 1:7, `random` 1:8 and `urandom`
1:9 (both read `random::fill`), `tty` 5:0 (`ENXIO`: there is no
controlling terminal) and `console` 5:1 (`fs::console::Console`). There
are also symlinks: `fd` → `/proc/self/fd`, and `stdin`/`stdout`/`stderr`
→ `/proc/self/fd/0..2`.

Generated files are `S_IFREG` 0444 special nodes whose content a closure
produces at open time into a read-only `MemFile` (size 0 in `stat`, like
procfs). This covers `/proc/1/{maps,status,stat,comm,cmdline,environ,
mounts}`, `/proc/{cpuinfo,meminfo,uptime,version,mounts,loadavg}`,
`/proc/sys/kernel/{hostname,osrelease,ostype,version,pid_max,threads-max,
random/boot_id,random/uuid}`, `/proc/sys/net/core/somaxconn` (4096, read
by `net.Listen`), `/sys/kernel/mm/transparent_hugepage/{hpage_pmd_size,
enabled}` and `/sys/devices/system/cpu/{online,possible}`.

The real nodes are symlinks and directories: `/proc/self` → `1`,
`/proc/1/exe` → `/app` and `/proc/1/root` → `/`. `/proc/1/cwd` is a
placeholder symlink that `chdir` retargets. Two places are "magic": the
path layer (`fs/namei.rs`) checks for them before normal resolution. It
repeats the walk with the new `Fs::lookup_parent` and follows final
symlinks itself, so `/dev/stdin`, `/dev/fd/N` and dirfd-relative names
all reach them. A name `N` in `/proc/1/fd` follows to the open file
itself (as Linux `proc_fd_link` does). For a VFS file it reopens the
inode with the new flags; anything else (pipe, eventfd) shares the open
file description, as `dup` does. Its `lstat` is a symlink and its
`readlink` is the open path, `pipe:[ino]`, `anon_inode:[eventfd]` and so
on. Opening the `/proc/1/fd` directory gives a generated listing
(`procfs::FdDirFile`). `/proc/1/cwd` as the final component gives the
live cwd. Deviations: deeper paths through an fd link
(`/proc/self/fd/3/x`) are `ENOENT`, `/proc/self/maps` shows ELF VMAs with
offset 0 and no `[heap]` (the break never moves), `VmRSS` is frames in
use system-wide, and `/proc/self/cgroup` and `mountinfo` are absent
(Go's cgroup-aware GOMAXPROCS then assumes no limit).

`/etc/{resolv.conf,hosts,hostname,nsswitch.conf,passwd,group}` are
created only if the initrd lacks them. `resolv.conf` uses the `ip=` DNS
fields, else `nameserver 10.0.2.3`. `hosts` has the localhost lines plus
`<ip> <hostname>` when `ip=` gives a client address. The base directories
`/tmp` (1777), `/root`, `/etc`, `/dev`, `/proc` and `/sys` always exist.

## D55: FIFOs
*2026-10-14 · Phase 3*

A FIFO inode (from `mknodat(S_IFIFO)` or the initrd) gets a `pipe::Fifo`
handle the first time it is opened. The FIFO keeps the
`Pipe` its ends share, plus Linux's `r_counter`/`w_counter`. Open follows
`fifo_open`. `O_RDONLY` waits until a writer has opened (not with
`O_NONBLOCK`). `O_WRONLY` waits for a reader, and with `O_NONBLOCK` it is
`ENXIO`. `O_RDWR` is one description that is both ends and never waits.
The wait is interruptible (`ERESTARTSYS`), and an interrupted open drops
the count it took. When every end has been closed, the next open starts
a fresh, empty pipe. The ends are the ordinary `PipeReader`/`PipeWriter`,
so `pipe2` semantics (atomic `PIPE_BUF` writes, `SIGPIPE`, readiness)
carry over unchanged.

## D56: path syscall conventions
*2026-10-14 · Phase 3*

Every path syscall decodes `(dirfd, path)` through `namei`. `AT_FDCWD`
and absolute paths use the cwd. Any other dirfd must be an open file with
a directory inode (`EBADF` / `ENOTDIR`); `O_PATH` fds qualify.
`AT_EMPTY_PATH` is honoured by `newfstatat`, `statx`, `linkat`,
`faccessat2`, `fchmodat2`, `fchownat` and `utimensat` (also implied by
`utimensat` with a NULL path). An empty path on a non-`AT_FDCWD` dirfd
reads the dirfd's own symlink for `readlinkat`. `O_PATH` descriptors only
support `fstat`, `fchdir`, `close`, `dup`, `fcntl` and use as a dirfd.
Everything else on them is `EBADF`. `O_TMPFILE` is `EOPNOTSUPP` and
`O_DIRECT` is `EINVAL` (tmpfs). `O_NOATIME`/`O_LARGEFILE`/`O_DSYNC`/
`O_SYNC` are accepted and ignored. The creation mode is masked with the
umask (`mkdir` keeps 01777 bits, files 07777).

There is a single user. Permission bits are stored but not enforced (as
for root), except `X_OK`. `chown` changes ids freely. `mknodat` creates
regular files and FIFOs; devices and sockets give `EPERM`. `statfs` and
`fstatfs` report tmpfs for every path. `fsync`, `fdatasync`, `syncfs`,
`sync`, `sync_file_range`, `flock` (validated, always granted) and
`fadvise64`/`readahead` are no-ops. `fallocate` mode 0 extends the file;
other modes are `EOPNOTSUPP`. `close_range` supports `CLOSE_RANGE_CLOEXEC`
(`UNSHARE` is a no-op). `copy_file_range`, `sendfile`, `splice`, `tee`
and `openat2` are `ENOSYS` without a log line. Go falls back to
read/write, and `os.Root` falls back to `openat` with `O_NOFOLLOW`.
`chroot` is `EPERM`. The x86_64 legacy calls (`open creat stat lstat
access readlink mkdir mknod rmdir unlink rename symlink link chmod chown
lchown utimes futimesat`) map onto the `*at` handlers. The old `getdents`
is `ENOSYS`. With `patina.strace` each path syscall also logs its path
argument. Deviations: an open file's `/proc/self/fd` name is the lexically
normalised path it was opened by, which `rename` does not update (except
for directories, whose name is computed live). Linux `xattr`s are not
supported (`EOPNOTSUPP`).

## D57: Tier-3 syscall stubs
*2026-10-14 · Phase 3*

Design §7's Tier-3 list is answered so that the `os` package tests and
typical programs see a plausible single-process root system instead of
`ENOSYS`. `wait4`/`waitid` return `ECHILD`. `execve`, `execveat`, `fork`
and `vfork` return `ENOSYS`, logged once per number like any unknown
call, because a program that needs them cannot work. `prctl` handles
`PR_SET_NAME`/`PR_GET_NAME` (the per-thread 16-byte `Thread::name`),
`PR_SET_DUMPABLE` (0/1) / `PR_GET_DUMPABLE` (1), `PR_SET_NO_NEW_PRIVS`,
`PR_GET_NO_NEW_PRIVS` (0) and `PR_SET_VMA` (accepted). Every other option
is `EINVAL`. The process is pid 1, its own group and session leader:
`getpgrp`/`getpgid`/`getsid` give 1, `setpgid` gives 0 and `setsid`
gives `EPERM`. Identity calls report uid/gid 0 everywhere: `getgroups` →
0 groups, `getresuid`/`getresgid` → 0,0,0, and every `set*id` and
`setgroups` call → 0 without changing anything. `capget` negotiates the
version and reports empty sets, and `capset` gives `EPERM`.
`sched_setaffinity` succeeds if the mask names an online CPU (no
migration; `EINVAL` otherwise). `sched_getscheduler` → `SCHED_OTHER`,
`sched_setscheduler`/`sched_setparam` → `EPERM`, `sched_getparam` →
priority 0, and `sched_get_priority_max`/`min` → 0. `getpriority` gives
20 (nice 0, raw syscall encoding) and `setpriority` gives 0.
`personality` gives 0. `sysinfo` fills in uptime, `totalram`/`freeram`
from the frame allocator, `procs` = live threads and `mem_unit` 1.
`getcpu` gives the current CPU, node 0. `mlock*`/`munlock*`/`msync` give
0. The `xattr` family gives `EOPNOTSUPP`. `inotify_*`, `memfd_create`,
`pidfd_open`, `kcmp`, `seccomp`, `*_mempolicy` and `ioprio_*` remain
`ENOSYS` (logged once). Go handles all of these.
## D58: virtio is in-tree behind a `Transport` trait; no `virtio-drivers`
*2026-10-14 · Phase 3*

`kernel/src/drivers/virtio/` (audit in doc/crates.md). Design §11 names
`virtio-drivers`; version 0.13.0 was read and not adopted: it brings
`safe-mmio`, `zerocopy`/`thiserror`/`enumn` with three proc-macro chains, its
console is single-port (the `patina.ctl` port needs the multiport control
queue), and our transmit paths must run with IRQs masked and from the
panic/exit path with bounded polling, which is easier to guarantee in about
600 lines we own. Its structure was kept: `virtio::Transport` is a trait
(device type, status, 64-bit features, queue setup with
desc/avail/used addresses, notify, read+ack interrupt status, config space
with generation retry) implemented by `mmio::MmioTransport` for legacy
(version 1: GuestPageSize, QueueAlign/QueuePFN) and modern (version 2:
QueueDesc/Driver/Device, QueueReady, FEATURES_OK checked,
`VIRTIO_F_VERSION_1` required) registers. QEMU 11 `virt` and `microvm` default
to legacy (`virtio-mmio.force-legacy=true`); modern was run with `-global
virtio-mmio.force-legacy=false` (aarch64: ready/ping/stop OK). `queue::VirtQueue`
is a split ring always laid out the legacy way (descriptors + avail from a
4 KiB boundary, used ring on the next page, ≤ 128 entries, 8 KiB), which
both versions accept, with a release fence before publishing `avail.idx`, an
acquire fence after reading `used.idx` and `arch::cpu::io_barrier()`
(`dsb sy` / `mfence`) before the doorbell. DMA memory (`DmaBuf`) is whole
frames from the frame allocator through the direct map; bus addresses are
physical (no IOMMU anywhere we run). Discovery registers each transport in
a registry (`register_transport(Device { transport, irq }, source)`) that
prints `[patina] virtio: mmio @ 0x… irq N device-id K (name) [source,
version, vendor, edge|level irq]`; drivers `take(device_type)` and anything
left (net until Phase 4) stays listed and undriven. Phase 4's virtio-net
and the later virtio-pci transport plug in at exactly these points:
virtio-pci implements `Transport` and calls `register_transport`; virtio-net
calls `take(DEVICE_NET)`, `negotiate`, `VirtQueue::new`, `driver_ok`, and
`attach_irq(line, callback)`.

## D59: virtio-mmio discovery per platform; device IRQ routing (GIC SPIs, x86 I/O APIC)
*2026-10-14 · Phase 3*

`arch::devices::virtio_mmio(&Cmdline) -> Vec<MmioDesc>` (pa, size, irq,
edge, active_low, source). aarch64: every enabled `compatible =
"virtio,mmio"` DTB node, `interrupts` = GIC triple (SPI n → INTID 32 + n,
flags 1/2 edge, 4/8 level); QEMU `virt` lists 32 transports, empty ones
(device id 0) are skipped silently. x86_64, merged in this order and
de-duplicated by base: `virtio_mmio.device=<size>@<base>:<gsi>[:<id>]`
parameters (parsed by `patina-cmdline`; QEMU microvm with `acpi=off`,
Firecracker; edge, active-high), then the DSDT's `LNRO0005` devices, then —
only if both are empty and there is no DSDT at all — a probe of
0xFEB00000 + 512·n for the magic with the GSI guessed as 5 + n. The DSDT is
found RSDP → RSDT/XSDT → FADT (X_DSDT preferred) and byte-scanned, not
interpreted: after each `LNRO0005` string the next Memory32Fixed (`86 09
00`) and Extended Interrupt descriptor (`89 len 00 flags n gsi`, bit 1 =
edge, bit 2 = active-low) before the next `LNRO`. All ACPI reading happens
in `early_main` on the boot tables (`arch/x86_64/platform.rs`), RSDP from
start_info, then the BIOS-area scan, then fw_cfg. Observed on this host
(QEMU 11.1.2): microvm with ACPI on (the CLI default) has qboot's tables at
an RSDP in the BIOS area (0xf5590), two I/O APICs (GSI 0-23, 24-47), a
hardware-reduced FADT and a 420-473-byte DSDT with the two devices at
0xfeb02e00 (console, GSI 47) and 0xfeb02c00 (rng, GSI 46), **Level,
active-high**; with `acpi=off` QEMU appends
`virtio_mmio.device=512@0xfeb00e00:12 virtio_mmio.device=512@0xfeb00c00:11`
after `--` and the cmdline path takes them (edge). pc/q35 have a DSDT with
no `LNRO0005` and no virtio-mmio bus: the kernel finds none. Routing:
`arch::irq::configure(irq, edge, active_low)` + `register(irq, fn(u32))` on
both arches. aarch64: the GIC is initialized on first use if `timer::init`
has not run yet, `GICD_ICFGR` is set from the DTB flags, the SPI is routed
to the registering (boot) CPU. x86_64 (`ioapic.rs`): I/O APICs from MADT
type 1 (default 0xFEC00000, GSI 0 without ACPI), all pins masked at first
use, interrupt source overrides (type 2) for ISA IRQs; `register` takes a
vector from 0x30-0x7f (new IDT stubs), programs fixed delivery, physical
destination = CPU 0's APIC id, the configured trigger/polarity, unmasked.
The device stub saves FS like the timer/IPI paths (the return path may
switch threads), calls the handler with IRQs masked, then sends the LAPIC
EOI, which is also the I/O APIC EOI for level pins. Handlers only read+ack
the transport's InterruptStatus and signal a driver `Event` (flag + wait
queue); queue processing happens in the woken kernel thread, which always
re-polls all queues, so an edge is never lost and a level line is quiet
before EOI. With `PATINA_CPUS=4` device IRQs still go to CPU 0 while the
scheduler runs everywhere (test-runtime 1/4 CPUs and test-ctl pass).

## D60: The kernel RNG is a ChaCha20 DRBG seeded from virtio-rng (replaces D35)
*2026-10-14 · Phase 3*

`crates/rand` (`patina-rand`, in-tree, no dependencies, RFC 8439 and RFC
7693 test vectors): `Drbg` hashes all inputs into a BLAKE2s pool; `reseed`
sets `key = BLAKE2s(old key ‖ pool digest)` and restarts the pool keyed by
that digest; `fill` uses ChaCha20 (nonce 0) with fast key erasure (block 0
becomes the next key, blocks 1.. are output, the key changes every call).
`kernel/src/random.rs` keeps `random::fill(&mut [u8])` (getrandom,
AT_RANDOM, the VFS worker's /dev/urandom). `seed_boot` (from the loader)
mixes 64 bytes from virtio-rng (polled, bounded 100 ms, so a silent device
does not hang the boot), up to four `cpu::hw_random` words, the boot
counter and `patina.epoch`, then logs `[patina] random: seeded from
virtio-rng (64 bytes) + RDSEED/RDRAND` (or the fallback, with a WARNING
when only the boot counter was available). With virtio-rng a kernel thread
reseeds from 32 fresh device bytes every 60 s and when `fill` reports 1 MiB
generated since the last reseed. `fill` also stirs the clock into the pool
(cheap, only matters for an unseeded boot). The virtio-rng driver is
polled (no IRQ): requests are rare; a timed-out request stays outstanding
and is collected by the next read.

## D61: virtio-console multiport; `patina.ctl` by name; `hvc0` with an early ring
*2026-10-14 · Phase 3*

`drivers/virtio/console.rs`. `VIRTIO_CONSOLE_F_MULTIPORT` is accepted when
offered; ports 0-3 get queues (port 0 = 0/1, control = 2/3, port n = 2n+2 /
2n+3), 8 posted 512-byte rx buffers each and one 4 KiB tx buffer. Boot
handshake, polled: DEVICE_READY; for each DEVICE_ADD answer PORT_READY (0
for ports ≥ 4); record PORT_NAME, CONSOLE_PORT, PORT_OPEN (host side
connected); send our PORT_OPEN for console and named ports; stop after 20 ms
without a new control message (QEMU answers during the notify). The control
channel finds its port by the exact name `patina.ctl` (QEMU: port 1, since
port 0 is reserved for a `virtconsole`); later control messages (PORT_OPEN
when a client attaches to the socket chardev, or detaches) are processed by
the ctl thread. Transmit is bounded-synchronous: stage into the DMA buffer,
notify, poll the used ring (100 ms for hvc0, 50 ms for ctl); a buffer the
device has not returned stays in flight and later data is dropped until it
returns, so a host that stops reading costs nothing. Without multiport the
device is one console port. `console=hvc0` (both arches, QEMU
`PATINA_CONSOLE=hvc0`): arch boot keeps its UART if the DTB/port probe has
one and `console::start_early_ring()` also records output in an 8 KiB ring;
when the console device is up, the first console port becomes the kernel
console (`console::switch_to`), after replaying the ring, so hvc0 gets the
whole boot log. The `Console` trait gained `flush()` (called after every
print) so hvc0 sends one buffer per print, not per `\r\n`. hvc0 works with
IRQs masked and on the exit/panic path: its tx lock is a normal lock until
`sched::exiting()` (an exit owner exists), then taken with a bounded spin
and force-broken, like the console lock. Verified on QEMU (both arches,
test-ctl `hvc0` case: boot log, Go output, sentinel on hvc0 while ctl runs on
port 1). Unverified: Apple VZ (`VZVirtioConsoleDeviceSerialPortConfiguration`,
expected single-port or multiport with port 0 as console).

## D62: virtio-vsock listener on port 4242 (UNVERIFIED)
*2026-10-14 · Phase 3*

`drivers/virtio/vsock.rs`, written against virtio 1.2 §5.10 and Linux's
`virtio_transport_common.c`; it compiles on both arches but has never run:
QEMU on macOS has no vhost-vsock and Firecracker/Cloud Hypervisor cannot
run here. Stream sockets only (`VIRTIO_VSOCK_F_STREAM` accepted if
offered), guest CID from config space (low 32 bits), rx (16 × 4 KiB posted
buffers, header + payload in one descriptor), tx (8 × 4 KiB slots, polled),
event queue (4-byte buffers; TRANSPORT_RESET drops the connection and
re-reads the CID). One connection on the listening port: a new REQUEST
replaces the current one (old peer gets RST) and is answered with
RESPONSE; requests for other ports, foreign data and RESPONSEs get RST;
SHUTDOWN → RST and close; RST → close. Credit: `buf_alloc` = 64 KiB,
`fwd_cnt` counts delivered bytes, CREDIT_UPDATE after half of it was
consumed since the last advertisement and on CREDIT_REQUEST; sends check
`peer_buf_alloc − (tx_cnt − peer_fwd_cnt)` and, without credit, drop the
data and send CREDIT_REQUEST. `flush` waits until the device has consumed
every tx slot (the exit event before poweroff). The ctl layer prefers vsock
when the device exists. The first real run (Firecracker or Cloud
Hypervisor on a Linux host, Apple VZ) must check: the CONNECT handshake end
to end, the header `len` for packets the device splits, and credit with a
host that reads slowly.

## D63: Control channel: one kernel thread, pending-event buffer, grace timer
*2026-10-14 · Phase 3*

`kernel/src/ctl.rs` over `patina_ctlproto` (design §10). Transport chosen
after `drivers::init`: vsock if present, else the `patina.ctl` console port,
else `[patina] ctl: no transport` (pc/q35). The loader calls
`ctl::ready(go)` just before `start_thread0` (Go version from
`.go.buildinfo` via `patina_loader::buildinfo`, `cpus` =
`arch::smp::cpu_count()` because the APs are not online yet, `mem_mb` =
total RAM), which also spawns the control thread. The thread waits on the
transport's interrupt `Event` (plus a 250 ms poll), feeds bytes through
`LineFramer<4096>` and answers: `ping` → `pong` (id, uptime); `signal` →
`send_to_process(SigInfo::kill(sig, SI_USER, 1, 0))` for SIGTERM, SIGINT,
SIGHUP, SIGUSR1, SIGUSR2, SIGQUIT (names with or without `SIG`, or numbers),
anything else → `error` "signal not injectable"; `dump` → SIGQUIT; `time`
→ `wallclock::set_now` (new `kernel/src/wallclock.rs`: an adjustable
CLOCK_REALTIME offset read by `clock_gettime`, POSIX/itimer realtime clocks
and futex absolute timeouts; `Process.epoch_ns` stays the boot value);
`stop` → SIGTERM and a grace deadline (`grace_ms`, default 10 s, 0 =
immediately) after which the thread calls `exit_with(137, None, true)`.
`crate::exit`/`exit_signal` became one `exit_with(code, signal, forced)`:
`stop_other_cpus` (EXIT_OWNER: a normal exit racing the forced one wins or
loses cleanly, the loser's CPU halts), the `exit` event, the `[patina] exit
code N` sentinel, poweroff. Events are encoded on the stack and appended to
a 4 KiB pending buffer that is written whenever a host is connected
(console PORT_OPEN / an accepted vsock connection) and dropped when full,
so `ready` reaches a host that connects after boot, and a host that
disconnects and reconnects keeps working (test-ctl `stop-handler`). `exit`
and `panic` are written synchronously with a 100 ms bound (vsock also waits
for the device to consume them) and use force-broken locks once exiting.
`panic` comes from the panic handler before the dump (test-ctl `panic`);
`log` (warn) from the ENOSYS-once path (test-ctl `ready-ping`, `signals
-enosys`). Malformed or unknown requests get `DecodeError::reply()`; an
over-long line gets `error` "line too long".
The channel is unauthenticated (design §15).

## D64: x86_64 poweroff order: isa-debug-exit, ACPI S5 from the FADT, i8042
*2026-10-14 · Phase 3*

`arch/x86_64/power.rs`. Cloud Hypervisor REBOOTS on an i8042 reset or the
ACPI reset register, so the old fixed order (debug-exit, PM1a at 0x604,
i8042) could rerun the program there. Now: (1) QEMU `isa-debug-exit` at
0xf4 if a read does not return 0xff; (2) ACPI S5 from the FADT gathered at
boot: hardware-reduced FADT (flag bit 20) with a SLEEP_CONTROL_REG → write
`SLP_TYP << 2 | SLP_EN` (0x34 for S5 = 5; I/O or MMIO per the GAS: Cloud
Hypervisor port 0x600, QEMU microvm MMIO 0xfea00200), else PM1a_CNT →
`SLP_TYP << 10 | SLP_EN` with SLP_TYPa from the DSDT `\_S5` package (byte
scan; QEMU pc/q35 `_S5` = 0 at 0x604); (3) the i8042 reset (Firecracker,
which has no ACPI sleep register; never reached on Cloud Hypervisor since
its S5 write does not return); (4) halt. Each method tried prints
`[patina] poweroff: <method>` after the sentinel. Verified without
isa-debug-exit: `-M pc` → `acpi s5 via pm1a_cnt (io 0x604, SLP_TYP 0)`,
`-M microvm` → `acpi s5 via sleep control register (mem 0xfea00200,
SLP_TYP 5)`, QEMU exits in both; with it (run-qemu.sh, CLI) microvm, pc and
q35 exit through isa-debug-exit as before. Firecracker and Cloud Hypervisor
are unverified.

## D65: os package test run and skip list policy
*2026-10-14 · Phase 3*

`scripts/test-os.sh [arch...]` is the early Milestone B coverage run (§13);
the Phase 5 harness will generalise it. It builds the kernel and `go test
-c os` (static, `-trimpath`) and packs the test binary as `/app`, a copy
of `$GOROOT/src/os` at `/src/os` and a small `/usr/bin`. `go test` runs a
test binary in its package directory, and `TestReadDir`, `TestReadFile`,
`TestFileReadDir/.`, `TestDirFSRootDir` and the testdata tests depend on
that. So the run uses the new kernel parameter `patina.cwd=/src/os`
(`fs/rootfs.rs`, documented in doc/cli-contract.md: the initial cwd, like
a container WORKDIR; a missing directory is logged and `/` is used).
`/usr/bin` is test data: on Linux `TestChdirAndGetwd` and
`TestReaddirnamesOneAtATime` use it. It is not a kernel base directory.
`HOME=/root` comes from `patina.env`. The program arguments are
`-test.short -test.v -test.timeout 12m -test.skip ^(A|B|…)$` in the
`-flag value` form, because a raw `k=v` word after `--` is a kernel
parameter. The run uses 1 GiB of RAM and a 900 s QEMU timeout. The script
counts the top-level `--- PASS/SKIP/FAIL` lines and prints
`os <arch> (cpus=N): N passed, M skipped (K by our list), F failed, exit
<code>`. It fails on any FAIL, a missing final `PASS`, a kernel
fatal/panic, a Go crash or a timeout. Logs go to `out/test/os/`.
`OS_TEST_RUN` and `OS_TEST_SHORT=0` are for debugging.

The skip list is `testapps/skiplist/os.txt`: one top-level test per line,
`TestName  # reason`, one file per package. A test goes on it only when it
needs a facility that is absent by design. A kernel bug or a missing
reasonable feature is fixed instead. There are two categories (14
entries). Network (Phase 4, `socket` is `ENOSYS`): `TestLargeCopyViaNetwork`,
`TestSpliceFile`, `TestGetPollFDAndNetwork` and `TestSendFile`. Child
processes ("needs os/exec (no execve, D57)"): `TestFileChdirTestlog`,
`TestStdPipe`, `TestReadNonblockingFd`, `TestProcessWithHandleLinux`,
`TestKillFindProcess`, `TestKillStartProcess`, `TestGetppid`,
`TestStatStdin`, `TestStartProcess` and `TestExecutable`. In Go 1.27,
`testenv.MustHaveExec` does not probe on Linux, so these tests fail rather
than skip. The process-creating `clone` fails with `EINVAL`
(`fork/exec /app: invalid argument`).

The tests skip 23 themselves. Six need `go build`. Seven follow from
`-test.short`; with `OS_TEST_SHORT=0`, five of them run and pass.
Five skip because we are root (`TestFilePermissions`,
`TestReadOnlyWriteFile` and three `RemoveAll` permission tests). The rest
are the Windows-only `TestStatConsole`, `TestLargeWriteToConsole` (needs
`-large_write`), `TestMkdirAllAtSlash` (not on a builder), `TestTTYClose`
(`/dev/tty` is `ENXIO`: there is no controlling terminal, D54) and
`TestHostname` (it execs `hostname`). Results with `-test.short`, at 1 and
4 vCPUs on both arches: 244 passed, 37 skipped (14 by our list), 0
failed. With `OS_TEST_SHORT=0` (x86_64, 2 vCPUs; aarch64, 4 vCPUs): 249
passed, 32 skipped, 0 failed.

The only `ENOSYS` lines are `socket` (Phase 4) and `pidfd_open` (D57;
Go falls back). Kernel fixes made for the run:

- `/proc/sys/fs/pipe-max-size` (65536: the fixed pipe ring, with no
  `F_SETPIPE_SZ`), used by `TestReadFileProc` and `TestDirFSReadFileProc`.
- `uname` reports the configured host name (`ip=` host field, else
  `patina`) as its nodename. It was a hard-coded `patina` and disagreed
  with `/proc/sys/kernel/hostname` and `/etc/hostname`.
- `patina.cwd`, described above.

The VFS semantics the suite exercises needed no changes. That includes
`Chtimes` ns precision, `Readdir` paging and seeking, `os.Root` (`openat2`
`ENOSYS` fallback), FIFOs, pipe deadlines, `RemoveAll`, `Truncate`,
`Link` and `Lchown`.
## D66: `patina run` against the real kernel: exit-code precedence, panic → 255, stop/timeout semantics, e2e test
*2026-10-14 · Phase 3*

`patina run` (cli/internal/run) picks the exit code in this order: `--timeout`
expired → 124 (like timeout(1), even when the guest then exits 0 on the
stop's SIGTERM or is forced), the `exit` event's code, a `panic` event →
255, the `[patina] exit code N` serial line, QEMU isa-debug-exit (x86,
status ≥ 3), the CLI killed the VMM → 137, else 1. SIGINT, SIGTERM and
(new) SIGHUP become `stop` with `--grace` (default 10 s); a second one kills
the VMM (137); the CLI also kills after grace + 2 s without an exit. The QEMU
control chardev changed from `wait=off` to `wait=on`: QEMU does not start
the guest until the control proxy is connected, so `ready` and the `exit` of
a program that ends in milliseconds always reach the CLI (with `wait=off` a
fast hvf guest finished before the proxy's 100 ms retry connected and the
code came from the sentinel). The resulting "QEMU waiting for connection"
info line is filtered from stderr. CLI messages are written between guest
console lines (a mid-line console gets a newline first), so `-v` event lines
never split a kernel line. No kernel change was needed: the pending-event
buffer, the exit flush before poweroff and the grace timer worked as D63
describes. Verified by `PATINA_E2E=1 go test ./internal/run -run E2E -v`
(cli/internal/run/e2e_qemu_test.go, 10 cases × 2 arches, all PASS, 3 runs
in a row): hello args/exit 3, ctl stop handler/nohandler/forced, ping, dump,
signal SIGUSR1, `--timeout`, SIGINT → 143, double SIGINT → 137. A `patina
run` killed with SIGKILL still leaves QEMU running (own process group, no
parent-death signal on macOS).

## D67: TCP/IP stack: smoltcp behind one lock, a loopback mux, one net task
*2026-10-14 · Phase 4*

The stack (`kernel/src/net/stack.rs`) is one smoltcp 0.14 `Interface`, one
`SocketSet`, the per-socket state of every inet socket and the bind table,
all in one IRQ-safe `SpinLock<Stack>`; nothing in it blocks. smoltcp has no
"deliver to self" path, so its single `Device` is a mux (`net/mux.rs`) over
the NIC (`net::device::take()`, after `drivers::init`) and a fixed 256-frame
loopback ring: a transmitted frame goes to the ring if it is for our MAC, is
an IP packet for one of our addresses / 127/8 / `::1`, or is an ARP request
or neighbor solicitation for one of them (smoltcp then answers its own ARP and
learns the neighbor, like its `examples/loopback.rs`); everything else goes
to the NIC, or is counted and dropped without one. One deviation from that
recipe: smoltcp never answers a solicitation for `::1` (its solicited-node
check skips the loopback address), so the mux re-addresses such a
solicitation to `::1` itself (fixing the ICMPv6 checksum); smoltcp records
`::1` → our MAC from its link-layer option. `transmit` refuses a token while
the ring is full, so loopback backpressure holds data in the TCP socket
instead of dropping it. The interface always has 127.0.0.1/8 and ::1/128, a
fe80::/64 EUI-64 address with a NIC, and the static `ip=` client address plus
a default IPv4 route through the gateway (`ip=dhcp` only logs that the DHCP
client is not implemented; `stack::start_dhcp` is the hook). The MAC is the
NIC's or 02:00:00:00:00:01. One boot line reports it: `[patina] net:
<driver> hwaddr …, addrs …, gateway …` or `[patina] net: no device, loopback
only (hwaddr …, addrs …)`. The interface is run by the net task (a kernel
thread from `drivers::spawn`: poll, fire readiness notifications, sleep on
the NIC's `Event` — or a stack-owned one — until `poll_delay`, at most 1 s,
with a guard against zero-delay spinning) and inline by every socket syscall
that changes state, right after the change and under the same lock, so a
`write` is on the wire (over loopback: in the peer's receive buffer) before
it returns; such a syscall signals the task's event only when the poll left
an earlier deadline than the one the task sleeps on. `Stack::poll_all` feeds
ingress one packet at a time (refilling TCP listen pools after each, D68),
then egress, repeating while the loopback ring is non-empty (bounded
rounds). After each poll every socket's readiness is recomputed and hooks
are notified of new bits (and of a grown receive queue even if `IN` was
already set, for edge-triggered users); the notifications are collected
under the lock and fired after it (fs/file.rs lock order). The NIC IRQ
handler only signals the event (net/device.rs contract). Delayed ACKs are
off (`set_ack_delay(None)`): they only add latency over loopback.
Verified by `scripts/test-hello.sh --app sockets` and httphello `-selftest`
on both arches (no NIC: loopback only), `scripts/test-smp.sh --app sockets
--cpus "1 2 4"`.

## D68: inet sockets and the socket syscalls: `SocketOps`, listen pools, dual stack
*2026-10-14 · Phase 4*

Socket syscalls (`syscall/net.rs`) find the socket through
`File::as_socket() -> Option<&dyn net::socket::SocketOps>` (`ENOTSOCK`
otherwise) and own everything family-independent: copying addresses
(`patina_inet::SockAddr`, which already decodes `sockaddr_un`), iovecs,
option values and `msghdr`; blocking with `fs::file::wait_ready` honouring
`O_NONBLOCK`, `MSG_DONTWAIT` and `SO_RCVTIMEO`/`SO_SNDTIMEO` (timeout →
`EAGAIN`); `MSG_WAITALL` loops; `SIGPIPE` on `EPIPE` unless `MSG_NOSIGNAL`.
`SocketOps` methods never block. AF_UNIX is meant to be a second
implementor plus two lines in `socket`/`socketpair` (which return
`EAFNOSUPPORT` for AF_UNIX today; AF_INET `socketpair` is `EOPNOTSUPP`;
AF_NETLINK, AF_PACKET and the rest `EAFNOSUPPORT`). Interrupted waits return
`ERESTARTSYS` (no timeout set) or `EINTR` (Linux `sock_intr_errno`); a
blocking `connect` returns `EINTR` and the attempt continues, a nonblocking
one always returns `EINPROGRESS` even when loopback could finish at once
(Linux does the same in practice, and Go waits for `EPOLLOUT` then reads
`SO_ERROR`). The inet socket (`net/inet.rs`) is an id; its state lives in the
stack. TCP listen with smoltcp's one-socket-per-connection model deviates
from "a pool of min(backlog, 16) refilled on accept": a listener keeps
`LISTEN_POOL` (4) sockets in `Listen` plus an accept queue of sockets that
left `Listen`, bounded by the backlog (clamped to 1..128); the pool is
refilled after every ingress packet, so a burst of SYNs (50 concurrent
dials) each find a listening socket, and a SYN is only refused (RST) when
the accept queue is full. `accept4` takes the first queued socket past
`SynReceived`. Connection errors come from smoltcp state transitions tracked
after each poll: `SynSent → Closed` is `ECONNREFUSED` (`SO_ERROR`, `EPOLLERR`),
75 s in `SynSent` is `ETIMEDOUT`, a synchronized state → `Closed` is a reset
(`ECONNRESET` once, then EOF on read and `EPIPE` on write). Close is graceful
(`close()`, the smoltcp socket kept as a zombie until `Closed`/`TimeWait`, at
most 60 s); unread data, data arriving after close, or `SO_LINGER` {1, 0}
abort with a RST, as on Linux. Socket buffers are 64 KiB each way (TCP) and
16 × 2 KiB packets each way (UDP); `EPOLLOUT` is reported once a quarter of
the send buffer is free. Ports: a bind table in `crates/inet` (host-tested)
with Linux's ephemeral range 32768–60999 per protocol, `EADDRINUSE` unless
both sides set `SO_REUSEPORT`, or both set `SO_REUSEADDR` and the existing
one is not listening; a binding is released when its socket's file is (so a
closed listener's port is immediately reusable even without `SO_REUSEADDR`;
TIME_WAIT never blocks a bind). Dual stack: AF_INET6 sockets default to
`IPV6_V6ONLY` = 0; a wildcard binding covers both families for the bind
table, the smoltcp endpoint has no address and matches both versions, IPv4
peers show as `::ffff:a.b.c.d`, and `::ffff:` destinations are used as IPv4.
With `V6ONLY` = 1, mapped addresses give `EINVAL` (bind) and `ENETUNREACH`
(connect/sendto). smoltcp wildcard endpoints match both versions even for
AF_INET or V6ONLY sockets, so a connection of the wrong family is reset at
accept and a datagram of the wrong family is dropped at receive. Go's IPv6
probe (bind `[::1]:0` with V6ONLY=1 and `[::ffff:127.0.0.1]:0` with
V6ONLY=0) succeeds, matching what the stack does. Other kernel changes:
`splice`/`tee` return `EINVAL` instead of `ENOSYS` — Go's `io.Copy` between
TCP connections tries splice and falls back to read/write only on `EINVAL`
(ENOSYS failed the copy and closed the echo connection). Of 127/8 only
127.0.0.1 is an address (smoltcp accepts packets for exact addresses only):
binding 127.0.0.2 is `EADDRNOTAVAIL`, connecting to it `ENETUNREACH`. Not done: UDP
datagrams larger than the MTU (`EMSGSIZE`; no IP fragmentation), ICMP errors
to UDP sockets, `SO_RCVTIMEO` on plain `read`/`write` (only the `recv*`/
`send*`/`accept`/`connect` syscalls look at it; Go uses deadlines instead),
`TCP_INFO` (`ENOPROTOOPT`), `sendmmsg`/`recvmmsg` (`ENOSYS`), multicast.
Verified by testapps/sockets (21 `ok` checks) on both arches at 1, 2
and 4 vCPUs and by httphello `-selftest`.

## D69: virtio-net driver: one queue pair, no offloads, `NetDevice` handle
*2026-10-14 · Phase 4*

`drivers/virtio/net.rs`, against virtio 1.2 §5.1. Features: only
`VIRTIO_NET_F_MAC` (5) and `VIRTIO_NET_F_STATUS` (16) when offered (plus
`VERSION_1` on modern transports); no `MRG_RXBUF`, no checksum/GSO/TSO/UFO/ECN
offloads (the stack computes checksums), no control queue, no multiqueue, so
there is exactly receiveq1 (0) and transmitq1 (1). The `virtio_net_hdr` is 10
bytes on a legacy transport and 12 (with `num_buffers`) once `VERSION_1` was
negotiated; transmit headers are all zero, receive headers are skipped. The
MAC comes from config bytes 0-5 (without F_MAC a locally administered
02:xx:… is generated and logged; it is not written back to the device), the
link state from config offset 6 at init (logged, not tracked). Receive: queue
size min(queue_max, 128), one posted 2 KiB device-writable descriptor per
buffer (one 256 KiB `DmaBuf`); `recv` pops used entries until one fits the
caller's buffer (oversized and runt frames are dropped), re-posts every buffer
it popped and kicks once per call. Transmit: min(queue size, 32) slots of
2 KiB; `send` reclaims the used ring, stages header + frame in a free slot as
one device-readable descriptor and kicks per frame, returning `false` when no
slot is free (a frame that cannot fit a slot is dropped and reported as
sent). The transport sits in a `OnceCell` so the interrupt callback only
counts, reads+acks the interrupt status and signals the driver's `Event` (D59)
without the ring lock (`SpinLock<Option<Net>>`); the `NetDevice` registered
with `net::device` is a handle that locks it per call, which also lets the
self-test drive the rings directly. Every init step is non-blocking (no device
round trip is awaited). Verified by `patina.selftest=net` (hand-built ARP
who-has 10.0.2.2 against QEMU slirp, reply `52:55:0a:00:02:02`, then 64
back-to-back requests with 64 replies, IRQs counted with the timer started) on
aarch64 hvf/tcg, legacy and modern (`force-legacy=false`, 12-byte header)
mmio, x86_64 microvm with ACPI (DSDT, level IRQ) and `acpi=off` (cmdline,
edge), 1 and 2 vCPUs; `scripts/run-qemu.sh` gained `PATINA_NET=user[:H-G,…]`.

## D70: virtio-pci transport, PCI discovery, MSI-X (LAPIC, GICv3 ITS, GICv2m) and INTx
*2026-10-14 · Phase 4*

Cloud Hypervisor has no virtio-mmio, so virtio-pci moved up from Phase 5.
**Discovery** (`drivers/pci.rs`): `arch::devices::pci_hosts()` returns
fixed-size `PciHostDesc`s. On x86_64 there is one per ACPI MCFG entry
(`acpi::mcfg`, read with the other tables in `platform.rs`, logged as
`[patina] acpi: mcfg @ <table>: ecam <base> segment S bus A-B`). QEMU q35 has
ECAM 0xb0000000 for buses 0-255; Cloud Hypervisor (from its source) has
0xe8000000 for bus 0 per segment. Without an MCFG but with a full,
non-hardware-reduced FADT and a working 0xCF8 probe (QEMU `pc`), the host uses
the legacy port mechanism. On aarch64 it is the DTB's
`pci-host-ecam-generic` node, parsed before the heap: `reg`, `bus-range`,
`linux,pci-domain`, `ranges` (first 32-bit and 64-bit memory windows),
`interrupt-map` with `interrupt-map-mask` (up to 64 entries; the parent
phandle's `#address-cells`/`#interrupt-cells` are honoured), and the first
`msi-map` entry's device-id offset. The ECAM window is mapped once (2 MiB
blocks). Enumeration starts at the first bus and follows bridges whose
secondary bus is already programmed; nothing is renumbered. Memory BARs are
sized with decoding off; I/O BARs are ignored. A BAR that reads 0 is
unassigned: QEMU `virt` with `-kernel` has no firmware, so the BAR gets an
address from the 32-bit window (the 64-bit window as a fallback for 64-bit
BARs). The address is bump-allocated past every BAR that is already assigned,
aligned to the BAR's size, written, and verified by read-back. SeaBIOS (q35
and pc) and Cloud Hypervisor assign BARs themselves. On x86 no window is known
(no `_CRS`), so a device whose needed BAR is unassigned is skipped with a log
line. COMMAND gets memory space and bus master set, plus INTx disable when
MSI-X is used.

**Transport** (`drivers/virtio/pci.rs`, `PciTransport: Transport`). It
handles vendor 0x1af4, devices 0x1000-0x107f: device type = id − 0x1040 for
modern ids, else the subsystem id for transitional devices. It uses only the
modern vendor capabilities (common, notify with `notify_off_multiplier`, ISR,
device cfg; the first of each type), so legacy I/O BAR0 is never touched, and
a device without them is skipped. Behaviour:
- 64-bit common-cfg fields are written as two 32-bit halves.
- `queue_notify_off` is read per queue at `queue_setup`. The doorbell is
  notify base + off × multiplier, written as a 16-bit queue index.
- Writing status 0 waits up to 100 ms for the reset to read back 0.
- Reading the ISR acks it, and deasserts INTx.

**Interrupt seam: no change for drivers.** `Device { transport, irq:
Option<IrqLine> }`, `IrqLine { irq, edge, active_low }` and `attach_irq(line,
cb)` are unchanged. The only addition is a defaulted
`Transport::irq_kind() -> Option<&str>` ("msi-x"/"intx"), used only by the
discovery line. MSI-X is set up at discovery, before any driver runs:
- `arch::irq::alloc_msi(device_id) -> Option<MsiMsg { irq, addr, data }>`
  reserves a vector or LPI and names it with an arch IRQ number that
  `configure`/`register` accept. The transport writes the message to MSI-X
  table entry 0 and unmasks it. MSI-X is enabled with the function mask
  held while the entry is written. The `IrqLine` handed to the driver is
  `{ irq: msg.irq, edge: true }`.
- The device is told to use vector 0 for every queue (`queue_msix_vector`,
  written in `queue_setup`) and for config changes (`msix_config`, written
  with `DRIVER_OK`, because a reset clears both). Each write is read back
  and a refusal (0xffff) is logged.
- `ack_interrupt` reports `INT_VRING` when the ISR reads 0 under MSI-X:
  Cloud Hypervisor never sets the queue bit with MSI-X.

Per platform:
- **x86_64**: `alloc_msi` takes a free vector from 0x30-0x7f with a no-op
  handler until `register`. The IRQ number is `MSI_IRQ_BASE` (0x100) +
  vector (q35 shows `irq 304` = vector 0x30). The message is address
  0xFEE00000 | (boot CPU APIC id << 12), data = vector (fixed, edge).
  `register` for such a number only installs the handler; the I/O APIC is
  not touched. INTx on x86 would need the DSDT `_PRT` (AML), so it is not
  done: a function without MSI-X is registered without an IRQ.
- **aarch64**: Cloud Hypervisor's virtio-pci has no INTx at all. The
  interrupt pin is 0, `interrupt-map` is empty, and only `VirtioInterruptMsix`
  exists (`virtio-devices/src/transport/pci_device.rs`). Under KVM that MSI-X
  is delivered through the GICv3 ITS (`msi-parent`/`msi-map` to an
  `arm,gic-v3-its` node). On MSHV it uses a GICv2m frame instead. Both are
  implemented:
  - `arch/aarch64/its.rs`, a minimal ITS driver. It sets up LPIs on the boot
    CPU's redistributor: `GICR_PROPBASER` with IDbits 14, a configuration
    byte of priority 0x80 | group 1 | enable, and a zeroed 64 KiB
    `GICR_PENDBASER`, then `EnableLPIs`. On the ITS it sets up a one-page
    command queue and flat device and collection tables. For the
    `GITS_BASER` page size it takes what the ITS keeps after asking for
    4 KiB: KVM's ITS only implements 64 KiB pages, QEMU's takes 4 KiB. Then
    MAPC collection 0 → the boot CPU (redistributor PA or processor number,
    per `GITS_TYPER.PTA`) and SYNC. Each MSI gets MAPD (ITT of 2 events),
    MAPTI event 0 → the next LPI (8192+), INV and SYNC, polled to
    completion. The message is address `GITS_TRANSLATER`, data 0.
  - `v2m.rs`: `MSI_TYPER` or the DTB `arm,msi-base-spi`/`arm,msi-num-spis`
    give the SPI range. The message is address `MSI_SETSPI_NS`, data = SPI
    INTID, edge.
  - `gic::ack` now passes LPI INTIDs through instead of treating them as
    special, and `irq::register` keeps LPI handlers in a separate 64-entry
    table.
  - INTx is the fallback when neither exists, or when forced with
    `patina.pci.irq=intx`. The pin is looked up in the `interrupt-map`,
    which already encodes the slot swizzle for root-bus devices, giving a
    level SPI. Shared lines work through `attach_irq`, which calls every
    callback on the line.

QEMU `virt` has an ITS under TCG and a v2m frame (SPIs 80-143) under hvf,
so both MSI paths are exercised on this host.

**Scripts**: `PATINA_VIRTIO=pci` makes `run-qemu.sh` add
`virtio-serial-pci` / `virtio-rng-pci` (same ports) instead of the `-device`
variants, using a `$vsuf` suffix. This is now the default for x86
`pc`/`q35`, and microvm refuses it. `PATINA_VIRTIO_PCI_OPTS` appends device
properties, for example `,disable-legacy=on` for modern-only ids 0x1040+.
The default devices are transitional and work as is.

**Verified on QEMU 11.1.2**:
- aarch64 `virt`:
  - hvf v2m MSI-X: test-ctl 8/8 (also with `PATINA_CPUS=4` and
    `disable-legacy=on`), test-hello, and fileio over `hvc0`.
  - TCG ITS LPIs: test-ctl 8/8, test-hello, fileio over `hvc0`.
  - INTx with `patina.pci.irq=intx`: fileio over `hvc0` on SPIs 36/37, and a
    shared SPI 36 with console at slot 1 and rng at slot 5.
- x86_64 q35 (MCFG ECAM, MSI-X): test-ctl 8/8 (also 4 CPUs), test-hello,
  and fileio over `hvc0`.
- x86_64 `pc` (port 0xCF8, MSI-X): test-ctl 8/8.

Interrupt delivery was confirmed with a temporary first-interrupt log in
`virtio::dispatch`: the ctl tests otherwise also pass on the control
thread's 250 ms poll.

**Untested**: Cloud Hypervisor on both arches. That covers MCFG bus 0 at
0xe8000000, 64-bit BARs above RAM, the 512 KiB BAR0 that holds the
capabilities and the MSI-X table, the KVM ITS with 64 KiB table pages, the
MSHV v2m DTB override, and `msi-map` offsets for segment > 0. Not done:
x86 INTx (`_PRT`), bridge renumbering, the swizzle for INTx behind bridges,
per-queue MSI-X vectors, and LPIs targeted at CPUs other than the boot CPU.

## D71: AF_UNIX sockets: one table, names by id, fds in flight as `OpenFile`s
*2026-10-14 · Phase 4*

`kernel/src/net/unix/` implements `SOCK_STREAM`, `SOCK_DGRAM` and
`SOCK_SEQPACKET` (`SOCK_RAW` is `SOCK_DGRAM`, as on Linux) as a second
`SocketOps` implementor. Every socket is a `Sock` in one global table keyed by
a never-reused id behind one leaf `SpinLock`; the fd-side `UnixSocket` holds
the id. Peers, listeners' accept queues, filesystem nodes and the abstract
namespace refer to sockets by id, so there are no reference cycles, a dead id
reads as "gone", and every two-socket operation (send, connect, close) is one
critical section with no per-socket lock order. Hooks are fired and passed
files dropped only after the lock is released, and no FS or fd-table lock is
taken under it (paths are resolved first, fds looked up before and installed
after). Received data lives with the receiver: streams use
`patina_inet::unix::StreamBuf` (a byte FIFO in which a send's ancillary data
is a mark on its bytes; a read that reaches a mark takes it and stops at the
end of that send, as Linux stops after the skb carrying fds), datagram and
seqpacket sockets a queue of messages carrying the sender's name. Each socket
receives into at most 212992 bytes (Linux's `wmem_default`; stream bytes, or
datagrams with at most 64 queued), the largest datagram is 212960 bytes
(`sk_sndbuf - 32`), and a stream writer is `EPOLLOUT` while a quarter of the
peer's buffer is free (it may write whenever any space is free).
`SO_SNDBUF`/`SO_RCVBUF` are stored doubled and reported but do not resize
anything. A stream `connect` creates the server-side socket at once (Linux's
embryo) and queues it on the listener, so it completes without
`EINPROGRESS`; only a full backlog (queue longer than the backlog, which is
clamped to `SOMAXCONN` 4096 and taken as 4096 when negative) gives `EAGAIN`, on
which the generic `connect` now waits for `OUT` (the socket registers on the
listener's `peer_wait` list) and retries unless non-blocking. The accepted
socket's name is the listener's and inherits its `SO_PASSCRED`. Closing a
socket shuts both directions of its peer (`EPOLLIN|RDHUP|HUP`, then EOF and
`EPIPE`); unread data or an unaccepted connection gives the peer one
`ECONNRESET` first. Closing a listener closes its unaccepted connections.
Datagram semantics follow `unix_dgram_sendmsg`: a connected socket only
accepts from its peer (`EPERM` otherwise), sending to a closed socket is
`ECONNREFUSED` (and drops a dead default peer), a full receiver gives
`EAGAIN` and the sender waits on that receiver's `peer_wait`. Seqpacket
ignores a destination address and reports EOF on close.

Names: a pathname `bind` creates an `S_IFSOCK` inode (mode 0777 & ~umask,
`EADDRINUSE` if the name exists, `ENOENT` without a parent) whose special is
a `SockSpecial(id)` (a plain `patina_vfs::Special`, not a `DevNode`, so
`open()` of it is `ENXIO`). `connect`/`sendto` resolve the path with
`namei::with_node`: a missing path is `ENOENT`; a non-socket inode or a socket
inode whose id is gone is `ECONNREFUSED`, as in Linux `unix_find_bsd` (not
`ENOTSOCK`); a type mismatch is `EPROTOTYPE`. `unlink` removes the node and
the bound socket keeps working; closing the socket leaves the node behind
(stale, `ECONNREFUSED`, and a later bind of it is `EADDRINUSE`), as on Linux.
Abstract names (leading NUL, the rest of `addrlen`) live in an in-kernel table
and are freed on close. Autobind (a five-hex-digit abstract name) happens for
`bind` with `addrlen == 2` and for `connect`/`sendto` with `SO_PASSCRED` set.
Unnamed sockets report `sun_family` only (`addrlen` 2). `mknodat(S_IFSOCK)`
stays `EPERM`.

Ancillary data: `sendmsg` parses `msg_control` like `__scm_send` (malformed
record, unknown `SOL_SOCKET` type or more than `SCM_MAX_FD` (253) fds →
`EINVAL`, bad fd → `EBADF`, `SCM_CREDENTIALS` accepted with any pid and
`EINVAL` for uid/gid -1) and attaches the fds as `Arc<OpenFile>`s; a stream
send passes the control data with its first chunk only. The receive side
needs `msg_controllen`, so `SocketOps` grew `recv_msg(buf, flags, room)` (the
default calls `recv`): the socket builds the control data with
`patina_inet::unix::CmsgWriter`, which reproduces `put_cmsg` and
`scm_detach_fds` byte for byte (credentials first when the receiver set
`SO_PASSCRED`, pid 1/uid 0/gid 0 unless the sender gave some; then as many
fds as `(room - 16) / 4` allows, installed with `FD_CLOEXEC` under
`MSG_CMSG_CLOEXEC`; the rest are closed and `MSG_CTRUNC` is set). `MSG_PEEK`
installs duplicates, as Linux does. `SO_PEERCRED` is {1, 0, 0} for connected
stream/seqpacket sockets, socketpairs and listeners, {0, 65534, 65534}
otherwise. Other generic changes: `SocketOps::max_dgram` (unix 212960)
replaces the fixed 65507 bound, and `read`/`write` on a datagram socket move
one whole message per call instead of 64 KiB chunks (`syscall/fs.rs`). A
`MSG_WAITALL` stream receive stops after any control data. Deviations: a read
on an unconnected stream socket is `EINVAL` (Linux blocks); fds in flight
are not garbage-collected, so a socket sent over itself (a cycle) is never
freed; `MSG_OOB` is `EOPNOTSUPP`; `SO_PEEK_OFF`, `SO_PASSSEC`,
`SO_PASSPIDFD` and other levels than `SOL_SOCKET` are not supported
(`ENOPROTOOPT` / `EOPNOTSUPP`); `recvfrom` on a stream reports no address
(Linux gives the peer's); no permission checks (single root user).
Verified by testapps/unixsock (36 `ok` checks: path and abstract listeners,
error paths, unlink while listening, 50 concurrent connections, 8 MiB each
way, half-close, deadlines, socketpair of every type, SCM_RIGHTS over a
socketpair and a `net.UnixConn`, `MSG_CTRUNC`, `MSG_CMSG_CLOEXEC`,
SCM_CREDENTIALS, `SO_PEERCRED`, unixgram with sender addresses, 64 KiB
datagrams, backlog, autobind, HTTP) on both arches at 1, 2 and 4 vCPUs, and
host tests of the stream buffer and the cmsg codec.

## D72: DHCP at boot, the effective network config, generated resolv.conf/hosts, test-net.sh
*2026-10-14 · Phase 4*

This replaces the `start_dhcp` stub described in D67.

**When DHCP runs.** `ip=dhcp` (also `on`/`any`/`bootp`/`both`, or an `ip=`
without a client address) adds a smoltcp `dhcpv4::Socket` to the socket set
in `net::init`. `kernel_main` calls `net::wait_dhcp()` after `ctl::init` and
before the self-test and the loader. It runs the interface itself, polling
under the stack lock at least every 1 ms and spinning between polls. It
stops when a lease is applied or after 10 s. No IRQs are needed, because the
NIC's `recv` polls its used ring. The net task may also poll during the
wait, which is harmless (see below).

**Client settings.** smoltcp waits 10 s before retransmitting a DISCOVER and
5 s before retransmitting a REQUEST. Both are lowered to 1 s, so one lost
packet does not use up the boot budget. The client also requests options 12
(host name) and 15 (domain), parsed from a 1500-byte receive buffer; smoltcp
only parses mask, router and DNS itself.

**Applying events.** `Stack::poll_all` handles the socket's events after
every poll, so the boot wait, the net task and socket syscalls all apply
them the same way:
- `Configured`: replace the non-loopback IPv4 address and the default route
  (`Stack::set_v4`, which also updates the mux's own-address list and
  `gw4`), and record the lease. Because a receive buffer is set, smoltcp
  emits `Configured` on every renewal ACK. An event with nothing new is
  ignored; a changed lease is logged as `dhcp lease changed: …`.
- `Deconfigured`: this is also the client's first event, before any lease,
  so it is a no-op unless a lease is held. Otherwise it removes the address
  and route and logs `dhcp lease lost`. Nothing panics.

**Log lines.**
- Lease: `[patina] net: dhcp lease 10.0.2.15/24 via 10.0.2.2, dns 10.0.2.3
  (2 ms)`. QEMU slirp takes 2 ms on aarch64 hvf and 5 ms on x86 TCG.
- Timeout: `[patina] net: dhcp: no lease after 10 s, continuing without an
  address`.
- No NIC: `ip=dhcp: no network device, …`.

The timeout and no-NIC lines also go out through `ctl::log_warn`, which is
why the wait runs after `ctl::init`. These events can arrive before `ready`;
the CLI accepts `log` events in any order.

**Effective config.** `net::effective_config() -> NetConfig { addr, gateway,
dns, hostname, domain, leased }` starts from the static `ip=` fields and is
replaced by the lease. DNS servers come from the lease, else from `ip=`. The
default (none) is left to the consumer.

**Generated /etc files.** `rootfs::etc()` runs inside the loader, after the
wait, and builds `/etc/resolv.conf` and `/etc/hosts` from the effective
config:
- `resolv.conf`: `search <domain>` when the server sent a domain, then the
  nameservers, falling back to `nameserver 10.0.2.3` (slirp).
- `hosts`: `<addr> <hostname>` (with `<hostname>.<domain>` first when there
  is a domain). Without an address the host name goes on the `127.0.0.1`
  line, so `LookupHost(hostname)` works on loopback-only VMs. 127.0.0.1 is
  used rather than Debian's 127.0.1.1 because it is the only 127/8 address
  the stack has (D68).
- `rootfs::hostname()` (uname, `/etc/hostname`) uses the `ip=` host field,
  else the DHCP host name, else `patina`.
- The initrd still wins (D54).

Lease changes after boot are not written back to these files: rewriting the
VFS from the net task would need a different lock order, and slirp and
static setups never change the lease.

**httphello additions.**
- `/big`: 4 MiB where byte i is `(i*31 + i>>12) & 0xff`. sha256 is
  `9d2b62e8…a5cb`, which the test computes independently.
- `/echo`: returns the request body.
- `-nohandler`: no `signal.Notify`, so SIGTERM is SIG_DFL and the exit code
  is 143.
- `-selftest` now also does loopback GET `/big`, POST `/echo`, and Go
  resolver lookups of `localhost` (must include 127.0.0.1) and of
  `os.Hostname()`.
- `-dns <name>` adds a lookup through resolv.conf's server.

**`scripts/test-net.sh [aarch64] [x86_64]`** is Milestone A on QEMU (§13).
It builds the CLI and kernels, `patina build`s httphello, and runs these
cases, each with a hard timeout:
- **serve**: `patina run --net user --publish P:8080`. GET /, 20 concurrent
  GETs, the /big sha256, then `patina ctl stop` gives exit 0 and the
  graceful-shutdown line.
- **nohandler**: the same with `-nohandler`; the result is 143/SIGTERM.
- **selftest**: through the CLI, with `-dns` when `PATINA_NET_EXTERNAL=1`.
- **dhcp**: `run-qemu.sh` with `PATINA_NET=user ip=dhcp`.
- **dhcp-nonic**: `ip=dhcp` with no NIC.
- **dhcp-nolease**: the NIC is a `-netdev socket` sending to the UDP
  discard port. The test checks the 10 s line and the ctl `log` event, and
  that the program still runs.
- **pci**: x86 `q35`, or aarch64 `PATINA_VIRTIO=pci`, with the static `ip=`.
  It curls, then stops through `ctlclient.py`.

Host ports are `PATINA_NET_PORT` (default derived from the PID), plus 0-9
for aarch64 and 10-19 for x86_64, each probed free. Run dirs and ctl sockets
live in a private `/tmp/ptnet.*` directory. The exit trap kills only QEMUs
whose command line names that directory, so concurrent QEMUs from other
runs are left alone. The CLI was not changed: `--net user` still sends the
static `ip=`, so DHCP is tested through `run-qemu.sh`.

## D73: net package test run, skip list, read-only netlink, sendfile
*2026-10-14 · Phase 4*

`scripts/test-gonet.sh [--pkg net|net/http] [arch...]` is the network
counterpart of `test-os.sh` (D65) and follows it: `go test -c` (static,
`-trimpath`), the package directory at `/src/<pkg>` plus
`$GOROOT/src/testdata` at `/src/testdata` (net's sendfile tests read
`../testdata/Isaac.Newton-Opticks.txt`), `patina.cwd=/src/<pkg>`,
`HOME=/root`, 1 GiB, a 900 s QEMU timeout, `-test.short -test.v
-test.timeout 13m -test.skip ^(…)$` from `testapps/skiplist/<pkg>.txt`
(`net.txt`; `net_http.txt` if net/http ever needs one), the same summary
line and failure rules, logs in `out/test/gonet/`. It adds a slirp NIC
(`PATINA_NET=user`) and the static `ip=10.0.2.15::10.0.2.2:255.255.255.0:
patina:eth0:off:10.0.2.3` line the CLI passes, so `eth0` has an address and
`/etc/resolv.conf` points at slirp's DNS. Knobs: `GONET_TEST_RUN`,
`GONET_TEST_SHORT=0`, `GONET_TEST_TIMEOUT`, `PATINA_PARAMS`.

The skip list follows the D65 policy and has two categories (35 entries).
"AF_UNIX: pending" (34): every top-level test with a unix, unixgram or
unixpacket case, since `socket(AF_UNIX)` is `EAFNOSUPPORT` until the AF_UNIX
change is merged; most of them also have tcp/udp subtests that pass, and the
list is to be emptied of these after the merge. "needs os/exec" (1):
`TestSplice` runs its peer as a re-exec of the test binary. The tests skip
54 themselves, mostly "no external network in -short mode" (31) and other
`-short` skips; also the cgo resolver (3), `-ipv6`/`-tcpbig`/`-dnsflood`
flags, `/etc/services` (absent), the pty test, MPTCP (`IPPROTO_MPTCP` is
`EPROTONOSUPPORT`, Go falls back) and the raw-IP cases. Results with
`-test.short`, both arches, 1 vCPU, and 4 vCPUs on each arch: 220 passed,
89 skipped (35 by our list), 0 failed; three 2-vCPU reruns on aarch64 were
identical. Without `-test.short` (2 vCPUs, both arches; the DNS and
external-network tests then run over slirp and pass): 250 passed, 3 failed —
the two multicast listener tests (`IP_ADD_MEMBERSHIP` etc. are
`ENOPROTOOPT`: multicast is not implemented; `-short` skips them) and
`TestSendfileWithLargeFile` (its 2 GiB file does not fit the RAM-backed
`/tmp`). `net/http` needs no list: 611 passed, 10 skipped, 0 failed with
`-short` on both arches (616/5/0 without it, aarch64, 2 vCPUs). The `os` run
(D65) now also runs `TestLargeCopyViaNetwork`; `TestSpliceFile`,
`TestGetPollFDAndNetwork` and `TestSendFile` pass their tcp subtests and stay
listed as "AF_UNIX: pending" for their unix ones (os: 245 passed, 36 skipped,
13 by the list, 0 failed, both arches). The only `ENOSYS` line in either run
is `pidfd_open` (D57).

Kernel changes the run needed:

- **Read-only `AF_NETLINK` / `NETLINK_ROUTE`** (`net/netlink.rs`; layouts in
  `patina_abi::netlink`; request parsing and reply encoding host-tested in
  `patina_inet::netlink`; `SockAddr::Netlink` for `sockaddr_nl`).
  `net.Interfaces`, `InterfaceAddrs`, `InterfaceByName/Index` and every test
  that looks for the loopback interface go through `syscall.NetlinkRIB`;
  without it the four interface tests failed outright. A send of a
  `RTM_GETLINK` or `RTM_GETADDR` dump queues `RTM_NEWLINK` / `RTM_NEWADDR`
  messages (`NLM_F_MULTI`, the request's seq, the socket's port id) packed
  into datagrams of at most 4 KiB, then `NLMSG_DONE`; `RTM_GETLINK` by
  index or name answers that one link; anything else gets `NLMSG_ERROR`
  `-EOPNOTSUPP`. Each `recv` returns one datagram (truncation and
  `MSG_PEEK` as for UDP). Interfaces come from the stack: `lo` (index 1,
  `UP|LOOPBACK|RUNNING|LOWER_UP`, MTU 65536, `ARPHRD_LOOPBACK`) with
  127.0.0.1/8 and ::1/128, and with a NIC `eth0` (index 2,
  `UP|BROADCAST|MULTICAST|RUNNING|LOWER_UP`, MTU 1500, `ARPHRD_ETHER`, the
  NIC's MAC) with the link-local and `ip=` addresses (IPv4 ones with
  `IFA_LOCAL`, `IFA_LABEL` and `IFA_BROADCAST`). `bind` with pid 0 (or the
  first send) assigns the process id, or a descending id from `0xfffff000`
  when another socket holds it; an explicit pid in use is `EADDRINUSE`.
  There are no multicast groups and nothing can be changed. `/proc/net/igmp`
  and `/proc/net/igmp6` list the all-hosts / all-nodes groups (and the
  solicited-node group of each non-loopback IPv6 address), in Linux's
  format: `TestInterfaceMulticastAddrs` wants an IPv6 group when a
  loopback interface and more than one IPv6 address exist.
- **`sendfile`** was `ENOSYS`. Go then falls back to a plain copy, but the
  net `TestSendfile*` tests check that `poll.SendFile` handled the write.
  It now reads from the input with `pread` (at `*offset`, or at the file
  position, which advances) and writes through the output's `write` with
  the usual blocking rules; only what was written counts as consumed. An
  unseekable input is `EINVAL`, as on Linux before 5.12.
- **`socket(AF_INET*, SOCK_RAW, …)`** is `EPERM`, which is what a process
  without `CAP_NET_RAW` gets (it was `ESOCKTNOSUPPORT`). The `TestIPConn*`
  tests check only for uid 0, and they skip their raw cases on `EPERM`
  through `testenv.SyscallIsNotSupported`.

The TCP/UDP semantics the suite exercises (dual stack, `[::ffff:…]`
forms, deadlines, close/shutdown races, `TestDialParallel`, the accept
abort tests, zero-byte reads and writes, UDP `ReadFrom`/`WriteTo` edge
cases, many short-lived connections) passed on the D67/D68 stack without
changes.

Post-merge note (supervisor): after D71 landed, the 34 "AF_UNIX: pending"
entries were removed from `testapps/skiplist/net.txt` and the three from
`os.txt`. One kernel fix was needed: an unnamed unix datagram sender must
yield no source address at all (`msg_namelen` 0, Linux `unix_copy_addr`),
not a 2-byte `sun_family`-only address that Go decodes as the abstract name
`@` (`TestReadUnixgramWithUnnamedSocket`, `TestUnixgramZeroByte*`,
`TestUnixgramLinuxAbstractLongName`). Final counts on both arches:
net 254 passed, 55 skipped (1 by our list: `TestSplice`), 0 failed;
net/http 611 passed, 10 skipped, 0 failed; os 248 passed, 33 skipped (10 by
our list), 0 failed.
## D74: Milestone B harness (test-gopkg.sh), package list and skip-list policy
*2026-10-14 · Phase 5*

`scripts/test-gopkg.sh <pkg>... [--arch aarch64|x86_64|both] [--cpus N]
[--run RE] [--no-short] [--mem MiB]` generalises the os and net runs (D65,
D73) to any std package; `test-os.sh` and `test-gonet.sh` are now thin
wrappers (their knobs map to the new flags; logs moved to
`out/test/gopkg/<pkg>-<arch>-cpuN.log`). Per package and arch it builds
the kernel, `go test -c` (static, `-trimpath`), and packs `/app`, the
package directory at `/src/<pkg>`, `$GOROOT/src/testdata` at
`/src/testdata` and `$GOROOT/src/go.mod` at `/src/go.mod`. With that file
`internal/testenv` finds GOROOT (`/`) by walking up from the cwd, which a
`-trimpath` binary otherwise lacks; `time`'s `TestLoadLocationFromTZData`
and net/http's `TestNoUnicodeStrings` then run. Per-package extras: `os`
gets the small `/usr/bin` of D65; `time` gets
`$GOROOT/lib/time/zoneinfo.zip` at `/lib/time/zoneinfo.zip` (its tests
load `../../lib/time/zoneinfo.zip`; no `ZONEINFO` is needed). Every run
has `patina.cwd=/src/<pkg>`, `HOME=/root`, a slirp NIC and the CLI's static
`ip=` line (harmless for non-net packages), and `-test.v -test.timeout
<t> -test.short -test.skip ^(…)$`. Defaults: 1 GiB, 900 s QEMU timeout,
`-test.timeout 13m`; `runtime` gets 2 GiB, 1500 s and 23m (1 GiB is enough
with `-short`; the margin is for `--no-short`). Overrides: `--mem` /
`PATINA_MEM` / `GOPKG_MEM_<pkg>`, `PATINA_TIMEOUT` / `GOPKG_TIMEOUT_<pkg>`,
`GOPKG_TEST_TIMEOUT` / `GOPKG_GOTIMEOUT_<pkg>`. Counting, the summary line
and the failure rules are those of D65, plus a final summary block.

Packages: design §13's `runtime`, `os`, `net`, `os/signal`, `time`,
`sync`, `syscall`, plus `io`, `bufio`, `context`, `net/http`,
`runtime/pprof` and `os/user`. All pass on aarch64 at 1 and 4 vCPUs (counts
in doc/status.md, **test-gopkg**); `runtime` also passes on x86_64 at 1
vCPU. The whole set takes a few minutes on aarch64/HVF.

Skip lists follow D65 (only tests that fail, one reason each, the list
only shrinks). They are `runtime.txt` (11), `os.txt` (10, unchanged),
`net.txt` (1, unchanged), `os_signal.txt` (2), `sync.txt` (2) and
`syscall.txt` (10). By category: "needs os/exec (no execve, D57)" 35 —
tests that re-execute the test binary or run `cat`, because
`testenv.MustHaveExec` does not probe on Linux; "GODEBUG re-exec" 1
(`runtime` `TestIssue66841`). No test needed a "/proc details",
"needs more RAM" or "too slow" entry. `syscall` `TestFcntlFlock` is listed
for exec only; its `F_SETLK` half failed first and was fixed. No test was
non-deterministic: three 4-vCPU `runtime` runs were identical.

Kernel changes the run needed:

- `recvmsg` reports `MSG_CMSG_CLOEXEC` back in `msg_flags` when the caller
  passed it, as Linux's `____sys_recvmsg` does (syscall
  `TestSCMCredentials`).
- Per-thread credentials. `Thread::cred` holds real, effective, saved and
  fs uid/gid, an all-or-nothing capability model and `keepcaps`;
  `Thread::groups` holds the supplementary groups. `clone` copies the
  parent's. The `set*uid`, `set*gid`, `setfs*id` and `setgroups` rules,
  including `cap_emulate_setxuid`, live in `patina_abi::cred` with host
  tests (Go's `TestSetuidEtc` sequence is one). `get*id`, `getres*id` and
  `getgroups` report them, `capget` reports the sets (it used to report
  empty sets, D57), and `PR_SET_KEEPCAPS`/`PR_GET_KEEPCAPS` work.
  `/proc/1/status` shows the main thread's `Uid`/`Gid`/`Groups`/`CapPrm`/
  `CapEff`. `/proc/1/task` lists the live threads, each `<tid>` directory
  with `status` (`Pid:` = tid), `stat` and `comm`; the directories are
  synced when `/proc/1/task` is opened. Nothing enforces file permissions:
  after dropping euid 0 the program can still do everything. Exposed by
  syscall `TestSetuidEtc` (needs `/proc/<pid>/task` and real `Uid:` lines)
  and `TestAllThreadsSyscall`, and by os/signal
  `TestAllThreadsSyscallSignals` (`PR_SET_KEEPCAPS` through
  `AllThreadsSyscall` was `EINVAL`).
- `fcntl` record locks (`F_GETLK`, `F_SETLK`, `F_SETLKW`, `F_OFD_*`) were
  `EINVAL`. They are now checked as `fs/locks.c` does: lock type, whence
  and range (`patina_fsutil::lock_range`, host-tested; `EINVAL` /
  `EOVERFLOW`), `EBADF` for a lock the open mode forbids, and `l_pid` 0
  for OFD locks. They are always granted, and `F_GETLK` answers `F_UNLCK`.
  With one process a POSIX lock never conflicts. OFD locks are treated
  like `flock` (D57), so two descriptions do not exclude each other
  (syscall `TestFcntlFlock`).
- `umount2` gives `EINVAL` for an existing path (nothing is mounted on
  the single filesystem) and `ENOENT` for a missing one, instead of
  `ENOSYS` (syscall `TestUnshareMountNameSpace` cleanup).

`ENOSYS` lines left in the runs are `pidfd_open` (D57) and `clone3`,
which the syscall tests use for namespaces and pidfds. Both stay `ENOSYS`
on purpose: Go falls back to `clone`, which cannot create a process (D57).
`cargo fmt --all --check` still reports `kernel/src/arch/aarch64/{dtb,its,
mod}.rs` and `kernel/src/drivers/virtio/pci.rs`. Those paths belong to
another change and were left alone.

x86_64 and 8-vCPU runs. All 13 packages pass on x86_64 (microvm, TCG) at
1 vCPU, and `runtime`, `os`, `net`, `syscall` and `os/signal` at 4 vCPUs,
with the aarch64 skip lists unchanged: no x86_64-only failure, so nothing
was added to a list. `runtime` passes at 8 vCPUs on both arches (aarch64
three times with identical pass/skip sets), as do `sync`, `os/signal` and
`runtime/pprof` on aarch64. `runtime --no-short` at 8 vCPUs needs more than
the 2 GiB default (about 20 tests run in parallel; the guest ran out of
frames) and passes with `--mem 4096`; the default stays, since the
`-short` run is the gate. Three syscalls that are `ENOSYS` only on a path
Go's `syscall` package reaches directly were cheap to add, though no
package test needed them: `select` (x86_64) and `pselect6` (both; Go's
`syscall.Select`), as a third client of `File::poll` with Linux's
`fd_set` rules (fdtest `ok select`), and x86_64 `utime` (`syscall.Utime`
issues `SYS_UTIME` on amd64) and `time` (fileio). The `ENOSYS` lines in
the x86_64 runs are the aarch64 ones: `pidfd_open` and `clone3`.

## D75: vDSO (clock_gettime, gettimeofday, getrandom) and the vvar page
*2026-10-14 · Phase 5*

Go reads the clock constantly (`nanotime` in the scheduler, `walltime` +
`nanotime` per `time.Now`) and, since 1.24, `crypto/rand` goes through the
vDSO's `getrandom`. Without `AT_SYSINFO_EHDR` each of those is a syscall.
patina now ships a small in-tree vDSO on both arches.

**Build.** `crates/vdso` (`patina-vdso`, a normal workspace crate, no new
dependency). Its `build.rs` compiles `image/vdso.rs` for
`x86_64-unknown-none` and `aarch64-unknown-none` with the pinned
toolchain's `rustc --emit=obj` (`-Copt-level=s -Cpanic=abort
-Crelocation-model=pic`, `#![no_builtins]`, `-Dwarnings`) and links each
object with the toolchain's `rust-lld -flavor gnu -shared -Bsymbolic
--hash-style=sysv -z rodynamic -z norelro --no-undefined -soname
linux-vdso.so.1`, `image/vdso.lds` and `image/<arch>.map` (version script).
Because only the crate's own object is linked, any call into `core`
(`memcpy`, a panic path) is an undefined symbol and fails the build; the
ChaCha20 block keeps its state in 16 locals for that reason. The crate
embeds both images (`IMAGE_X86_64`, `IMAGE_AARCH64`; `IMAGE` is the target
arch's) and the kernel `include`s nothing itself: it depends on
`patina-vdso`. `scripts/build-kernel.sh` and its callers are unchanged, the
kernel's clippy runs as before (the image is built by the dependency's
build script), and `cargo test -p patina-vdso` checks both images on the
host with a small ELF reader in the crate: ELF64 `ET_DYN`, one read-exec
`PT_LOAD` at vaddr/offset 0 with `filesz == memsz`, `PT_DYNAMIC` inside it,
`DT_HASH`/`DT_SYMTAB`/`DT_STRTAB`/`DT_SYMENT`/`DT_VERSYM`/`DT_VERDEF`, no
relocation tags or sections, no writable or NOBITS sections, every symbol
Go looks up resolving through the SysV hash table with the version check
of `runtime/vdso_linux.go` (and the SysV hash of each name equal to Go's
precomputed one), `__vvar_page` equal to where the kernel maps the vvar
page, and the image's ChaCha20 equal to `patina_rand::chacha` (RFC 8439).
Images are about 4 KiB: one text page.

**Symbols.** Exactly what Go looks up per arch: amd64 `__vdso_clock_gettime`,
`__vdso_gettimeofday`, `__vdso_getrandom` (plus `__vdso_time`,
`__vdso_clock_getres`) in version `LINUX_2.6`; arm64 `__kernel_clock_gettime`,
`__kernel_getrandom` (plus `__kernel_gettimeofday`, `__kernel_clock_getres`)
in `LINUX_2.6.39`. C ABI, leaf code, no data, no GOT.

**Layout and mapping.** The kernel maps the image's PT_LOAD pages read-exec
at `VDSO_BASE = 0x7fff_f800_0000` (above the initial stack, away from
Go's hints and the top-down mmap area; no randomization) as a `Vdso` VMA,
and the vvar frame read-only on the next page (the link script defines
`__vvar_page` at the page-aligned end of the PT_LOAD; the code reaches it
`rip`/`adrp`-relative) as a `Vvar` VMA. `AT_SYSINFO_EHDR` is the first auxv
entry. `/proc/self/maps` names them `[vdso]` and `[vvar]` (vdso first:
the order is the reverse of Linux x86, which nothing depends on). The vvar
frame is kernel-owned and shared: `Shootdown::free_frame` never frees it,
so `munmap`/`mremap`/`madvise(DONTNEED)` of it only drop the mapping, and a
fault in a `Vvar` VMA maps the same frame again instead of a zero page.
The image pages are ordinary frames of the address space. `mprotect` is
not restricted (a program could make its own vvar writable; there is only
one process). `patina.novdso=1` maps neither and omits
`AT_SYSINFO_EHDR`, for A/B measurements; the loader's log line says
`vdso on|off`.

**vvar and the seqlock.** `patina_vdso::vvar::Vvar` at offset 0 of the
page: `seq` (u32, odd while the kernel writes), `clock_mode` (counter or
none), `counter_base` (the counter at monotonic 0), `scale`,
`realtime_offset` (ns), `rng_generation`, `rng_ready`. All fields are
atomics written by the kernel through the direct map; writers are
serialized by a spinlock, make `seq` odd, `Release` fence, store, then
`seq += 1` with `Release`; readers load `seq` (`Acquire`, retry while
odd), the fields, an `Acquire` fence, and retry if `seq` changed. The
clock parameters are written once in the loader (after the TSC is
calibrated / with CNTFRQ); afterwards only `realtime_offset` changes, from
`wallclock::set_epoch_ns` (the control channel's `time` op), so a step of
`CLOCK_REALTIME` is visible to the vDSO immediately.

**Clock math.** No NTP-style slewing exists in patina, so there is no
`cycle_last`/`mult`/`shift` state: the vDSO uses the kernel's exact
formula, which now lives in `patina_vdso::vvar` and is called by the
kernel's `timer::now_ns` too: x86_64 `(tsc - base) * mult >> 32` (mult =
ns per tick, 32.32, from the PIT calibration), aarch64 `(c / f) * 1e9 +
(c % f) * 1e9 / f` with `f` = CNTFRQ_EL0. The vDSO and the syscalls
therefore agree to the nanosecond for the same counter value. Counter
reads: `lfence; rdtsc` (Linux's `rdtsc_ordered`) and `isb; mrs
cntvct_el0`. Fast clocks: `REALTIME`, `REALTIME_COARSE`, `TAI` (realtime;
the kernel has no TAI offset) and `MONOTONIC`, `MONOTONIC_RAW`,
`MONOTONIC_COARSE`, `BOOTTIME` (monotonic), as the kernel's own
`resolve_clock` maps them; anything else (CPU-time clocks, alarms, bad
ids) and the case where the kernel published no counter make the real
syscall from inside the vDSO (`syscall` / `svc #0`), so results and
errors are always the kernel's. `clock_getres` returns 1 ns for the fast
clocks, as the syscall does. CR4.TSD is now explicitly cleared on every
CPU (it was clear by default; Go's `cputicks` already used RDTSC).

**TSC assumption (x86_64).** The vDSO assumes the TSC is synchronized and
constant-rate across CPUs, as `timer::now_ns` already did: true under
QEMU TCG (one host clock) and on KVM with an invariant TSC. patina does
not check CPUID 0x8000_0007 EDX[8] (invariant TSC) and uses the TSC
regardless; on a host whose vCPUs' TSCs drift, monotonic time could step
backwards across CPU migrations by the skew, in the kernel and the vDSO
alike. Firecracker/Cloud Hypervisor (KVM, kvmclock-capable) are the likely
places to meet this; a kvmclock/pvclock clock mode would be the fix.

**getrandom: implemented** (the Linux 6.11 contract). The parameter query
`getrandom(NULL, 0, 0, &params, ~0UL)` reports a 144-byte opaque state,
`PROT_READ|PROT_WRITE` and `MAP_PRIVATE|MAP_ANONYMOUS` (no
`MAP_DROPPABLE`: patina's mmap does not know it; a reclaimable mapping
does not matter without swap). The state is `batch[96] ‖ key[32] ‖
generation ‖ pos ‖ in_use`. A call with a state of another size,
unsupported flags (anything but `GRND_NONBLOCK`/`GRND_INSECURE`), a
misaligned or null state, `rng_ready == 0`, or a re-entered state
(`in_use`, e.g. from a signal handler) takes the syscall. Otherwise: if
`state.generation != vvar.rng_generation` the key is refilled with a
32-byte `getrandom` syscall and the batch emptied; output comes from the
batch (consumed bytes are wiped with volatile stores), then whole
ChaCha20 blocks straight into the buffer (64-bit counter from 0, zero
nonce), then two more blocks overwrite `batch ‖ key` (fast key erasure:
the key that produced the output is gone when the call returns); if the
generation changed during the call the whole output is regenerated with
a fresh key. Lengths are capped at `MAX_RW_COUNT` like the syscall. The
kernel bumps `rng_generation` after every reseed of its DRBG
(`random::seed_boot`, `add_entropy_and_reseed`, i.e. the periodic and the
1 MiB virtio-rng reseeds) and starts it at 1 so fresh zeroed states fetch
a key on first use. Stack use: Go calls `vgetrandom1` on the goroutine
stack (NOSPLIT, no g0 switch), so the deepest path is kept at about 300
bytes (getrandom 96 + the out-of-line block function 208 on both arches,
checked with `llvm-objdump`).

**Measured** (`testapps/vdsobench`, 1 vCPU, release; aarch64 = HVF,
x86_64 = TCG; `RESULT` lines, vDSO / `patina.novdso=1`):

| | aarch64 | x86_64 |
|---|---|---|
| `time.Now()` (walltime + nanotime) | 63.4 ns / 333.5 ns (5.3×) | 361.9 ns / 3004 ns (8.3×) |
| `crypto/rand.Read` 32 B | 120 ns / 569 ns (4.7×) | 1139 ns / 3837 ns (3.4×) |
| raw `clock_gettime` syscall (reference) | 157–232 ns | 1318–1380 ns |

With 4 vCPUs: 63.6 ns (aarch64) and 437.6 ns (x86_64) per `time.Now()`.
Under `patina.strace=1` the 1e6-call `time.Now` loop and the 1e5-call
`crypto/rand` loop contain no `clock_gettime`, `gettimeofday` or
`getrandom` syscalls on either arch (all 104000 `clock_gettime` lines in
the log are the test's own syscall comparisons); the whole run makes one
vDSO key-fetch `getrandom(…, 32, 0)` at the first `rand.Read`, and one more
after the kernel reseeds (forced by 1.1 MiB of syscall `getrandom`).

**Unverified.** KVM hosts (TSC stability, see above; the HVF aarch64 runs
use the real CNTVCT); the `in_use` re-entry path (Go never calls getrandom
from a signal handler); the vvar refault after `madvise(DONTNEED)` and
`munmap`/`mremap` of the vDSO (no test does it); Go's SIGPROF traceback
through vDSO frames under heavy profiling (the `pproftest` case of
`test-runtime.sh` passes, but it does not target vDSO PCs).

## D79: Benchmarks and the Go version matrix
*2026-10-14 · Phase 5*

**What is measured.** `testapps/bench` (std only) prints one
`bench <name>: <value> <unit>` line per benchmark and `PASS bench`. Each
benchmark warms up with a tenth of its count, then runs a fixed count
(latency rows: ns or µs per op) or a fixed duration (throughput rows: 1 s
for pipes, 2 s for TCP/AF_UNIX, 64 KiB writes, MB/s = 10^6 B/s); `-n F`
scales both, `-only` picks rows. Rows: `getpid` through `RawSyscall` (bare
entry/exit) and `Syscall` (+ Go's `entersyscall`/`exitsyscall`); raw
`clock_gettime` and 32-byte `getrandom` syscalls against `time.Now()` and
`crypto/rand.Read` (vDSO, D75); a raw `FUTEX_WAIT`/`FUTEX_WAKE` ping-pong
between two `LockOSThread`ed goroutines, plus channel and
`sync.Mutex`+`Cond` ping-pongs for context; a blocking-pipe ping-pong
between two locked threads and pipe throughput; an `os.Pipe` ping-pong
(non-blocking fds through Go's netpoller: the epoll wakeup path); TCP
(`TCP_NODELAY`) and AF_UNIX round trip and throughput over loopback; a
locked goroutine that exits (Go kills its M: one clone + one thread exit);
1 MiB anonymous `mmap` + one touched page + `munmap`; `kill(self,
SIGUSR1)` to a `signal.Notify` channel. The raw ping-pongs raise
GOMAXPROCS to 2 for their duration: with one P, the thread blocked in the
futex would hold it and the partner could only run after sysmon retakes
it (≥ 20 µs), which measures Go, not the kernel. `scripts/bench.sh` builds
kernel and program, runs each (arch, vCPU count) 3 times under
`run-qemu.sh` (`PATINA_MEM=512`, 600 s timeout), with `--novdso` reruns
the clock rows with `patina.novdso=1`, and prints medians. The host runs
other agents' guests concurrently, so a single run can be 10–30 % off; the
median of 3 is what `doc/status.md` records. x86_64 is TCG on this host:
its numbers are an emulator's, kept to catch regressions and ratios only.

**Results (aarch64, HVF, 1 vCPU).** Syscall entry/exit 57 ns; vDSO
`time.Now` 60 ns vs 266 ns without (Go then makes two syscalls per call);
futex round trip 0.65 µs; pipe ping-pong 1.0 µs; TCP RTT 4.3 µs, 690 MB/s;
AF_UNIX RTT 1.9 µs, 2.8 GB/s; thread create+exit 3.8 µs; signal round
trip 3.2 µs. None reaches the pathology thresholds set for this task
(futex > 50 µs, pipe ping-pong > 100 µs, TCP loopback < 200 MB/s, signal >
100 µs on HVF), so no kernel change came out of it.

**Known costs (explained, not fixed).** (1) TCP loopback carries ~4× less
than AF_UNIX and has ~2.3× its round trip: every segment goes through
smoltcp (segmentation, checksums, ACK processing) under the single stack
lock and the net task (D67), while AF_UNIX has no protocol stack in the
path (D71). (2) With 2 vCPUs, `signal-rtt` (3.2
→ 6.9 µs) and `thread-create-exit` (3.8 → 6.6 µs) roughly double on HVF:
the woken thread is placed on the other CPU, which costs a Reschedule IPI
to a CPU idling in `wfi` (an HVF exit and re-entry); futex and pipe
ping-pongs grow less (0.65 → 0.90, 1.0 → 1.4 µs). (3) x86_64
`mmap-munmap-1M` at 2 vCPUs is 3× the 1-vCPU cost: `munmap` on x86 sends
an `IpiKind::TlbFlush` IPI to the other CPU (`mm/tlb.rs`), which TCG
makes expensive; aarch64 uses broadcast `tlbi …is` and shows no change.

**Go version matrix.** Design §13 asks for the latest two Go releases.
`scripts/build-testapp.sh` takes `GO=<go command>` (default `go`, so no
behaviour change); go1.26.9 was installed through `golang.org/dl` outside
the repo. hello, threads, fdtest, fileio, signals, sockets, unixsock,
httphello `-selftest` and bench built with go1.26.9 pass on both arches
under `patina.strace=1`, as with go1.27.1; the ENOSYS set is the same for
both (`copy_file_range`, silent, Go falls back) and the distinct-syscall
sets match up to timing noise. `scripts/test-os.sh aarch64` with go1.26.9
first in `PATH` (test-gopkg.sh calls plain `go`; the knob does not reach
it, a PATH shim does) gives 247 passed / 0 failed, ENOSYS `pidfd_open`
only; 1.27 has one more top-level test (248).

**Static diff of Go 1.27.1's syscalls against the dispatch table.** Every
`SYS_*` number referenced by `runtime/sys_linux_{amd64,arm64}.s`,
`internal/runtime/syscall/linux`, `internal/syscall/unix` (incl. its
`*Trap` numbers), `syscall` (incl. `_SYS_clone3`, `_SYS_faccessat2`,
`_SYS_fchmodat2`), `os`, `os/signal`, `net`, `internal/poll`,
`crypto/rand` and `time` was compared with `kernel/src/syscall/mod.rs`.
Nothing a normal `os`/`net`/runtime path issues is missing. Not
implemented (fall to `enosys`, logged once): `inotify_init`/`inotify_init1`/
`inotify_add_watch`/`inotify_rm_watch` (D57; `syscall` wrappers only, no
std path uses them), `select` (amd64) and `pselect6` (arm64) — only
`syscall.Select`, no std path, but third-party code calling it gets
ENOSYS, the most plausible gap; `utime` and `ustat` (amd64 legacy
wrappers; `os.Chtimes` uses `utimensat`); privileged or exec-only calls
`ptrace`, `mount`, `pivot_root`, `unshare`, `acct`, `adjtimex`,
`settimeofday`, `reboot`, `sethostname`, `setdomainname`, `syslog`,
`ioperm`, `iopl`. `fcntl64` and the `*32` id calls are 32-bit-only names.
ENOSYS by design and handled by Go: `pidfd_open`, `pidfd_send_signal`,
`clone3`, `openat2`, `copy_file_range`, `execve`, `rseq`, `membarrier`,
`set_robust_list` (D57).

## D77: Tickless idle
*2026-10-14 · Phase 5*

An idle CPU no longer takes the 100 Hz tick. When `sched::idle_loop` is
about to wait with nothing runnable (`enter_tickless`), it runs a deferred
TLB `reclaim()`, turns its periodic tick off (`timer::set_periodic(false)`:
aarch64 programs CNTV_CVAL only for the deadline and disables the timer
when there is none; x86_64 re-arms the LAPIC one-shot for the deadline or
writes a zero initial count) and arms its one-shot for the earliest
deadline it *owns*. It turns the tick back on, on its own `TICK_HZ` grid,
when it switches to a thread. Running threads keep the tick, so the
20 ms quantum, `need_resched` and tick-granular CPU-clock timers are
unchanged.

Ownership is what keeps every deadline covered without waking every idle
CPU for every deadline. A sleep-list entry records the CPU that added it
(`sleep::next_deadline_for`). A wall-clock POSIX timer and `ITIMER_REAL`
record the CPU that last armed them (`timers::next_wall_deadline_for`).
A busy CPU keeps ticking and arms the global earliest deadline
(`sched::earliest_deadline`: sleepers and wall timers), as before; an idle
CPU arms only its own. Since every deadline has an owner that is either
busy (ticking, armed for the global earliest) or idle (armed for it), each
one is honoured at its first timer interrupt or after, never early. A
periodic timer that another CPU re-armed costs its idle owner at most one
early wake-up, after which it re-arms for the new expiry.

Nothing else needs an idle CPU's tick: per-thread CPU time is accounted at
context switches (`run_start`); CPU-clock timers and `ITIMER_PROF` /
`ITIMER_VIRTUAL` only advance while some thread runs, and that CPU ticks;
the deferred TLB reclaim (D52) also runs at every vm operation, before
going idle and from `/proc/meminfo`, and idle CPUs still get `TlbFlush`
IPIs; wake-ups, signals and stops reach an idle CPU by IPI.

Wall-clock timers became sub-tick deadlines in the same change, which D51
listed as a follow-up. `timers::WALL_NEXT` (the earliest wall-clock expiry,
in monotonic ns) is recomputed whenever the table changes, and
`timer_settime`/`setitimer` arm it on the calling CPU
(`sched::arm_deadline`). A 500 µs `timer_settime`, `CLOCK_REALTIME`
absolute timer or `setitimer(ITIMER_REAL)` now fires within about 0.6 ms
(1.4 ms on TCG) instead of at the next 10 ms tick. aarch64's
`ns_to_count` rounds up, so a deadline interrupt never arrives before
`now_ns() >= deadline`.

Overrun accounting was also fixed. The expiry pass used to reset the
accumulated overrun and set `timer_getoverrun`'s value before it knew
whether the signal was queued, so every refused send left the count at 1.
Now both are settled after the send: a queued signal resets the
accumulation and becomes the `timer_getoverrun` value, and a refused one
adds 1 + the missed intervals. patina still reports the overrun of the
last *queued* signal, where Linux reports the last delivered one.

`patina.stats=1` prints per-CPU counters at exit (timer interrupts, ticks
and how many were taken in the idle loop, idle waits, switches) and a
memory line (heap, kernel stack slots, TLB-pending frames, threads).
Measured with **test-robust** `idle` (4 vCPUs, three 1 s sleeps then 3 s
idle, about 7 s of uptime): 0–42 timer interrupts per CPU on aarch64 and
x86_64, against about 700 per CPU with the old tick. Verified by
**test-robust** (`default`, `idle`, `restart`), **test-signals**,
**pproftest** (94–201 samples) and **test-smp** (sleep-latency unchanged:
200 × `Sleep(500µs)` in 249–264 ms).

## D78: Memory commit accounting and ENOMEM
*2026-10-14 · Phase 5*

Design §15 asks that `mmap` beyond RAM fail with ENOMEM. Before this
change, all anonymous memory was lazy: a Go program allocating past RAM got
`[patina] out of memory filling user page`, then SIGSEGV `SEGV_ACCERR`
(the address is inside a VMA), and Go printed a misleading
`fatal error: fault`. Fault time is too late to report ENOMEM, and Go
cannot recover from a fault there.

`mm::vm::AddressSpace` now keeps a commit charge: the pages of every
writable private VMA, resident or not. `MAP_NORESERVE` mappings and the
vvar page are not charged, and neither are `PROT_NONE` and read-only
mappings, so Go's huge `PROT_NONE` arena reservations cost nothing until
`sysMap` makes them writable. The charge changes when a VMA is inserted
or removed and when `mprotect` changes permissions. `mmap` (including a
`MAP_FIXED` replacement, net of what it replaces), `mprotect` adding write
access and `mremap` growth fail with ENOMEM if they would push the charge
past `commit_limit`. A refusal logs one line:
`[patina] mmap: N KiB refused, commit limit reached (X of Y KiB committed, D78)`.
The loader sets the limit after mapping the image: the free frames plus
the image pages the program already holds, minus a kernel reserve of 1/16
of RAM clamped to 4–64 MiB. The reserve covers page tables, kernel stacks,
pipe and socket buffers and heap growth. `/proc/meminfo` reports
`CommitLimit` and `Committed_AS`. This is close to Linux's strict mode
(`vm.overcommit_memory=2`) with a high ratio. Unlike that mode,
`MAP_NORESERVE` is honoured, as in Linux's default heuristic mode. A
program that maps more than RAM on purpose has to say so, which it would
also have to do on a Linux machine of that size.

Go hits the limit during heap growth, gets ENOMEM from `mmap`, and dies
through its own path: `runtime: out of memory: cannot allocate …` and
`fatal error: runtime: out of memory`, exit 2. A page fault can still find
no free frame, for example when a `MAP_NORESERVE` mapping is touched past
RAM or when kernel allocations used the reserve. That backstop
(`vm::Fault::OutOfMemory`) kills the program like Linux's OOM killer: one
`[patina] out of memory: no free frame for user page … (free/total frames,
committed/limit pages); killing the program (SIGKILL)` line, then a forced
SIGKILL, `[patina] exit signal SIGKILL`, exit 137, reported through the
normal exit path and control-channel event. Kernel-mode user accesses
(uaccess) that cannot fill a page still return EFAULT/ENOMEM to the
syscall.

`VmRSS` in `/proc/self/status` now counts the program's resident pages.
It used to count every frame in use, kernel ones included.

Verified by **test-robust** `oom` (`PATINA_MEM=256`: the third 64 MiB
chunk is refused, `fatal error: runtime: out of memory`, exit 2; both
arches) and `oomfault` (1 GiB `MAP_NORESERVE` touched page by page: the
`[patina] out of memory` line, SIGKILL, 137). Mappings larger than RAM
in tests must now use `MAP_NORESERVE`, as robust's page-table reclaim
check does.

## D80: Kernel stack and page-table frame reclaim
*2026-10-14 · Phase 5*

Before this change, kernel stacks were never returned to the frame
allocator (D39): freed slots stayed mapped on a free list, so a burst of
2000 threads pinned 64 MiB for the life of the VM. User page tables were
never freed (D36/`uspace` docs): `munmap` of a large, sparsely touched
range kept every PT/PD page. `selftest vm` reported 7 frames held by page
tables.

Kernel stacks: `kstack::free` (run by `sched::finish_switch` on the CPU
that switched away from the dead thread, so nothing runs on the stack any
more) keeps up to `POOL_KEEP` = 32 slots mapped for cheap reuse. Past that
it unmaps the stack pages (`paging::unmap_page`) and frees the frames:
* aarch64: `TLBI VAAE1IS` + `DSB ISH` invalidates the page on every CPU
  before `unmap_page` returns, so frames and slot are reusable at once.
* x86_64: kernel pages are global, and the D52 shootdown (a CR3 reload)
  does not flush global entries. Stack pages are therefore mapped with
  the new `MapFlags::LOCAL` (no `PTE_G`). The frames go through a
  `Shootdown` and are freed once every CPU flushed, and the slot records
  the shootdown generation. It is reused only once
  `tlb::generation_reached(g)`: a CPU still caching the old stack VA
  would otherwise reach the old frame through the new stack.
Kernel page-table changes in the window are serialized by a `KMAP` lock;
`map_in` has none of its own. Page tables of the window are kept,
bounded by the peak number of stacks.

User page tables: `release_frames` (munmap, `MAP_FIXED` replacement,
madvise `DONTNEED`/`FREE`, mremap shrink) prunes, for each 2 MiB region
that had resident pages, the level-3 table and then its parents while
they are all-zero, never the root (`UserTables::prune`). PROT_NONE
entries carry their frame and are not zero. Before the freed table frames
are handed to the `Shootdown`, `uspace::flush_table_walks` runs: on
aarch64 `TLBI ASIDE1IS` (all user ASID-0 entries and walk caches, all
CPUs) makes immediate freeing safe; on x86_64 a local CR3 reload, with
other CPUs covered by the deferred free (D52 ordering: entries cleared →
generation bump → frees after every CPU's CR3 reload, which also drops
paging-structure caches).

Verified by **test-robust** `reclaim` at 1 and 4 vCPUs on both arches.
Over 5 × 2000 thread exits, after a warm-up round, the frames the kernel
holds (MemTotal − MemFree − VmRSS) moved by −10 to +8. MemFree alone
drifts with what Go's allocator keeps resident. A 1 GiB `MAP_NORESERVE`
range touched every 2 MiB held 513 page-table frames, and all of them came
back on `munmap` (kernel-held delta 0). `selftest vm` now reports 1 frame
held by page tables, the root. `selftest sched` counts slots relative to
background kernel threads: it had failed since the RNG reseeder (D60)
took a slot, and nobody re-ran it.
## D83: Apple VZ drops the first console tx buffer available at device start; VZ virtio-net needs F_MTU
*2026-10-14 · Phase 5 (Milestone A on VZ)*

`drivers/virtio/console.rs`, `virtio/pci.rs`, `virtio/net.rs`. The first
real runs of the Phase 3/4 drivers on Apple Virtualization.framework
(`patina run --vmm vz`, macOS 26) booted, printed on `hvc0`, attached the
vsock control channel and reported exit codes — except that ~15% of boots
printed nothing at all although the program ran. Diagnostics sent over the
(working) vsock channel showed the hvc0 transmit queue with `avail_idx` 1,
`used_idx` 0 forever, device status 0xf and a correctly configured queue;
re-notifying every 1 ms did nothing, but a second buffer posted later was
returned (only it) and output resumed behind the lost one. Measured with a
probe posting 1-byte buffers: VZ starts its console device model
asynchronously after `DRIVER_OK` (normally 10–50 µs later, up to ~12 ms
with every host CPU busy) and at that moment consumes exactly one buffer —
the first available on the queue — without writing or returning it; buffers
published afterwards, and all later ones, are handled normally. Drop rates
for a single buffer published x after `DRIVER_OK` (30 boots each, idle host,
1/2/4 vCPUs): before `DRIVER_OK` 60/60 (so it is the start, not queue
enable); 0 µs 30/30; 10 µs 11–14/30; 20 µs 5–9/30; 30 µs 3–5/30; ≥ 50 µs
0/30; with the host saturated still 4–17/30 between 30 µs and 1 ms, which
rules out a fixed delay. VZ's rng (requests before and right after
`DRIVER_OK` answered in 8–47 µs), vsock (tx at t=0 returned; rx buffers
posted before or 30 ms after `DRIVER_OK` both used) and net (nothing
dropped; first completion 27–300+ ms after `DRIVER_OK`) drop nothing. Our
first hvc0 transmit is the early-ring replay of `console::switch_to`, a few
microseconds after `DRIVER_OK`, and once a buffer is in flight every later
write is dropped behind it (D61), so one lost buffer silenced the console.

Fix: the transport flags VZ devices (`Transport::apple_vz()`: function 0 of
the host's first bus has Apple's vendor id 0x106b — VZ's host bridge is
0x106b:0x1a05; the discovery line says `apple host`). On VZ every console
transmit queue (port tx and control tx) gets one sacrificial 1-byte buffer
(`\n`) before `DRIVER_OK`, which is therefore the one VZ drops. Its
descriptor is never reused and `TxQ::reap` ignores a completion for it, so
a future VZ that returns it costs the host one newline. No delay, no
timeout. Verified: 200 consecutive hello runs at 2 vCPUs, 20 each at 1 and
4 vCPUs and 80 with the host saturated, 0 silent runs, exit codes right;
another 40/40 on the merged tree; QEMU `test-ctl` (mmio and `PATINA_VIRTIO=pci`),
`test-hello` with `hvc0`, `test-net` and `patina.selftest=net` unchanged.
The temporary diagnostics (boot delay / settle knobs, a re-kick helper that
alone still failed 7/60) were removed. Console rx on VZ is unverified (VZ
attaches no guest input).

Separately, VZ's virtio-net (00:01.0, modern 0x1041) offers features
0x5300119ab and fails `FEATURES_OK` unless `VIRTIO_NET_F_MTU` (bit 3) is
accepted; with D69's MAC + STATUS + VERSION_1 set the driver logged `device
rejected the feature set` and the guest had loopback only. The driver now
also accepts `F_MTU` (amending D69), reads the device MTU at config offset
10 and logs it (`link up, mtu 1500 (pci 0000:00:01.0, hdr 12 B)`), warning
when it is below the stack's 1500. QEMU's devices do not offer it. On VZ
NAT, DHCP leases arrive in 0.2–0.5 s.

## D84: `PATINA_VMM=vz` for the test scripts, `patina image pack`, the VZ timing check
*2026-10-14 · Phase 5 (Milestone A on VZ)*

Every suite script goes through `scripts/run-qemu.sh`, so the VMM switch
lives there: `PATINA_VMM=vz` (default `qemu`) execs the new
`scripts/run-vz.sh` with the same arguments, which packs the kernel and
initrd into a temporary image with the hidden CLI subcommand `patina image
pack --kernel K --initrd I --target vz-aarch64 -o X` (no Go build; an empty
newc archive when there is no initrd) and runs `patina run --vmm vz --cpus
$PATINA_CPUS --mem $PATINA_MEM --timeout ${PATINA_TIMEOUT}s --grace 5s -v`
under an outer `timeout` 30 s longer. Kernel params travel as
`PATINA_PARAMS` (which `patina run` reads; raw `k=v` words among the args
are moved there, since the CLI percent-encodes `=` in real args), the exit
status is the guest's code / 124 on timeout / 125 without a code, the
console goes to stdout and `PATINA_LOG`. Mapped: `PATINA_NET=user` →
`--net user` with the caller's `ip=` dropped (VZ NAT is DHCP, not slirp's
10.0.2.x); `PATINA_CTL_SOCK` → a symlink to the run's `ctl.sock` proxy (raw
NDJSON like QEMU's chardev socket, replaying `ready` to late clients;
`test-ctl.sh` therefore checks early `log` events on serial only and runs
the panic case without a client, since nothing holds the VM until a client
attaches); `PATINA_VZ_PARAMS` adds params on the VZ path only. Not
mappable, warned about and ignored: `PATINA_NET=user:H-G` (no port
forwarding on VZ: the host reaches the guest at its leased address — an
error), `PATINA_CONSOLE` (always `hvc0`), `PATINA_CTL_WAIT`, `PATINA_ACCEL`,
`PATINA_MACHINE`, `PATINA_VIRTIO`, `PATINA_QEMU_EXTRA`. `test-net.sh` stays
QEMU-only (`--publish`, socket netdevs); `scripts/test-vz.sh` is the VZ
milestone script (D85). Results on VZ: see doc/status.md (test-runtime
33/33 at 1/2/4 vCPUs, fileio/sockets/unixsock, test-ctl 8/8, os 248/33/0,
net 254/55/0 — the same counts as QEMU).

The timing oddity recorded in doc/cli-contract.md (a 0 s sleep taking 2.4 s
at 2 vCPUs, 1 s taking 3.4 s at 1 vCPU) was measured with the driverless
kernel and does not exist with the drivers: `testapps/sleeper` (Sleep(N),
200 × Sleep(500 µs), ticker, cross-thread ping-pong, raw `nanosleep`/
`epoll_pwait` timing, `-spin` for a CPU-bound competitor) at 1/2/4 vCPUs on
VZ and QEMU hvf, 3 runs each: kernel time to `main` 3–4 ms on VZ (29 ms on
QEMU), `Sleep(1s)` = 1000.3–1001.0 ms everywhere, vCPU counts within 1 ms
of each other; `patina.selftest=smp`: secondaries start in 75–95 µs (2
vCPUs) / 236–257 µs (4) on VZ vs 99–109 / 386–390 µs on QEMU, a +2 ms
deadline fires at 2003–2005 µs, past deadlines at 3 µs, every IPI
delivered; `selftest=timer` 5 ticks in 51–52 ms on both; the 100 Hz tick
costs no wall time; the virtual timer (PPI 11, INTID 27) is used. What
remains is host-side and identical on both VMMs: a vCPU woken from WFI by
its own timer wakes ~20–25% late below 2 ms (nanosleep 500 µs → 0.60 ms,
1 ms → 1.2 ms, 2 ms → 2.3–2.5 ms; 10 ms → 10.1–10.9 ms), exact when another
vCPU is busy (its deadline fires on time and IPIs the sleeper) — macOS
timer coalescing on the idle vCPU's wake, not patina; spinning instead of
WFI under 2 ms would cost host CPU and is not done. Go's `Sleep(500µs)`
lands at ~1.2 ms because the netpoller rounds up to `epoll_pwait(1 ms)`.
Noted for the scheduler (not VZ-specific, identical on QEMU): at 1 vCPU a
thread woken from `nanosleep` while a CPU-bound thread runs is late by up
to a quantum (500 µs → 1.1 ms avg, 12 ms worst; 10 ms → 16.5 ms avg).

## D85: Milestone A on Apple VZ: NAT networking, guest-address reporting, gateway ARP, no NIC tx drops, fast control attach
*2026-10-14 · Phase 5 (Milestone A on VZ)*

`--net user` on VZ attaches virtio-net (00:01.0, modern 0x1041) on
`VZNATNetworkDeviceAttachment` with `ip=dhcp`; VZ's bootpd leases
192.168.64.0/24 with gateway and DNS at .1 (DISCOVER at ~80 ms, OFFER
~200–500 ms later, ACK at once), and INTx (SPI 32 + slot, level) delivers
the NIC's interrupts (ping RTT 0.3–1.3 ms; frames are answered in the
millisecond they arrive, no 1 s net-task poll stalls). VZ has no port
forwarding, but the host reaches the guest at its leased address, so the
CLI must learn it: the kernel now emits the lease line as a control-channel
`log` event at level `info` (`{"ev":"log","level":"info","msg":"net: dhcp
lease A/P via G, dns D… (N ms)"}`, the console line minus `[patina] `; a
changed lease says `net: dhcp lease changed: …`; `ctl::log_info` beside
`log_warn`, no new event type), possibly before `ready`. `patina run`
prints `patina: guest address A (gateway G)` on stderr once per new
address (from the event, else parsed from the `[patina] net: dhcp lease`
console line, `cli/internal/run/lease.go`), records
`"net":{"addr","prefix","gateway","dns"}` in `state.json`, and `patina ps`
gained an `ADDR` column.

Two stack bugs came out of measuring from the host. (1) The macOS NAT host
drops the packet that triggers its own ARP resolution of the guest, so
every first connection paid a 1 s SYN retransmit (the guest's ARP reply
went out in the same millisecond): after the first lease the kernel sends
one ARP request for the gateway (`MuxTx::arp_request`; the target of a
request creates an entry for the sender, RFC 826 / BSD `in_arpinput`), and
the first ping is now 1.3 ms. (2) `Mux::transmit` handed smoltcp a token
while the virtio tx ring was full, `route` dropped the frame (counted in
`nic_tx_drops`) and TCP waited smoltcp's 1 s minimum RTO — a 4 MiB `/big`
took 1.02 s instead of ~15 ms in about half the runs. `transmit` now
returns no token while `!nic.can_send()` (the contract net/device.rs always
stated); the transmit-completion interrupt wakes the net task. Both apply
to QEMU too (`test-net.sh` 14/14 unchanged).

CLI: the control hub dialled the VZ vsock port every 100 ms, so a program
that exits within ~100 ms of boot (hello) powered off before the host
connected and its `ready`/`exit` events were never delivered (the exit
code still came from the sentinel). It now retries every 5 ms until the
first connection (a refused connect costs ~1 ms on VZ), 100 ms for
reconnects; hello attaches 40/40. After the guest stops, `run` waited a
fixed 300 ms for an exit event and the VZ driver drained the console for a
fixed 250 ms; now `Hub.Quiesce` returns as soon as the live connection's
buffered events are delivered (at once if none attached; 300 ms cap) and
the drain stops once the console pipe has been quiet for 20 ms after the
stop event (250 ms cap). hello's wall time fell from ~480 ms to ~185 ms.
The exit-code precedence (D66) is unchanged.

Verified on this Mac (macOS 26): `cli/scripts/vztest.sh --e2e`
(`TestVZE2E`: hello ×3 with attach + ready + exit events, stop
handler/nohandler/forced, ping, `--net user` GET / (connect 2.6–7 ms,
first byte 3.5–16 ms), 20 concurrent GETs, `/big` 4 MiB sha256 in 13–21 ms
(~200–310 MB/s), stop → 0, `-nohandler` → 143, `-selftest -dns example.com`
through 192.168.64.1), `scripts/test-vz.sh`, `test-net.sh aarch64 x86_64`
14/14, QEMU CLI e2e 20/20, `go test ./...`. Known host-side effect: right
after macOS creates the NAT bridge (no other NAT VM running) the first
connection can be delayed by seconds while the host ARPs unrelated
addresses (the guest answers in < 1 ms); the e2e test treats the first
request as informational. Also noted: every accepted TCP connection sends
one 1-byte segment right after the handshake (smoltcp keep-alive
handling, harmless, not fixed).
