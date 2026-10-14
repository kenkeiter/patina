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
#   PATINA_KERNEL   kernel image to boot (default out/kernel-<arch>...)
#   PATINA_LOG      file to capture serial output in (default: a temp file)
#   PATINA_QEMU_EXTRA  extra QEMU arguments (word-split)
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
    # ---- x86_64: STUB, TO BE FINISHED BY THE x86_64 BRING-UP ----
    # QEMU microvm (ACPI on, the default), 16550 at 0x3f8, PVH ELF via
    # -kernel. isa-debug-exit only terminates QEMU: QEMU reports
    # (value << 1) | 1 and only statuses >= 3 are trustworthy, so exit code 0
    # MUST come through the "[patina] exit code 0" serial sentinel. This
    # script always derives its exit status from the sentinel.
    qemu=qemu-system-x86_64
    kernel=${PATINA_KERNEL:-$root/out/kernel-x86_64-pvh.elf}
    accel=${PATINA_ACCEL:-tcg}
    console=ttyS0
    qargs=(-M microvm -accel "$accel" -cpu max
           -smp "$cpus" -m "$mem" -nodefaults -no-user-config -display none -no-reboot
           -device isa-debug-exit,iobase=0xf4,iosize=0x04
           -kernel "$kernel")
    ;;
  *) usage ;;
esac

[[ -f $kernel ]] || { echo "error: kernel $kernel not found (run scripts/build-kernel.sh $arch)" >&2; exit 2; }
command -v $qemu >/dev/null || { echo "error: $qemu not found" >&2; exit 2; }
[[ -n $initrd ]] && qargs+=(-initrd "$initrd")
cmdline="console=$console patina.epoch=$(date +%s)${params[*]:+ ${params[*]}} --${args[*]:+ ${args[*]}}"
qargs+=(-append "$cmdline" -chardev stdio,id=con0,signal=off -serial chardev:con0)
# shellcheck disable=SC2206
[[ -n ${PATINA_QEMU_EXTRA:-} ]] && qargs+=(${PATINA_QEMU_EXTRA})

log=${PATINA_LOG:-}
cleanup_log=
if [[ -z $log ]]; then log=$(mktemp "${TMPDIR:-/tmp}/patina-qemu.XXXXXX"); cleanup_log=1; fi
trap '[[ -n $cleanup_log ]] && rm -f "$log"' EXIT

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
