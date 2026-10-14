# Kernel ↔ CLI contract (as implemented in cli/)

The CLI (`cli/`) is the reference. Reference encoder/decoder for the cmdline: `cli/internal/cmdline/cmdline.go`.

## Kernel cmdline

```
console=<dev> [ip=...] [patina.strace=1] patina.epoch=<unix-seconds> [patina.env=K=V,K=V] -- arg1 arg2
```

- Tokens are separated by single spaces. `--` is always present; everything after the first `--` is argv[1:], and the kernel sets argv[0] = `/app`.
- Keys, values and args are percent-encoded: every byte ≤0x20 or ≥0x7f, plus `% , = " ' \`. Decoding is `%XX` → that byte, all else literal.
- `patina.env`: split on a raw `,`, then each item on its first raw `=`, then percent-decode.
- `patina.cwd=<dir>` (optional, percent-decoded): the program's initial working directory, like a container's WORKDIR. It must be an existing directory in the initrd; otherwise the kernel logs it and starts in `/`. The CLI does not send it yet (used by `scripts/test-os.sh`).
- Exception: QEMU microvm with `acpi=off` appends ` virtio_mmio.device=512@0xfeb00e00:12 virtio_mmio.device=512@0xfeb00c00:11` (one per device, from the top of the window down; seen with the CLI's console + rng devices) *after* `--`. The CLI always escapes `=` in args, so a raw `virtio_mmio.device=` token after `--` is a kernel parameter, not an arg.
- `console=`: `ttyAMA0` (PL011, QEMU aarch64), `ttyS0` (16550 at 0x3f8 on x86; Firecracker/CH), `hvc0` (virtio-console, Apple VZ).
- Networking: `--net user` → `ip=10.0.2.15::10.0.2.2:255.255.255.0:patina:eth0:off:10.0.2.3` plus a virtio-net-device. A tap device → `ip=dhcp`.
- Multi-queue (D88): the kernel negotiates `VIRTIO_NET_F_MQ` wherever it is offered and uses min(vCPUs, `max_virtqueue_pairs`) queue pairs (`patina.net.queues=N` caps them; 1 turns multi-queue off). The CLI does not add `mq=on`: `-netdev user` has one queue, so QEMU would offer one pair and only add a control queue, and a tap with `queues=N` must have been created `multi_queue`. A hand-written QEMU command line enables it with `-netdev tap,…,queues=N -device virtio-net-pci,mq=on,vectors=2N+2,…` (or `virtio-net-device,mq=on`) on a Linux host. Apple VZ offers no MQ.
- `ip=dhcp` (also `on`/`any`/`bootp`/`both`, or an `ip=` without a client address): the kernel runs DHCPv4 before the program starts and waits at most 10 s. On a lease it logs `[patina] net: dhcp lease <addr>/<prefix> via <gw>, dns <servers>[, host H][, domain D] (<n> ms)` and sends the same text (minus `[patina] `) as a control-channel event `{"ev":"log","level":"info","msg":"net: dhcp lease …"}` (a later change: `net: dhcp lease changed: …`); `patina run` prints `patina: guest address <addr> (gateway <gw>|none)` once per new address and records `"net":{"addr","prefix","gateway","dns"}` in `state.json` (D85). On timeout it logs `[patina] net: dhcp: no lease after 10 s, continuing without an address` and sends a control-channel event `{"ev":"log","level":"warn","msg":"net: dhcp: no lease after 10 s, continuing without an address"}`, possibly before `ready`. Without a NIC the message is `ip=dhcp: no network device, continuing without an address`. The program then runs without an IPv4 address. `/etc/resolv.conf` and `/etc/hosts` are generated from the lease, or from the static `ip=` fields, unless the initrd ships them (D72).

## QEMU devices

aarch64:
```
-M virt,gic-version=3 -accel hvf -cpu host -smp N -m M -nodefaults -no-user-config -display none -no-reboot
-kernel K -initrd I -append CMDLINE -chardev stdio,id=con0,signal=off -serial chardev:con0
-device virtio-serial-device,id=vser0 -chardev socket,id=ctl0,path=<run>/qemu-ctl.sock,server=on,wait=on
-device virtserialport,bus=vser0.0,chardev=ctl0,name=patina.ctl -device virtio-rng-device
```

x86_64: the same, but with `-M microvm` (ACPI on), `-accel tcg -cpu max`, and `-device isa-debug-exit,iobase=0xf4,iosize=0x04`. With ACPI on (the CLI's setup), QEMU 11 describes the virtio-mmio devices in the DSDT and the kernel takes them from there: console at 0xfeb02e00 GSI 47, rng at 0xfeb02c00 GSI 46 (kernel lines `[patina] virtio: mmio @ 0xfeb02e00 irq 47 device-id 3 (console) [acpi dsdt, ...]`, level-triggered via the second IOAPIC). The 0xfeb00e00/IRQ 12 layout applies only to `-M microvm,acpi=off`, where QEMU passes the devices on the cmdline (see above). On aarch64 `virt` the devices come from the DTB (CLI layout: rng at 0xa003c00 INTID 78, console at 0xa003e00 INTID 79).

The CLI layout is unchanged by virtio-pci (D70). The kernel also drives the `-pci` variants of the same devices (`-device virtio-serial-pci,id=vser0 ... -device virtio-rng-pci`), and these are required on x86 `-M q35` or `-M pc`, which have no virtio-mmio bus. q35 needs nothing else: SeaBIOS assigns the BARs, the MCFG gives ECAM, and interrupts are MSI-X. On aarch64 `virt` the kernel assigns BARs itself (there is no firmware with `-kernel`). Interrupts there are MSI-X through the ITS (TCG) or the GICv2m frame (hvf). Discovery lines look like `[patina] virtio: pci 0000:00:01.0 irq N device-id 3 (console) [ecam dtb|ecam acpi mcfg|port 0xcf8 config, modern virtio-pci, vendor 0x1af4, msi-x|intx irq]`.

- Control channel on QEMU: a virtio-mmio virtio-serial device (multiport) with a port named exactly `patina.ctl`, carrying NDJSON per design §10.2. vsock port 4242 on the other VMMs.
- The chardev is `wait=on`: QEMU creates the socket and does not start the guest until `patina run`'s control proxy has connected, so the proxy sees every event, including `ready` and the `exit` of a program that ends in milliseconds. The QEMU "info: QEMU waiting for connection on: ..." line this causes is filtered from stderr (other QEMU stderr output passes through).
- Exit code of `patina run` (all VMMs), first match wins:
  1. `--timeout` expired: 124 (like timeout(1)), even if the guest then exited 0 or was forced.
  2. The `exit` event's `code` (0, the program's code, 128+N for a `SIG_DFL` death, 137 `forced` after the grace).
  3. A `panic` event: 255 (the kernel also prints `[patina] exit code 255`, but the event works without serial).
  4. The `[patina] exit code N` serial line.
  5. The VMM's own carrier: QEMU isa-debug-exit on x86 (status = (v<<1)|1, only ≥3 is trusted). Exit code 0 must be reported through the event or the sentinel.
  6. The CLI killed the VMM (second interrupt, or no exit within grace + 2 s): 137.
  7. Otherwise 1, with a message. CLI errors are 125, usage errors 2.

## `patina run` signals and messages

- Ctrl-C (SIGINT), SIGTERM and SIGHUP: `{"op":"stop","grace_ms":<--grace, default 10 s>}`; the result is the guest's exit code (0 for a program that handles SIGTERM, 143 without a handler). A second signal kills the VMM (137). If the control channel is not connected, the first signal kills.
- `--timeout D`: the same stop when D expires, then exit 124.
- `-v`: the VMM argv, `patina: control channel attached (...)`, and one `patina: control: <event>` line per event, e.g. `patina: control: ready: go1.27.1, 2 cpus, 512 MiB` and `patina: control: exit code 3`. A `panic` event is always printed (`patina: KERNEL PANIC: <msg>`).
- CLI messages on stderr are written between console lines: if the guest console is mid-line, the message starts on a new line.
- The run dir is removed when `patina run` returns; `patina ps` lists run dirs whose `patina run` process is alive (columns `NAME VMM ARCH PID ADDR STARTED IMAGE`; `ADDR` is the guest's leased address or `-`).
- `PATINA_PARAMS` (environment, space-separated) appends extra kernel parameters to the cmdline; for tests and experiments (`scripts/run-vz.sh` uses it), not part of the normal CLI surface. A `patina run` killed with SIGKILL leaves QEMU running (own process group) and a stale run dir, which the next run with that name replaces.

## initrd

newc cpio. Entry names have no leading slash (`app`, `srv/www/x`), parents come before children, uid/gid are 0, and the archive ends with `TRAILER!!!`. `/app` has mode 0755.

## Run state

`patina run` writes `$XDG_RUNTIME_DIR/patina/<name>/` or `~/.patina/run/<name>/`, containing `state.json` and `ctl.sock`. `ctl.sock` is a proxy served by `run`: it holds the single upstream VMM connection and broadcasts guest events to all `ctl` clients (QEMU's chardev only accepts one client). A client that connects after boot first receives the last `ready` line. `patina ctl <vm> stop` exits with the guest's exit code and prints every event line it receives (raw NDJSON), ending with the `exit` event; `ping` prints the `pong` (`{"ev":"pong","id":N,"uptime_ms":M}`); `signal`/`dump` print events for `--linger` (500 ms) and exit 0.

## End-to-end test (real kernel, QEMU)

`cd cli && PATINA_E2E=1 go test ./internal/run -run E2E -v` (`PATINA_E2E_ARCH=aarch64|x86_64` to limit; skipped without the env var, QEMU or `out/kernel-*`). It builds the CLI, `patina build`s testapps/hello and testapps/signals, and checks per arch: `hello -- 3 a b` → 3 and the argv line; `ctl stop` (handler → 0, nohandler → 143 with `"signal":"SIGTERM"`, ignore with `--grace 1s` → 137 `forced:true` in < 3.5 s); `ctl ping`; `ctl dump` → goroutine dump and 2; `ctl signal SIGUSR1`; `--timeout 3s` → 124; SIGINT to `patina run` → 143 (nohandler); two SIGINTs → 137; run dir removed. 60 s bound per wait.

## Firecracker and Cloud Hypervisor

Drivers: `cli/internal/vmm/firecracker.go` (written against the Firecracker v1.17.0 swagger) and `cli/internal/vmm/cloudhypervisor.go` (Cloud Hypervisor v54.0). Neither VMM runs on the macOS dev host, so both are tested only against fakes (`cli/internal/vmm/fakevmm`, the `Firecracker*`/`CloudHypervisor*` tests in `cli/internal/run`, and `TestRunVsockVMMsCLI`). The binary comes from PATH or `--vmm-bin`.

Both drivers:

- Use the console from `--serial`/the 16550 on the VMM's stdout; the VMM's stdin is an idle pipe.
- Configure virtio-vsock with guest CID 3 and host socket `<run>/vsock.sock`. The control channel is `CONNECT 4242\n` → `OK <hostport>\n` on that socket, then NDJSON. `patina run` retries every 100 ms until the guest listens (until then the VMM closes the connection). `patina ctl <vm>` goes through the `ctl.sock` proxy as usual, and `patina ctl --vsock <run>/vsock.sock` works directly.
- Stop the guest with `{"op":"stop"}` over the control channel. A forced stop sends SIGKILL to the VMM. Neither driver uses `SendCtrlAltDel` or ACPI power-button requests.
- Take the exit code from the `exit` event, then the `[patina] exit code N` serial line. The VMM's process status carries no guest code: both exit 0 on guest power-off.
- Networking: only `--net <tap>` is supported. `--net user` and `--publish` are QEMU-only. The guest gets a stable, locally administered MAC derived from the run dir. The kernel parameter is `ip=dhcp`, or with `--ip ADDR/PREFIX[,GW[,DNS]]` it is `ip=ADDR::GW:MASK:patina:eth0:off[:DNS]`. A raw nfsroot-style `ip=` value is passed through. `--ip` also applies to QEMU tap devices.

### Firecracker

```
firecracker --api-sock <run>/fc.sock --log-path <run>/fc.log --level Warning
PUT /boot-source              {"kernel_image_path":K,"initrd_path":I,"boot_args":CMDLINE}
PUT /machine-config           {"vcpu_count":N,"mem_size_mib":M}
PUT /vsock                    {"guest_cid":3,"uds_path":"<run>/vsock.sock"}
PUT /network-interfaces/eth0  {"iface_id":"eth0","host_dev_name":TAP,"guest_mac":MAC}   (--net TAP only)
PUT /entropy                  {}
PUT /actions                  {"action_type":"InstanceStart"}
```

- The CLI writes HTTP/1.1 over the Unix socket with JSON headers. A non-2xx reply aborts the run with Firecracker's `fault_message`. Firecracker's logger defaults to stdout, so the CLI points it at `fc.log` to keep it out of the console. The log is replayed to stderr if Firecracker fails.
- vCPUs must be 1 or an even number ≤ 32. The CLI checks this before starting.
- Kernel: the PVH ELF on x86_64 (Firecracker uses the `XEN_ELFNOTE_PHYS32_ENTRY` note) and the arm64 `Image` on aarch64.
- Console: `console=ttyS0` on both arches. On x86_64 this is a 16550 at I/O port 0x3f8 (IRQ 4). On aarch64 it is an MMIO 16550 that the DTB describes as `ns16550a`. Firecracker registers that serial device only when the cmdline contains `console=`.
- Command-line additions. Firecracker appends these after our `--` (linux-loader appends to the end), each as a raw-`=` token that the kernel already treats as a kernel parameter:
  - Both arches: ` pci=off` (PCI is off unless `--enable-pci`).
  - x86_64: one ` virtio_mmio.device=4K@0x<addr>:<gsi>` per virtio device (vsock, entropy, net). GSIs start at 5, and the MMIO window is the 32-bit hole below 4 GiB.
  - aarch64: ` earlycon=uart,mmio,0x<serial addr>`. The virtio-mmio devices are in the DTB, not on the cmdline.
  - Firecracker appends nothing else. Its default `reboot=k panic=1 pci=off nomodule 8250.nr_uarts=0 i8042.* ...` cmdline is used only when `boot_args` is absent, and the CLI always sends `boot_args`. There is no `keep_bootcon`.
- Command-line length: Firecracker's buffer is 2048 bytes including what it appends, so the CLI refuses `boot_args` longer than 1792 bytes.
- x86_64 ACPI: Firecracker ≥ 1.8 also describes each virtio-mmio device in the DSDT (`_HID` `LNRO0005`, Memory32Fixed + interrupt). It places the RSDP at 0xe0000 but does not set `hvm_start_info.rsdp_paddr` on PVH boot, so the kernel must scan 0xe0000–0xfffff for `RSD PTR `. Alternatively, the cmdline entries are enough to find the devices.
- Power-off: on x86_64, an i8042 reset (write 0xfe to port 0x64) makes Firecracker exit with status 0. On aarch64, PSCI SYSTEM_OFF does the same.

### Cloud Hypervisor

```
cloud-hypervisor --api-socket path=<run>/ch.sock --kernel K --initramfs I --cmdline CMDLINE
  --cpus boot=N --memory size=<M>M --serial tty --console off
  --vsock cid=3,socket=<run>/vsock.sock --rng src=/dev/urandom [--net tap=TAP,mac=MAC]
GET /api/v1/vmm.ping        (informational: logs the version with -v)
```

- Why command-line flags instead of `PUT /api/v1/vm.create` + `vm.boot`: a single exec configures and boots the VM, so there is no API-readiness race and no window where the VMM is up but unconfigured. A bad configuration fails the process with its reason on stderr. The flag syntax has been stable across releases, while the `vm.create` body has changed shape (kernel, cmdline and initramfs moved under `payload` in v28). The API socket stays open for `ch-remote`.
- Console: `console=ttyS0` on x86_64 (16550 at 0x3f8). `console=ttyAMA0` on aarch64: Cloud Hypervisor's serial device there is a **PL011**, not a 16550, and it is described in the DTB. `--console off` disables the default virtio-console (hvc0), so it does not also write to stdout.
- Command-line additions: none on x86_64. On aarch64, Cloud Hypervisor appends ` earlycon=pl011,mmio,0x<addr>` after `--`. The buffer is 64 KiB on x86_64.
- **Virtio transport is PCI only**: Cloud Hypervisor has no virtio-mmio. The kernel needs virtio-pci (modern, MSI-X capable devices):
  - x86_64: ECAM from the ACPI MCFG (Cloud Hypervisor sets `hvm_start_info.rsdp_paddr`).
  - aarch64: the DTB `pci-host-ecam-generic` node.
  - From Cloud Hypervisor's source (D70; untested here): the VMM assigns the BARs. BAR0 is 512 KiB and holds the capabilities and the MSI-X table; it is 64-bit and placed above RAM except for cold-plugged virtio-block. ECAM covers bus 0 only: x86_64 0xe8000000, aarch64 0x30000000. There is **no INTx**: the interrupt pin is 0 and the aarch64 `interrupt-map` is empty. Interrupts are MSI-X only: x86 through the LAPIC, aarch64 through the GICv3 ITS (KVM) or a GICv2m frame (MSHV), which the kernel drives.
- Platform: on x86_64 PCI comes from ACPI (MCFG, DSDT) and CPUs from the MADT. Cloud Hypervisor also writes an MP table, but PCI discovery needs ACPI.
- Power-off on x86_64: an ACPI S5 write of 0x34 (`SLP_TYP=5<<2 | SLP_EN`) to I/O port 0x600, the hardware-reduced FADT sleep control register. **Do not use an i8042 reset (0xfe to port 0x64) or the ACPI reset value (1 to port 0x600): on Cloud Hypervisor both *reboot* the VM, which would run the program again.**
- Power-off on aarch64: PSCI SYSTEM_OFF. Cloud Hypervisor exits with status 0 on guest shutdown, because the CLI does not pass `--no-shutdown`.
## Apple Virtualization.framework (`--vmm vz`, target `vz-aarch64`)

Driver: `cli/internal/vmm/vz.go` (platform-neutral) and `vz_darwin_arm64.go` (cgo + Objective-C, built only for `darwin && arm64 && cgo`; elsewhere `--vmm vz` fails with "unsupported on this platform"). The VM runs in the `patina run` process on a private serial dispatch queue; there is no VMM PID.

Build: `cli/scripts/build.sh` → `cli/bin/patina`, ad-hoc signed with `cli/scripts/patina.entitlements` (`com.apple.security.virtualization`). An unsigned binary fails at configuration validation with an error naming the entitlement. `cli/scripts/vztest.sh` builds and signs the `internal/vmm` test binary so the in-process VZ tests run instead of skipping.

Machine:
```
VZLinuxBootLoader(kernel Image, initialRamdiskURL = initrd, commandLine)
VZGenericPlatformConfiguration, CPUCount = --cpus, memorySize = --mem MiB
serialPorts:    [VZVirtioConsoleDeviceSerialPortConfiguration]  -> console=hvc0; guest output -> stdout (no guest input)
socketDevices:  [VZVirtioSocketDeviceConfiguration]             -> control channel
entropyDevices: [VZVirtioEntropyDeviceConfiguration]
networkDevices: [VZVirtioNetworkDeviceConfiguration + VZNATNetworkDeviceAttachment]   only with --net user
```

- Cmdline: `console=hvc0 [ip=dhcp] patina.epoch=<unix> ... -- args`. `patina run` also sends `{"op":"time","epoch_ns":N}` right after every `ready` event (drivers opt in through `vmm.TimeSyncer`), per design §3.3. (The probe below shows VZ does expose a PL031 RTC holding host wall time, so the kernel can read the clock itself; `patina.epoch` and the time op remain the contract.)
- Networking (verified, D85): `--net user` (alias `nat`) attaches virtio-net (00:01.0, 1af4:1041) on `VZNATNetworkDeviceAttachment` and the kernel gets `ip=dhcp`. VZ's bootpd leases 192.168.64.0/24 with the gateway and DNS at .1 (DISCOVER at ~80 ms, OFFER ~200–500 ms later). VZ's virtio-net refuses `FEATURES_OK` unless the driver accepts `VIRTIO_NET_F_MTU` (device MTU 1500; D83); interrupts are INTx (SPI 32 + slot). No port forwarding (`--publish` is rejected); the host reaches the guest directly at its leased address, which the kernel reports on hvc0 and as `{"ev":"log","level":"info","msg":"net: dhcp lease A/P via G, dns D… (N ms)"}` (possibly before `ready`). `patina run` prints `patina: guest address A (gateway G)` on stderr (from the event, else parsed from the console line), records `"net":{"addr","prefix","gateway","dns"}` in `state.json` and `patina ps` shows it as `ADDR`. After the lease the guest ARPs the gateway once, so the host's first packet is not lost to its own ARP resolution. Right after macOS creates the NAT bridge (no other NAT VM running), the first connection can be delayed by seconds host-side. `--net <tap>` is rejected.
- Control channel: the host connects with `VZVirtioSocketDevice connectToPort:4242`. The guest CID is chosen by VZ (the host never needs it). Before the guest listens, the connect fails promptly ("connection reset"-style error, ~1 ms); the `run` proxy retries every 5 ms until the first connection and every 100 ms for reconnects, so a program that exits within milliseconds of boot still delivers `ready` and `exit` (hello: 40/40); a guest that never listens only costs the control channel (exit code then comes from the serial sentinel). The connection's descriptor is wrapped as a `net.Conn`; `state.json` records the upstream as `{"kind":"vz-vsock","port":4242}`, which only the owning `patina run` can dial — `patina ctl <vm>` always goes through `ctl.sock`. A `ctl` client that attaches after boot gets only the replayed `ready`: events emitted earlier (e.g. an early `log`) are not seen by it (there is no `wait=on` equivalent on VZ).
- AF_VSOCK for the program (D76): the guest's virtio-vsock device also carries the program's `AF_VSOCK` stream sockets. Port **4242 stays reserved** for the kernel's control channel (a program's `bind(4242)` is `EADDRINUSE`, autobind never picks it, and the driver routes every packet for 4242 to the control channel before the socket layer sees it), so `patina ctl`, the `run` proxy and `ctl.sock` are unaffected by what the program does with vsock. Any other guest port the program listens on is reachable from the host with `connectToPort:<port>`; in Go that is `vmm.VsockDialer.DialVsock(port, timeout)` on the VZ machine (the same dial path as the control channel; the conn supports `CloseWrite`). `patina` has no command that forwards such ports yet. Guest→host connections reach VZ's `VZVirtioSocketListener`s; the CLI registers none, so they are refused (`ECONNRESET` in the guest, Linux's error for a `RST` answering a `REQUEST`). The guest's CID is the one VZ assigns (3 so far), which the program reads with `IOCTL_VM_SOCKETS_GET_LOCAL_CID` on `/dev/vsock`. VZ stalls all vsock traffic while one connection's host reader stops reading (D76). Verified by `TestVZVsock` (`cli/scripts/vztest.sh -test.run TestVZVsock`, also run by `vztest.sh --e2e` and `scripts/test-vz.sh`).
- Stop/exit: guest PSCI `SYSTEM_OFF` → `guestDidStopVirtualMachine` → the VM is over (status 0). A VZ error → `didStopWithError`, reported on stderr (status 1). Ctrl-C / `--timeout` send `stop` when the channel is up; otherwise (or after the grace) the VM is force-stopped with `stopWithCompletionHandler` (exit 137). Exit code: `exit` event → `[patina] exit code N` on hvc0 → 1. After `guestDidStop` the console is drained until it has been quiet for 20 ms (at most 250 ms) and `run` waits for the control connection's buffered events only while one is attached (at most 300 ms); hello runs in ~130–185 ms wall.
- Limits on this host (macOS 26, Apple Silicon): 1–64 vCPUs, 4 MiB–64 GiB.
- Tests: `scripts/test-vz.sh` (the VZ milestone: hello exit code, console, ctl ping/signal/time/dump, stop 0/143/137, httphello over VZ NAT, threads/signals/sockets), `cli/scripts/vztest.sh --e2e` (`PATINA_VZ_E2E=1 go test -run TestVZE2E .`, with `PATINA_NET_EXTERNAL=1` for the DNS case), and every QEMU suite script with `PATINA_VMM=vz` (D84: `scripts/run-qemu.sh` execs `scripts/run-vz.sh`, which packs the kernel + initrd with the hidden `patina image pack` and runs `patina run --vmm vz`; `PATINA_PARAMS` carries kernel params).
- Kernel quirk (D83): VZ starts a device model asynchronously after `DRIVER_OK` and the console device discards the first transmit buffer available at that moment; the kernel posts a sacrificial 1-byte buffer on VZ console tx queues (devices behind Apple's 0x106b host bridge, discovery line `apple host`).

### What VZ gives the guest (observed, macOS 26 / Apple Silicon)

VZ shows the host neither the device tree nor guest memory, and the guest has no console without a virtio driver. `TestVZProbeDTB` (`cli/internal/vmm/vz_darwin_arm64_test.go` + `testdata/vzprobe.S`) works around that with a timing side channel: a 200-byte probe Image reads one nibble (of the DTB at `x0`, or of a physical address), waits `nibble × 10 ms`, and calls PSCI SYSTEM_OFF; the host times start → `guestDidStop` (baseline ≈ 1.2 ms, ≈ 115 ms per boot including VM setup). The whole 3616-byte DTB took ~9 min to read after a coarser first pass; run it with `PATINA_VZ_PROBE=1 cli/scripts/vztest.sh -test.run TestVZProbeDTB -test.v` (env knobs are documented in the test).

Device tree, for 2 vCPUs / 512 MiB (`dtc` output, interrupt-map condensed; two single-nibble misreads in property names fixed by hand — `gpio-controller`, `psci`):

```dts
/ {
	#address-cells = <2>; #size-cells = <2>; interrupt-parent = <&gic>;
	clock { compatible = "fixed-clock"; #clock-cells = <0>; clock-frequency = <24000000>; clock-output-names = "clk24mhz"; };
	hypervisor { compatible = "apple,virtualization-generic-platform"; };
	memory { device_type = "memory"; reg = <0x0 0x70000000 0x0 0x20000000>; };   /* RAM starts at 0x7000_0000 */
	gic: gic { compatible = "arm,gic-v3"; #interrupt-cells = <3>; interrupt-controller; ranges;
		reg = <0x0 0x10000000 0x0 0x10000		/* GICD */
		       0x0 0x10010000 0x0 0x40000>; };	/* GICR, 0x20000 per CPU; no ITS / MSI controller */
	pl061 { compatible = "arm,pl061", "arm,primecell"; reg = <0x0 0x20060000 0x0 0x1000>; interrupts = <0 5 4>;
		gpio-controller; #gpio-cells = <2>; clocks = <&clk>; clock-names = "apb_pclk"; };
	gpio-keys { compatible = "gpio-keys";
		power { label = "GPIO Key Power"; linux,code = <116>; gpios = <&pl061 6 0>; debounce-interval = <0>; }; };
	pl031 { compatible = "arm,pl031", "arm,primecell"; reg = <0x0 0x20050000 0x0 0x1000>; interrupts = <0 4 4>; };
	psci { compatible = "arm,psci-0.2"; method = "hvc"; cpu_on = <0xc4000003>; cpu_off = <0x84000002>; ... };
	cpus { #address-cells = <1>; #size-cells = <0>;
		cpu@0 { device_type = "cpu"; compatible = "arm,arm-v8"; enable-method = "psci"; reg = <0>; };
		cpu@1 { ... reg = <1>; }; };
	timer { compatible = "arm,armv8-timer"; always-on;
		interrupts = <1 13 0x3f08  1 14 0x3f08  1 11 0x3f08  1 10 0x3f08>; };	/* virtual timer = PPI 11 (INTID 27) */
	pci { compatible = "pci-host-ecam-generic"; device_type = "pci"; bus-range = <0 0xff>;
		#address-cells = <3>; #size-cells = <2>; #interrupt-cells = <1>;
		reg = <0x0 0x40000000 0x0 0x10000000>;					/* ECAM, 256 buses */
		ranges = <0x01000000 0x0 0x00000000  0x0 0x6fff0000  0x0 0x00010000	/* I/O */
		          0x02000000 0x0 0x50000000  0x0 0x50000000  0x0 0x1ffe0000	/* MEM32 */
		          0x03000000 0x1 0x00000000  0x1 0x00000000  0x4 0x00000000>;	/* MEM64 (4 GiB at 4 GiB) */
		interrupt-map-mask = <0xf800 0 0 7>;
		interrupt-map = <(dev<<11) 0 0 1  &gic 0 0  0 (0x20+dev) 4  for dev = 0..31>; };	/* INTA only: SPI 32+dev (INTID 64+dev), level-high */
	chosen { bootargs = "console=hvc0 patina.epoch=1 --";
		kaslr-seed = <random u64, differs every boot>;
		linux,initrd-start = <0x0 0x70001208>; linux,initrd-end = <0x0 0x7000120d>; };
};
```

PCI bus 0 (ECAM reads through the same probe; slots were the same with and without the NIC):

| BDF | vendor:device | class | what |
|---|---|---|---|
| 00:00.0 | 106b:1a05 | 060000 host bridge | Apple |
| 00:01.0 | 1af4:1041 | 020000 | virtio-net (only with `--net user`) |
| 00:05.0 | 1af4:1043 | 078000 | virtio-console (`hvc0`) |
| 00:06.0 | 1af4:1053 | 078000 | virtio-vsock |
| 00:07.0 | 1af4:1044 | 100000 | virtio-rng (entropy) |

All are virtio 1.x **modern-only** PCI devices (device ID 0x1040+type, revision 1, subsystem 1af4:00<type>).

Consequences for the kernel (VZ is unlike the other targets):

- **No virtio-mmio at all**, and no UART: there is no PL011/16550 node and no `/chosen/stdout-path`. Every virtio device is virtio-pci (modern, capabilities in config space) behind a generic ECAM host bridge, so VZ support needs: ECAM enumeration of bus 0, BAR assignment from the `ranges` windows (nothing pre-assigns them; there is no firmware), the virtio-pci modern transport (common/notify/ISR/device cfg capabilities), and legacy INTx through `interrupt-map` (SPI 32+slot, level) — there is no GIC ITS, so no MSI-X. Console output before the virtio-console driver is up is impossible.
- Console: one virtio-console device per `VZVirtioConsoleDeviceSerialPortConfiguration`; whether it offers `VIRTIO_CONSOLE_F_MULTIPORT` was not probed (feature negotiation needs BAR writes). Treat port 0 as `hvc0`.
- vsock: virtio-vsock at 00:06.0; the guest CID comes from its device config (`guest_cid`) — not probed. Until the guest listens on 4242, `connectToPort` fails in ~20 ms with "Connection reset by peer" (observed with the probe kernel).
- Wall clock: a PL031 at 0x2005_0000 reads host Unix time (probe read 1792005261 when the host clock said 1792005260), so design §3.3's "none at boot" is not accurate for macOS 26.
- RAM base is 0x7000_0000 (not 0x4000_0000 as on QEMU `virt`). The initrd is placed right after the kernel's `image_size`, **unaligned** (0x7000_1208 above for a tiny Image).
- Power: PSCI 0.2 over HVC; `SYSTEM_OFF` stops the VM cleanly (`guestDidStopVirtualMachine`). VZ's own graceful-stop request (`requestStopWithError`, unused by the CLI) arrives as a KEY_POWER press on PL061 GPIO 6.
- Observed with the driverless kernel at the time of the probe: the VM booted and stopped by itself but the exit code was unknown, and wall times looked odd (a 0 s sleep took 2.4 s with 2 vCPUs, 1 s took 3.4 s with 1). With the drivers (D83–D85) none of that remains: hello runs in ~130 ms wall at 1/2/4 vCPUs, `Sleep(1s)` measures 1000.3–1001.0 ms, CPU_ON takes 75–257 µs and timer deadlines fire within 5 µs, the same as QEMU hvf (D84, `testapps/sleeper`). What remains is host-side and identical on QEMU hvf: an idle vCPU woken from WFI by its own timer wakes ~20–25% late below 2 ms.
