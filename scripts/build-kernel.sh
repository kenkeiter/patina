#!/usr/bin/env bash
# Build the patina kernel for one architecture.
#
#   scripts/build-kernel.sh <aarch64|x86_64> [--debug]
#
# Outputs (in <repo>/out/):
#   aarch64: kernel-aarch64.Image (Linux arm64 Image, raw binary)
#            kernel-aarch64.elf   (ELF with symbols, for debugging)
#   x86_64:  kernel-x86_64-pvh.elf (PVH-bootable ELF)
#
# PATINA_PROFILE=dev|release (default release) or --debug selects the cargo profile.
set -euo pipefail

usage() { echo "usage: $0 <aarch64|x86_64> [--debug]" >&2; exit 2; }
[[ $# -ge 1 ]] || usage
arch=$1; shift
profile=${PATINA_PROFILE:-release}
for a in "$@"; do
  case $a in
    --debug) profile=dev ;;
    *) usage ;;
  esac
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=/dev/null
source "$root/env.sh"
cd "$root"

case $arch in
  aarch64) triple=aarch64-unknown-none ;;
  x86_64)  triple=x86_64-unknown-none ;;
  *) usage ;;
esac

if [[ $profile == release ]]; then
  cargo build --release -p patina-kernel --target "$triple"
  profdir=release
else
  cargo build -p patina-kernel --target "$triple"
  profdir=debug
fi

elf="$root/target/$triple/$profdir/patina-kernel"
mkdir -p "$root/out"

case $arch in
  aarch64)
    host=$(rustc -vV | sed -n 's/^host: //p')
    objcopy="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-objcopy"
    [[ -x $objcopy ]] || { echo "llvm-objcopy not found at $objcopy (rustup component add llvm-tools)" >&2; exit 1; }
    cp "$elf" "$root/out/kernel-aarch64.elf"
    "$objcopy" -O binary "$elf" "$root/out/kernel-aarch64.Image"
    echo "built $root/out/kernel-aarch64.Image ($(wc -c < "$root/out/kernel-aarch64.Image" | tr -d ' ') bytes, $profile)"
    ;;
  x86_64)
    cp "$elf" "$root/out/kernel-x86_64-pvh.elf"
    echo "built $root/out/kernel-x86_64-pvh.elf ($profile)"
    ;;
esac
