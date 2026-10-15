# bzImage boot entry for x86_64 (design note, not implemented)

Status: **design only, nothing implemented or verified** (2026-10-14). Kept so the
loader research is not lost. The x86_64 kernel today is only a PVH ELF
(`out/kernel-x86_64-pvh.elf`, 32-bit entry with `hvm_start_info`, linked at
phys 0x200000).

## Why

A Linux-boot-protocol **bzImage** with a 64-bit entry would let patina be
loaded by:

- `kexec_file_load` (`kexec -s`) from a running Linux guest. This is the only
  way to run patina on a Fly.io Machine: Fly boots its own kernel
  (6.12.105-fly, `CONFIG_KEXEC_FILE=y`, `CONFIG_KEXEC` unset so only the
  bzImage format is accepted, no `KEXEC_SIG`, no lockdown, root holds all
  capabilities, `rdinit=/fly/init`), there is no custom-kernel or nested-KVM
  option, and the devices are plain virtio-mmio named on the cmdline
  (`pci=off virtio_mmio.device=4K@0xc0001000:6 … :11`: balloon, 3× block,
  net, vsock).
- Firecracker's bzImage loader (it tries ELF first, then bzImage).
- `qemu -kernel` / qboot on `-M microvm` (16-bit entry, see below).

## What the loaders actually do

Sources read: Linux v6.12 `arch/x86/kernel/kexec-bzimage64.c`,
`machine_kexec_64.c`, `Documentation/arch/x86/boot.rst`, `bootparam.h`;
QEMU v10.0.0 `hw/i386/x86-common.c`; qboot `linuxboot.c`; Firecracker v1.17.0
`src/vmm/src/arch/x86_64/{mod,layout,regs}.rs`; rust-vmm linux-loader
`bzimage/mod.rs`.

### kexec_file (`kexec-bzimage64.c`)

- `bzImage64_probe` requires: image ≥ 1024 bytes, `HdrS` at 0x202,
  `boot_flag` 0xAA55, protocol version ≥ 0x020C, `loadflags` LOADED_HIGH,
  `xloadflags` XLF_KERNEL_64 and XLF_CAN_BE_LOADED_ABOVE_4G.
- `bzImage64_load` never reads `relocatable_kernel`. The kernel buffer has
  memsz = PAGE_ALIGN(`init_size`), align = `kernel_alignment`,
  min = max(`pref_address`, 1 MiB), no max, searched **top-down** from the
  highest RAM. The load address is therefore usually the highest suitable
  hole and may be above 4 GiB. Relying on `pref_address` is impossible.
- boot_params and the cmdline share one buffer (cmdline right after
  boot_params; min 0x3000, top-down). The initrd is top-down with min
  16 MiB. The cmdline must be shorter than `cmdline_size` − 30.
- Entry: `rip = load + 0x200`, `rsi = boot_params`, 64-bit mode.
- boot_params filled: e820 from the old kernel's `e820_table_kexec` (≤ 128
  entries), `acpi_rsdp_addr`, `cmd_line_ptr` + `ext_cmd_line_ptr`, ramdisk
  fields (+ ext), an RNG seed in `setup_data`.
- The identity map at entry (`init_pgtable`) covers all old-kernel RAM, every
  kexec segment and the ACPI tables.

### Firecracker v1.17.0 (rust-vmm linux-loader)

- Loads the protected-mode kernel at the header's `code32_start` (must be
  ≥ 1 MiB) and enters at load + 0x200 in 64-bit mode.
- Its identity page tables cover only 0–1 GiB. Zero page at 0x7000, cmdline
  at 0x20000. e820: RAM 0–0x9fc00, reserved to 1 MiB, reserved PCI MMCONFIG,
  RAM from 1 MiB. Sets `acpi_rsdp_addr` = 0xe0000.

### QEMU 10 `-kernel` and qboot (`-M microvm`)

- Both enter the **16-bit real-mode setup code** at (setup segment + 0x20):0,
  not the 64-bit entry. Setup code at 0x10000, cmdline at 0x20000, the
  protected-mode kernel always at 0x100000; neither updates `code32_start`.
  QEMU places the initrd at (below_4g − acpi − 1 − size) & ~4095.
- So QEMU needs a real-mode stub in the setup sector plus a 32-bit entry,
  and the image must cope with a 1 MiB load address.

## Design settled on (unwritten)

- **Make the kernel physically relocatable at 4 KiB granularity** rather than
  copying it to 0x200000: under kexec the other segments avoid only the
  original load range, not 0x200000, and a copy overwrites the code doing
  the copying.
- **64-bit entry at image offset 0x200**, rip-relative only: zero `.bss`;
  copy boot_params (4 KiB) and the cmdline into `.bss` while still on the
  loader's identity map (both may sit above 4 GiB under kexec); build the
  boot tables (low 4 GiB identity + PHYS_OFFSET with 2 MiB pages, plus an
  identity PD for the GiB holding the load address if above 4 GiB); map the
  kernel window with 4 KiB PTEs `KERNEL_VA_BASE + 0x200000 + off → load +
  off`; switch CR3, GDT and segments at the high address; call
  `early_main(info, proto, load)`.
- **Kernel changes:** `kernel_load_phys()` becomes a runtime value;
  `image_virt_to_phys` / `mm::kernel_virt_to_phys` and `paging::table_ptr`
  subtract the actual load address; a `PlatformInfo::BootParams` variant; a
  host-testable crate parsing boot_params (e820 → memmap with the PVH memmap
  kinds, cmdline, initrd with ext_ high bits, `acpi_rsdp_addr`).
- **32-bit entry at offset 0:** find the load address via call/pop with a
  stack in the boot_params scratch field, build a temporary 4 GiB identity
  map in scratch past `__kernel_end` (counted in `init_size`), enter long
  mode, jump to the 64-bit entry.
- **Setup block:** boot sector, header and real-mode stub in kernel
  `global_asm` as a non-loaded section; the linker computes `syssize` and
  `init_size`; `build-kernel.sh` joins the dumped section with the
  `objcopy -O binary` output. Header: version 0x020F, loadflags LOADED_HIGH,
  xloadflags KERNEL_64 | CAN_BE_LOADED_ABOVE_4G, `kernel_alignment` 2 MiB,
  `pref_address` 0x200000, `code32_start` 0x100000, `cmdline_size` 4095,
  `initrd_addr_max` 0x7fffffff.
- **Timer:** `timer.rs` (D33) uses the xAPIC timer calibrated against the
  PIT, never TSC-deadline, so Fly's `lapic=notscdeadline` should not matter.
  Unverified.

## Fly deployment sketch (unverified)

An OCI image containing kexec-tools, the bzImage and the initrd cpio. Its
entrypoint reads the `virtio_mmio.device=` tokens from `/proc/cmdline` and
eth0's IPv4 address/gateway (Fly's proxy reaches the app over the
172.19.x.x/29 address; the 6PN IPv6 address is unusable since the stack is
IPv4 only), composes the patina cmdline per `doc/cli-contract.md`, then
`kexec -s -l … --initrd … --command-line … && kexec -e`. After the kexec
Fly's init is gone: no `fly ssh`, no log forwarding; patina's serial output
should appear in the console Fly captures. Expected extra start-up cost: one
Linux boot (~1 s).
