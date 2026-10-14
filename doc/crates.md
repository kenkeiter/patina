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
- **Used by:** `patina-abi`; `patina-inet` (only `IntoBytes::as_bytes`, for
  the netlink reply encoder, D73; no extra features)
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

## buddy_system_allocator (removed)

- **Was:** 0.13.0, the kernel heap's allocator (`Heap<32>` behind the
  in-tree IRQ-safe `SpinLock`, D3) until Phase 5. Removed when D87 replaced
  it with the in-tree slab + page allocator in `kernel/src/mm/heap.rs`,
  which gives freed frames back to the frame allocator (the crate's heap
  never did). No longer in `Cargo.lock`.

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

## smoltcp (Phase 4)

- **Version:** 0.14.0, pinned exactly (`smoltcp = { version = "=0.14.0",
  default-features = false, features = [...] }` in `kernel/Cargo.toml`;
  released 2026-08-17).
- **Used by:** `patina-kernel` (`kernel/src/net/`: `mux.rs` implements
  `phy::Device`, `stack.rs` owns the `Interface` and `SocketSet`, `inet.rs`
  wraps `socket::tcp` and `socket::udp`). Nothing outside `net/` sees it;
  drivers only implement `net::device::NetDevice`.
- **Why:** the TCP/IP stack (design §9): Ethernet + ARP + IPv4 + IPv6/NDP +
  ICMP echo, TCP (window scaling, retransmission, keep-alive, Nagle), UDP and
  a DHCPv4 client, `no_std` + `alloc`, no global state, driven by explicit
  `poll` calls — which is what lets one stack lock and one kernel thread own
  it. Writing a TCP from scratch is out of scope.
- **Features enabled:** `alloc`, `medium-ethernet`, `proto-ipv4`,
  `proto-ipv6`, `proto-dhcpv4`, `socket-tcp`, `socket-udp`,
  `socket-dhcpv4` (for the `ip=dhcp` follow-up; not instantiated yet),
  `auto-icmp-echo-reply` (answer pings), `iface-max-addr-count-8` (127.0.0.1,
  ::1, the `ip=` address, the link-local address, room for DHCP/SLAAC),
  `iface-max-route-count-4`, `iface-neighbor-cache-count-32`. Not enabled:
  `std`, `log`, `defmt`, `phy-*` (host TAP/raw sockets), `async`, fragmentation,
  `multicast`, `socket-raw`/`-icmp`/`-dns`/`-mdns`, `medium-ip`/`-ieee802154`.
- **no_std:** yes with `default-features = false`; needs `alloc` (the
  `SocketSet` and socket buffers are `Vec`s, `managed::ManagedSlice` owned).
- **Unsafe surface:** `#![deny(unsafe_code)]` at the crate root. The only
  `unsafe` (18 sites, `src/phy/sys/{mod,raw_socket,tuntap_interface,bpf}.rs`)
  is the host TAP/raw-socket/BPF code behind the `phy-*`/`std` features,
  which is not compiled for the kernel. So the kernel links no smoltcp
  `unsafe`.
- **Build script:** `build.rs` runs on the host at build time; it only
  turns the `*-count-N`/`*-size-N` features and `SMOLTCP_*` environment
  variables into constants (`config.rs` in `OUT_DIR`). No network or
  file access beyond that.
- **Caveats:** smoltcp has one socket per TCP connection and no accept
  queue (patina's listener keeps a pool, D68); no loopback "deliver to
  self" path (patina's mux, D67); it never answers a neighbor solicitation
  for `::1` (the mux re-addresses those, see `net/mux.rs`); a wildcard
  listen/bind endpoint matches both IP versions (patina filters by family).

### smoltcp's transitive dependencies

All resolved by `Cargo.lock`; none has default features that pull `std`.

| crate | version | why (for smoltcp) | no_std | unsafe |
|---|---|---|---|---|
| managed | 0.8.0 | `ManagedSlice`/`ManagedMap`: borrowed or owned storage for sockets and buffers (`map` + `alloc` features) | yes | none |
| heapless | 0.9.3 | fixed-capacity `Vec`/`LinearMap` (interface addresses, routes, neighbor cache) | yes | ~280 sites in 22 files: the container internals (`MaybeUninit` storage, raw-pointer drains, the `pool`/`mpmc`/`spsc` lock-free structures, which smoltcp does not use). Widely used embedded crate (rust-embedded org). |
| hash32 | 0.3.1 | `heapless` dependency (32-bit hashers for its maps) | yes | 9 sites (`Hasher` byte casts) |
| stable_deref_trait | 1.2.1 | `heapless` dependency (marker trait) | yes | 28 sites: `unsafe impl StableDeref` markers, no code |
| byteorder | 1.5.0 | wire-format integer reads/writes | yes (`default-features = false`) | 40 sites in 2 files: unaligned pointer reads in its slice helpers |
| bitflags | 1.3.2 | TCP/IP flag sets (smoltcp still uses bitflags 1; the kernel's own `bitflags` is 2.x, both are linked) | yes | 19 sites, all `from_bits_unchecked` (an `unsafe fn` smoltcp does not call) and its tests |
| cfg-if | 1.0.5 | `cfg_if!` macro | yes | none |

## patina-inet (in-tree, Phase 4)

`crates/inet`: socket-address codec (`SockAddr` ↔ `sockaddr_in`/`_in6`/`_un`
bytes, IPv4-mapped IPv6 handling) and the inet bind table (`PortTable`:
EADDRINUSE with `SO_REUSEADDR`/`SO_REUSEPORT` and dual-stack overlap,
ephemeral ports 32768–60999), plus the `NETLINK_ROUTE` request parser and
link/address dump encoder (`netlink`, D73). Depends on `patina-abi` and
`zerocopy` (already audited above); no `unsafe`; host tests in the crate
(`cargo test -p patina-inet`).

## patina-vdso (in-tree, Phase 5)

`crates/vdso` (D75): the vDSO image and everything about it. `build.rs`
compiles `image/vdso.rs` (+ `image/chacha.rs`, `src/vvar.rs`) for both
bare targets with the toolchain's own `rustc --emit=obj` and links it with
the toolchain's `rust-lld` (`-shared`, `image/vdso.lds`, a per-arch version
script) into `OUT_DIR/vdso-<arch>.so`; no system compiler or linker, no
new dependency. The library embeds both images (`IMAGE` for the target
arch), defines the vvar page layout and seqlock (`vvar`, shared verbatim
with the image) and has a small no-alloc ELF64 reader (`elf`: layout,
SysV-hash symbol lookup with versions as Go does it, relocation /
writable-section checks). `unsafe` only in the image itself (inline asm
for the counter, syscalls and the PC-relative vvar address; raw pointers
into the caller's buffers and state). Host tests: `cargo test -p
patina-vdso`; `patina-rand` is a dev-dependency (ChaCha20 cross-check).
