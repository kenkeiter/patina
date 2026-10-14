#!/usr/bin/env bash
# Build a test program from testapps/ as a static GOOS=linux binary.
#
#   scripts/build-testapp.sh <name> <aarch64|x86_64> <out>
#
# e.g. scripts/build-testapp.sh hello aarch64 out/hello-aarch64
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <name> <aarch64|x86_64> <out>" >&2
  exit 2
fi
name=$1
arch=$2
out=$3

case $arch in
  aarch64) goarch=arm64 ;;
  x86_64) goarch=amd64 ;;
  *) echo "build-testapp: unknown arch '$arch' (aarch64|x86_64)" >&2; exit 2 ;;
esac

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
[[ -d $root/testapps/$name ]] || { echo "build-testapp: no testapps/$name" >&2; exit 1; }

mkdir -p "$(dirname "$out")"
out_abs=$(cd "$(dirname "$out")" && pwd)/$(basename "$out")

cd "$root/testapps"
GOOS=linux GOARCH=$goarch CGO_ENABLED=0 \
  go build -trimpath -ldflags=-extldflags=-static -o "$out_abs" "./$name"
echo "$out_abs"
