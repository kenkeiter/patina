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
`sched::arm_deadline`. Lock order: `TABLE` → `THREADS` (process CPU clock);
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
