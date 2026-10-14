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
