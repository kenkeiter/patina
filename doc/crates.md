# Crate audit notes

One section per third-party dependency: version, why we need it, `no_std`
status, and its unsafe surface. Versions are what `Cargo.lock` resolves to;
the manifest pins the major version.

## bitflags

- **Version:** 2.13.x (`bitflags = { version = "2", default-features = false }`)
- **Used by:** `patina-abi`; `patina-kernel` (`arch::paging::MapFlags`,
  `bitflags = "2.9"` in `kernel/Cargo.toml`, same resolved version)
- **Why:** type-safe flag sets for Linux ABI flag words (`OpenFlags`,
  `MapFlags`, `ProtFlags`, `CloneFlags`, `SaFlags`, `EpollFlags`,
  `MsgFlags`) with `from_bits_retain` for user-supplied values that may carry
  unknown bits.
- **no_std:** yes, unconditionally; no default features are enabled (no
  `serde`/`arbitrary`/`std`).
- **Unsafe surface:** none in the generated code we use; the macro expands to
  plain integer newtypes. No proc-macro (it is a `macro_rules!` crate), so no
  build-time code execution.

## zerocopy (+ zerocopy-derive)

- **Version:** 0.8.x (`zerocopy = { version = "0.8", default-features = false, features = ["derive"] }`)
- **Used by:** `patina-abi`
- **Why:** byte-safe conversion of `#[repr(C)]` ABI structs to and from user
  memory (`IntoBytes::as_bytes`, `FromBytes::read_from_bytes`) without
  hand-written `unsafe` transmutes. The `IntoBytes` derive also refuses
  structs with implicit padding, which forces every ABI struct to spell out
  its padding fields — a useful guard against layout mistakes (the layout
  tests are the primary guard).
- **no_std:** yes; `default-features = false` drops `alloc`/`std`
  integration.
- **Unsafe surface:** zerocopy is built on `unsafe` internally (pointer casts
  justified by its trait invariants) and the derives emit `unsafe impl`s
  after checking layout at compile time. It is maintained by Google, heavily
  fuzzed, and formally reasoned about (Kani proofs in-tree); it is the
  standard choice for this job. `patina-abi` itself is
  `#![deny(unsafe_code)]`. `zerocopy-derive` is a proc-macro (runs at build
  time on the host) and pulls in `syn`/`quote`/`proc-macro2`, which are
  build-only dependencies and are not linked into the kernel.

## buddy_system_allocator

- **Version:** 0.13.0 (`buddy_system_allocator = { version = "0.13", default-features = false }`)
- **Used by:** `patina-kernel` (`kernel/src/mm/heap.rs`)
- **Why:** the kernel heap (`#[global_allocator]`). Only `Heap<32>` is used,
  wrapped in the in-tree IRQ-safe `SpinLock` (`kernel/src/sync.rs`). The
  crate's `LockedHeap` is not used: it needs the `use_spin` feature and its
  lock does not mask interrupts. The crate's `FrameAllocator` is not used
  either (it needs a heap); physical frames come from the in-tree buddy
  allocator in `kernel/src/mm/frame.rs`. The heap is seeded with 8 MiB from
  the frame allocator and grows by at least 2 MiB on OOM. Design §4/§11 plan
  to replace it with an in-tree slab later.
- **no_std:** yes; with default features off it has no dependencies and does
  not need `alloc`.
- **Unsafe surface:** about 39 `unsafe` sites, mostly the intrusive free-list
  linked lists that store `next` pointers inside free blocks.
  `Heap::add_to_heap` and `Heap::dealloc` are `unsafe fn` (caller guarantees
  the range is exclusively owned / the pointer came from this heap); the
  kernel calls `add_to_heap` only on freshly allocated frames and `dealloc`
  only from `GlobalAlloc::dealloc`.

## fdt

- **Version:** 0.1.5 (`fdt = "0.1.5"`, aarch64 targets only:
  `[target.'cfg(target_arch = "aarch64")'.dependencies]`)
- **Used by:** `patina-kernel` (`kernel/src/arch/aarch64/dtb.rs`)
- **Why:** parse the flattened device tree the VMM passes in `x0`: memory
  nodes, `/memreserve/` and `/reserved-memory`, `/chosen` (`bootargs`,
  `stdout-path`, `linux,initrd-start/end`), UART nodes, `/psci` `method`.
- **no_std:** yes; no dependencies, no `alloc`.
- **Unsafe surface:** only `Fdt::from_ptr` (reads the header to learn the
  blob size, then builds a slice); the kernel calls it once on the DTB,
  which the boot page tables map (two 2 MiB blocks from the DTB's block) and
  the memory map reserves. Everything else is safe slice parsing.
- **Caveats:** it can `expect()`-panic on malformed `#address-cells` or node
  unit names (a panic at boot is acceptable for a broken DTB, but it happens
  before the console exists, so it would be silent). `Chosen::bootargs`
  slices `len - 1` and would panic on an empty property, so `dtb.rs` reads
  the raw `bootargs` and `stdout-path` properties itself and strips the NUL.

## Considered and not used (Phase 0)

Design §11 lists these; Phase 0 does the job in-tree instead. Any of them may
be revisited in later phases.

- **`spin` / `lock_api`:** `SpinLock` (IRQ-safe: masks local interrupts while
  held) and `OnceCell` are in-tree in `kernel/src/sync.rs`, about 170 lines;
  `spin`'s locks do not mask interrupts.
- **`aarch64-cpu` / `tock-registers`:** the handful of system registers
  touched are read and written with `asm!` `msr`/`mrs`, which is smaller than
  the register-description crates.
- **`smccc`:** PSCI over HVC/SMC (`SYSTEM_OFF`, `SYSTEM_RESET`, `VERSION`,
  conduit picked from the DTB) is about 40 lines in
  `kernel/src/arch/aarch64/power.rs`.
- **`uart_16550` / `arm_pl011`:** the PL011 and 16550-MMIO (with `reg-shift`
  / `reg-io-width`) transmit paths are small in-tree drivers in
  `kernel/src/arch/aarch64/console.rs` (transmit only, polled).
- **`x86_64` / `raw-cpuid` / `x2apic` / `uart_16550` (x86 port):** Phase 0
  needs only port I/O, a GDT/TSS/IDT, CR/MSR writes and a 4-level page-table
  walker, all in-tree under `kernel/src/arch/x86_64/` (no crates added by the
  x86_64 port). `raw-cpuid`/`x2apic` become relevant with SMP and the APIC
  timer in Phase 2.

## Phase 1 note

Phase 1 added no third-party dependency. The kernel now depends on the
in-tree crates `patina-abi`, `patina-cmdline`, `patina-cpio` and
`patina-loader` (ELF parser + initial-stack builder, `#![deny(unsafe_code)]`,
host-tested against real Go binaries); `patina-loader` depends only on
`patina-abi`. `fdt` and `buddy_system_allocator` are unchanged.

## Phase 2 note

Phase 2 (part 1: signals, fd layer, SMP bring-up) added no third-party
dependency, so no new audit section. The two new crates are in-tree and
have empty or path-only `[dependencies]` (checked in their `Cargo.toml`):

- `crates/fsutil` (`patina-fsutil`): `#![no_std]` + `alloc`, no
  dependencies at all, no `unsafe`. It holds fd slot allocation, the pipe
  byte ring and the eventfd counter (D46). The kernel depends on it.
- `crates/vfs` (`patina-vfs`): `#![no_std]`, `#![forbid(unsafe_code)]`,
  depends only on the in-tree `patina-abi` and `patina-cpio` (whose
  third-party dependencies, `bitflags` and `zerocopy`, are audited above).
  It is not linked into the kernel yet (Phase 3).

## Phase 3 note: virtio, RNG, control channel (no third-party crate)

The device/control-channel half of Phase 3 added no third-party dependency.
New kernel dependencies are in-tree only: `patina-ctlproto` (already
host-tested, `#![forbid(unsafe_code)]`, no dependencies) and the new
`crates/rand` (`patina-rand`: `#![no_std]`, `#![forbid(unsafe_code)]`, no
dependencies; ChaCha20 per RFC 8439 and BLAKE2s per RFC 7693, checked
against the RFC test vectors). `patina-loader` gained `buildinfo.rs`
(still `#![deny(unsafe_code)]`).

### virtio-drivers (considered, not used)

- **Version audited:** 0.13.0 (2026-03-03), the newest on crates.io,
  downloaded and read (about 13 kLoC, ~145 `unsafe` sites).
- **What it would have given:** `MmioTransport` (legacy + modern) and the
  `Transport` trait, `VirtQueue`, and device drivers.
- **Why not:** (1) dependency weight: `safe-mmio`, `zerocopy` +
  `zerocopy-derive`, `thiserror` (+ derive), `enumn`, `log`,
  `embedded-io` (default feature), i.e. three proc-macro crates and their
  `syn`/`quote` build chains for code the kernel needs about 600 lines of;
  (2) its `VirtIOConsole` is single-port, and `patina.ctl` needs the
  multiport control queue (DEVICE_READY / DEVICE_ADD / PORT_READY /
  PORT_NAME / PORT_OPEN), so the console and vsock logic would be in-tree
  anyway; (3) the transmit paths must work with IRQs masked, from the panic
  / exit path with force-broken locks and bounded polling, which is easier
  to guarantee in code we own. The crate's `Hal` + `Transport` split was
  kept as the model: `drivers::virtio::Transport` is a trait (mmio now,
  virtio-pci in Phase 4/5) and drivers only see it and `VirtQueue`.
- **In-tree unsafe surface instead** (`kernel/src/drivers/virtio/`):
  volatile MMIO register access in `mmio.rs` (mapped uncached with
  `paging::map_device`), volatile ring accesses in `queue.rs` (descriptor
  table / avail / used ring in frames owned by the queue, reached through
  the direct map; release fence before publishing `avail.idx`, acquire
  fence after reading `used.idx`, `arch::cpu::io_barrier` before notify),
  `DmaBuf` copies, and `SpinLock::force_unlock` on the no-return exit/panic
  path. DMA addresses are physical addresses (no IOMMU on any target VMM).
