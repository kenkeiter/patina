#!/usr/bin/env bash
# Build the patina CLI. On macOS/Apple Silicon the binary is ad-hoc signed
# with the com.apple.security.virtualization entitlement, which `patina run
# --vmm vz` needs (Virtualization.framework refuses unsigned callers).
#
#   cli/scripts/build.sh [-o out] [go build flags...]
#
# Default output: cli/bin/patina. Cross builds (GOOS/GOARCH set to anything
# but darwin/arm64) are plain `go build`; the vz driver then reports
# "unsupported on this platform".
set -euo pipefail

cli=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out="$cli/bin/patina"
if [[ ${1:-} == -o ]]; then
  [[ $# -ge 2 ]] || { echo "usage: $0 [-o out] [go build flags...]" >&2; exit 2; }
  out=$2
  shift 2
fi
case $out in /*) ;; *) out="$PWD/$out" ;; esac
mkdir -p "$(dirname "$out")"

goos=${GOOS:-$(go env GOOS)}
goarch=${GOARCH:-$(go env GOARCH)}

cd "$cli"
if [[ $goos == darwin && $goarch == arm64 ]]; then
  CGO_ENABLED=1 go build "$@" -o "$out" .
  codesign --force --sign - --entitlements "$cli/scripts/patina.entitlements" "$out"
  if ! codesign -d --entitlements - "$out" 2>/dev/null | grep -q com.apple.security.virtualization; then
    echo "codesign: entitlement missing on $out" >&2
    exit 1
  fi
  echo "built $out (ad-hoc signed, com.apple.security.virtualization)"
else
  go build "$@" -o "$out" .
  echo "built $out ($goos/$goarch, no VZ support)"
fi
