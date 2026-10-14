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
