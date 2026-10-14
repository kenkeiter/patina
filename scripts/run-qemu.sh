#!/usr/bin/env bash
# Boot the patina kernel under QEMU.
#
#   scripts/run-qemu.sh <aarch64|x86_64> [initrd.cpio] [-- kernel args...]
#
# Kernel cmdline follows doc/cli-contract.md:
#   console=<dev> patina.epoch=<unix secs> [params...] -- [args...]
# `--` is always present. Script arguments after the script's `--` are
# program args; if they contain their own `--`, the words before it are
# extra kernel params and the words after it are args (merged, not doubled):
#   run-qemu.sh aarch64 -- a b                  -> ... -- a b
#   run-qemu.sh aarch64 -- foo=bar -- a b       -> ... foo=bar -- a b
# Raw `key=value` words (e.g. `-- patina.selftest=panic`) also work as kernel
# params after `--`: the CLI percent-encodes `=` in real args, so the kernel
# treats raw `k=v` tokens after `--` as params.
#
# Environment:
#   PATINA_MEM      guest RAM in MiB (default 512)
#   PATINA_CPUS     vCPUs (default 1)
#   PATINA_ACCEL    hvf | kvm | tcg (default: hvf on Apple Silicon, kvm if
#                   /dev/kvm is usable, else tcg)
#   PATINA_TIMEOUT  hard timeout in seconds (default 60)
#   PATINA_MACHINE  x86_64 only: QEMU machine type (default microvm; pc, q35 work)
#   PATINA_KERNEL   kernel image to boot (default out/kernel-<arch>...)
#   PATINA_LOG      file to capture serial output in (default: a temp file)
#   PATINA_QEMU_EXTRA  extra QEMU arguments (word-split)
#   PATINA_CTL_SOCK control channel: Unix socket (QEMU server, wait=off) for
#                   the virtio-serial port "patina.ctl" (default: a temp path,
#                   removed afterwards). Devices match the CLI's layout
#                   (doc/cli-contract.md): virtio-serial-device + virtserialport
#                   name=patina.ctl + virtio-rng-device.
#   PATINA_CTL_WAIT 1: QEMU waits for a client on PATINA_CTL_SOCK before
#                   starting the guest (wait=on; tests of very short runs)
#   PATINA_CONSOLE  hvc0: kernel console on virtio-console port 0 (a
#                   virtconsole on the same virtio-serial bus) instead of the
#                   UART; passes console=hvc0. Default: the UART.
#   PATINA_VIRTIO   0: no virtio devices at all (default 1; 0 for x86 pc/q35,
#                   which have no virtio-mmio bus)
#
# Serial output is streamed to the terminal and captured. Exit status:
#   N    from the last "[patina] exit code N" line
#   124  timeout (guest did not power off in time)
#   125  no exit sentinel seen
set -euo pipefail

usage() { echo "usage: $0 <aarch64|x86_64> [initrd.cpio] [-- kernel args...]" >&2; exit 2; }
[[ $# -ge 1 ]] || usage
arch=$1; shift
initrd=
if [[ $# -gt 0 && $1 != "--" ]]; then initrd=$1; shift; fi
if [[ $# -gt 0 ]]; then
  [[ $1 == "--" ]] || usage
  shift
fi
params=()
args=()
if [[ " $* " == *" -- "* ]]; then
  while [[ $# -gt 0 && $1 != "--" ]]; do params+=("$1"); shift; done
  shift
fi
args=("$@")

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mem=${PATINA_MEM:-512}
cpus=${PATINA_CPUS:-1}
tmo=${PATINA_TIMEOUT:-60}
TIMEOUT_BIN=$(command -v timeout || command -v gtimeout || true)
[[ -n $TIMEOUT_BIN ]] || { echo "error: coreutils 'timeout' not found" >&2; exit 2; }

if [[ -n $initrd && ! -f $initrd ]]; then echo "error: initrd $initrd not found" >&2; exit 2; fi

pick_accel() {
  local qemu=$1
  if [[ -n ${PATINA_ACCEL:-} ]]; then echo "$PATINA_ACCEL"; return; fi
  local avail
  avail=$("$qemu" -accel help 2>/dev/null || true)
  if [[ $(uname -s) == Darwin && $(uname -m) == arm64 && $arch == aarch64 ]] && grep -qx hvf <<<"$avail"; then
    echo hvf
  elif [[ $(uname -m) == "$arch" || ( $(uname -m) == arm64 && $arch == aarch64 ) ]] && [[ -r /dev/kvm && -w /dev/kvm ]] && grep -qx kvm <<<"$avail"; then
    echo kvm
  else
    echo tcg
  fi
}

virtio=${PATINA_VIRTIO:-1}
ctl_wait=off
[[ ${PATINA_CTL_WAIT:-0} == 1 ]] && ctl_wait=on
case $arch in
  aarch64)
    qemu=qemu-system-aarch64
    kernel=${PATINA_KERNEL:-$root/out/kernel-aarch64.Image}
    accel=$(pick_accel $qemu)
    case $accel in
      hvf|kvm) cpu=host ;;
      *) accel=tcg; cpu=max ;;
    esac
    console=ttyAMA0
    qargs=(-M virt,gic-version=3 -accel "$accel" -cpu "$cpu"
           -smp "$cpus" -m "$mem" -nodefaults -no-user-config -display none -no-reboot
           -kernel "$kernel")
    ;;
  x86_64)
    # QEMU microvm (ACPI on, the default) with a 16550 at 0x3f8, booting
    # the PVH ELF via -kernel. isa-debug-exit only terminates QEMU: QEMU
    # reports (value << 1) | 1 and only statuses >= 3 are trustworthy, so
    # exit code 0 MUST come through the "[patina] exit code 0" serial
    # sentinel. This script always derives its exit status from the sentinel.
    # PATINA_MACHINE overrides the machine type (e.g. pc, q35).
    qemu=qemu-system-x86_64
    kernel=${PATINA_KERNEL:-$root/out/kernel-x86_64-pvh.elf}
    accel=${PATINA_ACCEL:-tcg}
    case $accel in
      kvm|hvf) cpu=host ;;
      *) cpu=max ;;
    esac
    machine=${PATINA_MACHINE:-microvm}
    [[ $machine == microvm* ]] || virtio=${PATINA_VIRTIO:-0}
    console=ttyS0
    qargs=(-M "$machine" -accel "$accel" -cpu "$cpu"
           -smp "$cpus" -m "$mem" -nodefaults -no-user-config -display none -no-reboot
           -device isa-debug-exit,iobase=0xf4,iosize=0x04
           -kernel "$kernel")
    ;;
  *) usage ;;
esac

[[ -f $kernel ]] || { echo "error: kernel $kernel not found (run scripts/build-kernel.sh $arch)" >&2; exit 2; }
command -v $qemu >/dev/null || { echo "error: $qemu not found" >&2; exit 2; }
[[ -n $initrd ]] && qargs+=(-initrd "$initrd")
ctl_sock=${PATINA_CTL_SOCK:-}
cleanup_sock=
if [[ $virtio != 0 ]]; then
  if [[ -z $ctl_sock ]]; then
    ctl_sock=$(mktemp -u "${TMPDIR:-/tmp}/patina-ctl.XXXXXX"); cleanup_sock=1
  fi
  rm -f "$ctl_sock"
fi
if [[ ${PATINA_CONSOLE:-} == hvc0 ]]; then
  [[ $virtio != 0 ]] || { echo "error: PATINA_CONSOLE=hvc0 needs virtio" >&2; exit 2; }
  console=hvc0
fi
cmdline="console=$console patina.epoch=$(date +%s)${params[*]:+ ${params[*]}} --${args[*]:+ ${args[*]}}"
qargs+=(-append "$cmdline" -chardev stdio,id=con0,signal=off)
if [[ $virtio != 0 ]]; then
  qargs+=(-device virtio-serial-device,id=vser0)
  if [[ $console == hvc0 ]]; then
    # The first virtconsole on the bus is port 0 (the console port).
    qargs+=(-device virtconsole,bus=vser0.0,chardev=con0)
  else
    qargs+=(-serial chardev:con0)
  fi
  qargs+=(-chardev "socket,id=ctl0,path=$ctl_sock,server=on,wait=${ctl_wait}"
          -device virtserialport,bus=vser0.0,chardev=ctl0,name=patina.ctl
          -device virtio-rng-device)
else
  qargs+=(-serial chardev:con0)
fi
# shellcheck disable=SC2206
[[ -n ${PATINA_QEMU_EXTRA:-} ]] && qargs+=(${PATINA_QEMU_EXTRA})

log=${PATINA_LOG:-}
cleanup_log=
if [[ -z $log ]]; then log=$(mktemp "${TMPDIR:-/tmp}/patina-qemu.XXXXXX"); cleanup_log=1; fi
trap '[[ -n $cleanup_log ]] && rm -f "$log"; [[ -n $cleanup_sock ]] && rm -f "$ctl_sock"; true' EXIT

echo "[run-qemu] $qemu ${qargs[*]}" >&2
set +e
# stdin is only wired to the guest when interactive.
if [[ -t 0 ]]; then
  "$TIMEOUT_BIN" --foreground -k 5 "$tmo" $qemu "${qargs[@]}" | tee "$log"
else
  "$TIMEOUT_BIN" --foreground -k 5 "$tmo" $qemu "${qargs[@]}" </dev/null | tee "$log"
fi
rc=${PIPESTATUS[0]}
set -e

if [[ $rc -eq 124 || $rc -eq 137 ]]; then
  echo "[run-qemu] timeout after ${tmo}s" >&2
  exit 124
fi
code=$(tr -d '\r' < "$log" | sed -n 's/^\[patina\] exit code \([0-9][0-9]*\)$/\1/p' | tail -n 1)
if [[ -z $code ]]; then
  echo "[run-qemu] no exit sentinel (qemu status $rc)" >&2
  exit 125
fi
exit "$code"
