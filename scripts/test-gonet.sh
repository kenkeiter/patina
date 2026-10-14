#!/usr/bin/env bash
# Run Go's own `net` package tests under patina (design §13 Milestone B,
# D73), the network counterpart of scripts/test-os.sh.
#
#   scripts/test-gonet.sh [--pkg net|net/http] [aarch64] [x86_64]   (default: net, both)
#
# Builds the kernel and `go test -c <pkg>` for each arch, packs the test
# binary as /app with a copy of the package directory at /src/<pkg>
# (testdata included: `go test` runs a test binary in its package
# directory) and $GOROOT/src/testdata at /src/testdata, and runs it with
# cwd /src/<pkg> (patina.cwd), HOME=/root, a QEMU user-mode NIC
# (PATINA_NET=user: eth0 = 10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3
# through the static `ip=` line the CLI passes) and
#   -test.short -test.v -test.timeout <n> -test.skip ^(A|B|...)$
# where the skip regex comes from testapps/skiplist/<list>.txt (net.txt,
# net_http.txt for net/http; one top-level test per line,
# `TestName  # reason`).
#
# Environment: PATINA_CPUS, PATINA_ACCEL, ... as for run-qemu.sh;
# PATINA_MEM (default 1024), PATINA_TIMEOUT (default 900),
# GONET_TEST_TIMEOUT (go -test.timeout, default 13m), GONET_TEST_RUN
# (optional -test.run regex, for debugging), GONET_TEST_SHORT=0 (drop
# -test.short), PATINA_PARAMS (extra kernel params, e.g. patina.strace=1).
#
# Prints per arch:
#   <pkg> <arch> (cpus=N): N passed, M skipped (K by our list), F failed, exit <code>
# followed by the failing tests and the unique `[patina] ENOSYS` lines.
# Exits non-zero on any FAIL, a crash, a kernel fatal/panic, a missing
# final PASS line, or a timeout. Logs go to out/test/gonet/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
pkg=net
if [[ ${1:-} == --pkg ]]; then
  pkg=$2
  shift 2
fi
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test/gonet
mkdir -p "$out"
export PATINA_MEM=${PATINA_MEM:-1024}
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-900}
export PATINA_NET=${PATINA_NET:-user}
gotimeout=${GONET_TEST_TIMEOUT:-13m}
pname=${pkg//\//_}
skipfile=$root/testapps/skiplist/$pname.txt
goroot=$(go env GOROOT)
[[ -d $goroot/src/$pkg ]] || { echo "test-gonet: no package $pkg" >&2; exit 2; }
ipline=ip=10.0.2.15::10.0.2.2:255.255.255.0:patina:eth0:off:10.0.2.3

# Skip regex: first word of each non-comment line. Test names are Go
# identifiers, so they need no escaping.
skip_names=()
if [[ -f $skipfile ]]; then
  while IFS= read -r line; do
    line=${line%%#*}
    read -r name _ <<<"$line" || true
    [[ -n ${name:-} ]] && skip_names+=("$name")
  done <"$skipfile"
fi
skip_regex=
if [[ ${#skip_names[@]} -gt 0 ]]; then
  skip_regex="^($(IFS='|'; echo "${skip_names[*]}"))\$"
fi

# Extra initrd content, shared by both arches: the package directory and
# $GOROOT/src/testdata (net's sendfile tests read ../testdata).
extra=$out/root-$pname
rm -rf "$extra"
mkdir -p "$extra/src/$(dirname "$pkg")"
cp -R "$goroot/src/$pkg" "$extra/src/$(dirname "$pkg")/"
cp -R "$goroot/src/testdata" "$extra/src/"
chmod -R u+w "$extra/src"

failed=0
summaries=()
for arch in "${arches[@]}"; do
  case $arch in
    aarch64) goarch=arm64 ;;
    x86_64) goarch=amd64 ;;
    *) echo "test-gonet: unknown arch '$arch'" >&2; exit 2 ;;
  esac
  cpus=${PATINA_CPUS:-1}
  tag=$pname-$arch-cpu$cpus
  log=$out/$tag.log
  bin=$out/$pname-$arch.test
  cpio=$out/$pname-$arch.cpio
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    tail -n 30 "$out/build-$arch.log" >&2
    summaries+=("$pkg $arch: kernel build failed")
    failed=1
    continue
  fi
  (cd "$root/testapps" && GOOS=linux GOARCH=$goarch CGO_ENABLED=0 \
    go test -c -trimpath -ldflags=-extldflags=-static -o "$bin" "$pkg")
  "$root/scripts/mkinitrd.sh" "$cpio" "$bin" "$extra/src:/src/" >/dev/null

  args=(-test.v -test.timeout "$gotimeout")
  [[ ${GONET_TEST_SHORT:-1} == 0 ]] || args+=(-test.short)
  [[ -n $skip_regex ]] && args+=(-test.skip "$skip_regex")
  [[ -n ${GONET_TEST_RUN:-} ]] && args+=(-test.run "$GONET_TEST_RUN")
  rc=0
  # shellcheck disable=SC2086
  PATINA_LOG=$log.raw "$root/scripts/run-qemu.sh" "$arch" "$cpio" -- \
    patina.cwd=/src/$pkg patina.env=HOME=/root $ipline ${PATINA_PARAMS:-} -- "${args[@]}" >/dev/null 2>"$log.err" || rc=$?
  tr -d '\r' <"$log.raw" >"$log"
  rm -f "$log.raw"

  npass=$(grep -cE '^--- PASS: ' "$log" || true)
  nskip=$(grep -cE '^--- SKIP: ' "$log" || true)
  nfail=$(grep -cE '^--- FAIL: ' "$log" || true)
  nlist=${#skip_names[@]}
  problems=()
  [[ $rc -eq 0 ]] || problems+=("run-qemu exit $rc")
  [[ $rc -ne 124 ]] || problems+=("timeout")
  grep -qx 'PASS' "$log" || problems+=("no final PASS line")
  grep -qE '^\[patina\] (fatal|panic)|kernel panic' "$log" && problems+=("kernel fatal/panic")
  grep -qE '^(panic: |fatal error: )' "$log" && problems+=("test binary crashed")
  [[ $nfail -eq 0 ]] || problems+=("$nfail failed")
  summary="$pkg $arch (cpus=$cpus): $npass passed, $((nskip + nlist)) skipped ($nlist by our list), $nfail failed, exit $rc"
  summaries+=("$summary")
  echo "$summary"
  grep -E '^--- FAIL: ' "$log" | sed 's/^--- FAIL: /  FAIL /' || true
  grep -E '^\[patina\] ENOSYS' "$log" | sort -u | sed 's/^/  /' || true
  if [[ ${#problems[@]} -gt 0 ]]; then
    echo "  problems: ${problems[*]} (log $log)"
    failed=1
  fi
done

printf '%s\n' "${summaries[@]}"
exit $failed
