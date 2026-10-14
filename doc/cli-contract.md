# Kernel ↔ CLI contract (as implemented in cli/)

The CLI (`cli/`) is the reference. Reference encoder/decoder for the cmdline: `cli/internal/cmdline/cmdline.go`.

## Kernel cmdline

```
console=<dev> [ip=...] [patina.strace=1] patina.epoch=<unix-seconds> [patina.env=K=V,K=V] -- arg1 arg2
```

- Tokens are separated by single spaces. `--` is always present; everything after the first `--` is argv[1:], and the kernel sets argv[0] = `/app`.
- Keys, values and args are percent-encoded: every byte ≤0x20 or ≥0x7f, plus `% , = " ' \`. Decoding is `%XX` → that byte, all else literal.
- `patina.env`: split on a raw `,`, then each item on its first raw `=`, then percent-decode.
- Exception: QEMU microvm with `acpi=off` appends ` virtio_mmio.device=512@0xfeb00e00:12` *after* `--`. The CLI always escapes `=` in args, so a raw `virtio_mmio.device=` token after `--` is a kernel parameter, not an arg.
- `console=`: `ttyAMA0` (PL011, QEMU aarch64), `ttyS0` (16550 at 0x3f8 on x86; Firecracker/CH), `hvc0` (virtio-console, Apple VZ).
- Networking: `--net user` → `ip=10.0.2.15::10.0.2.2:255.255.255.0:patina:eth0:off:10.0.2.3` plus a virtio-net-device. A tap device → `ip=dhcp`.

## QEMU devices

aarch64:
```
-M virt,gic-version=3 -accel hvf -cpu host -smp N -m M -nodefaults -no-user-config -display none -no-reboot
-kernel K -initrd I -append CMDLINE -chardev stdio,id=con0,signal=off -serial chardev:con0
-device virtio-serial-device,id=vser0 -chardev socket,id=ctl0,path=...,server=on,wait=off
-device virtserialport,bus=vser0.0,chardev=ctl0,name=patina.ctl -device virtio-rng-device
```

x86_64: the same, but with `-M microvm` (ACPI on), `-accel tcg -cpu max`, and `-device isa-debug-exit,iobase=0xf4,iosize=0x04`. With ACPI on, the kernel finds virtio-mmio by probing from 0xfeb00000 in 512-byte steps (the first device was seen at 0xfeb00e00, IRQ 12) or through the DSDT.

- Control channel on QEMU: a virtio-mmio virtio-serial device (multiport) with a port named exactly `patina.ctl`, carrying NDJSON per design §10.2. vsock port 4242 on the other VMMs.
- Exit code: the `exit` event first, then the `[patina] exit code N` serial line, then isa-debug-exit (x86; QEMU status = (v<<1)|1, only ≥3 is trusted). Exit code 0 must be reported through the event or the sentinel.

## initrd

newc cpio. Entry names have no leading slash (`app`, `srv/www/x`), parents come before children, uid/gid are 0, and the archive ends with `TRAILER!!!`. `/app` has mode 0755.

## Run state

`patina run` writes `$XDG_RUNTIME_DIR/patina/<name>/` or `~/.patina/run/<name>/`, containing `state.json` and `ctl.sock`. `ctl.sock` is a proxy served by `run`: it holds the single upstream VMM connection and broadcasts guest events to all `ctl` clients (QEMU's chardev only accepts one client).

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
- Networking: `--net user` (alias `nat`) attaches VZ's NAT. VZ runs the DHCP server (vmnet shared mode; typically 192.168.64.0/24 with the host/gateway/DNS at .1 — not verified here, the kernel has no virtio-net-pci driver yet), so the kernel gets `ip=dhcp` and must do DHCP. No port forwarding (`--publish` is rejected); the host reaches the guest at its leased address. `--net <tap>` is rejected.
- Control channel: the host connects with `VZVirtioSocketDevice connectToPort:4242`. The guest CID is chosen by VZ (the host never needs it). Before the guest listens, the connect fails promptly ("connection reset"-style error); the `run` proxy retries every ~100 ms forever, so a guest that never listens only costs the control channel (exit code then comes from the serial sentinel). The connection's descriptor is wrapped as a `net.Conn`; `state.json` records the upstream as `{"kind":"vz-vsock","port":4242}`, which only the owning `patina run` can dial — `patina ctl <vm>` always goes through `ctl.sock`.
- Stop/exit: guest PSCI `SYSTEM_OFF` → `guestDidStopVirtualMachine` → the VM is over (status 0). A VZ error → `didStopWithError`, reported on stderr (status 1). Ctrl-C / `--timeout` send `stop` when the channel is up; otherwise (or after the grace) the VM is force-stopped with `stopWithCompletionHandler` (exit 137). Exit code: `exit` event → `[patina] exit code N` on hvc0 → 1.
- Limits on this host (macOS 26, Apple Silicon): 1–64 vCPUs, 4 MiB–64 GiB.

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
- Observed with the current kernel (no virtio drivers): `patina run` boots it, the Go program runs (a test program sleeping 4 s keeps the VM up ~4 s longer than with 0 s), and the VM stops by itself; the exit code is unknown to the host (no console, no vsock), so `run` exits 1. Wall times are odd and worth a look once hvc0 works: a program that sleeps 0 s takes 2.4 s with 2 vCPUs on every run but 0.4 s with 1 vCPU, and sleeping 1 s takes 1.4 s with 2 vCPUs but 3.4 s with 1 (QEMU hvf: 0.39 s / 1.36 s). That suggests timer or IPI delivery differs under VZ (GICR at 0x1001_0000, timer PPIs as above).
