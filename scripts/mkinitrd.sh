#!/usr/bin/env bash
# Build a patina initrd (doc/cli-contract.md "initrd"): a newc cpio whose
# entry names have no leading slash, parents before children, uid/gid 0,
# ending with TRAILER!!!, and the program at `app` with mode 0755.
#
#   scripts/mkinitrd.sh <out.cpio> <app-binary> [src:dst ...]
#
# Each extra `src:dst` copies file `src` to `dst` inside the archive
# (a leading `/` on dst is dropped; parent directories are created 0755).
# Uses the host `cpio` (GNU cpio or bsdcpio/libarchive).
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <out.cpio> <app-binary> [src:dst ...]" >&2
  exit 2
fi
out=$1
app=$2
shift 2

[[ -f $app ]] || { echo "mkinitrd: no such file: $app" >&2; exit 1; }

stage=$(mktemp -d "${TMPDIR:-/tmp}/patina-initrd.XXXXXX")
trap 'rm -rf "$stage"' EXIT

cp "$app" "$stage/app"
chmod 0755 "$stage/app"

for spec in "$@"; do
  src=${spec%%:*}
  dst=${spec#*:}
  if [[ $src == "$spec" || -z $src || -z $dst ]]; then
    echo "mkinitrd: extra file must be src:dst, got '$spec'" >&2
    exit 2
  fi
  [[ -f $src ]] || { echo "mkinitrd: no such file: $src" >&2; exit 1; }
  dst=${dst#/}
  [[ $dst != app ]] || { echo "mkinitrd: 'app' is reserved for the program" >&2; exit 2; }
  mkdir -p "$stage/$(dirname "$dst")"
  cp "$src" "$stage/$dst"
done
find "$stage" -mindepth 1 -type d -exec chmod 0755 {} +

cpio_flags=(-o -H newc -R 0:0)
if cpio --help 2>&1 | grep -q -- --reproducible; then
  cpio_flags+=(--reproducible)
fi

out_abs=$(cd "$(dirname "$out")" && pwd)/$(basename "$out")
# C-locale sort keeps every parent directory before its children and makes
# the order deterministic.
(cd "$stage" && find . -mindepth 1 | sed 's|^\./||' | LC_ALL=C sort \
  | cpio "${cpio_flags[@]}" --quiet >"$out_abs")

size=$(wc -c <"$out_abs" | tr -d ' ')
echo "$out: $size bytes"
