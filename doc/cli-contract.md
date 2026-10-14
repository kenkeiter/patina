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
