#!/usr/bin/env bash
# Run Go's own package tests under patina: the Milestone B harness (design
# §13, D74; it generalises the Phase 3/4 os and net runs, D65/D73).
#
#   scripts/test-gopkg.sh <pkg>... [--arch aarch64|x86_64|both] [--cpus N]
#                         [--run REGEX] [--no-short] [--mem MiB]
#
# Default arch: both; --arch also takes a comma list. For each arch it
# builds the kernel (scripts/build-kernel.sh), and for each package
#   GOOS=linux GOARCH=<goarch> CGO_ENABLED=0 go test -c -trimpath \
#     -ldflags=-extldflags=-static -o <bin> <pkg>
# from testapps/, packs the binary as /app with a copy of $GOROOT/src/<pkg>
# at /src/<pkg> (testdata included: `go test` runs a test binary in its
# package directory and many tests read "."), $GOROOT/src/testdata at
# /src/testdata (net's sendfile tests read ../testdata), plus per-package
# extras (see pkg_extras below: os gets a small /usr/bin, time gets
# $GOROOT/lib/time/zoneinfo.zip at /lib/time/zoneinfo.zip, which
# time/internal_test.go loads as ../../lib/time/zoneinfo.zip), and runs it
# under scripts/run-qemu.sh with cwd /src/<pkg> (patina.cwd),
# patina.env=HOME=/root[,<pkg env>], a QEMU user-mode NIC (PATINA_NET=user,
# the CLI's static ip=10.0.2.15::10.0.2.2:255.255.255.0:patina:eth0:off:10.0.2.3
# line: eth0 up and /etc/resolv.conf pointing at slirp; harmless for
# non-net packages) and the program args
#   -test.v -test.timeout <t> -test.short -test.skip ^(A|B|...)$ [-test.run R]
# in the "-flag value" form (run-qemu.sh treats raw k=v words after `--`
# as kernel params). The skip regex comes from
# testapps/skiplist/<pkg with / -> _>.txt: one top-level test per line,
# `TestName  # reason`; a missing file means no skips.
#
# Per-package defaults (table in pkg_defaults below): guest RAM 1024 MiB,
# QEMU timeout 900 s, go -test.timeout 13m. Overrides, strongest first:
# --mem / PATINA_MEM / GOPKG_MEM_<P>, PATINA_TIMEOUT / GOPKG_TIMEOUT_<P>,
# GOPKG_TEST_TIMEOUT / GOPKG_GOTIMEOUT_<P>, where <P> is the package path
# with / and . replaced by _ (GOPKG_MEM_runtime=2048, GOPKG_MEM_net_http=...).
# Other environment: PATINA_CPUS (default for --cpus, 1), PATINA_ACCEL, ...
# as for run-qemu.sh; PATINA_PARAMS (extra kernel params, e.g.
# patina.strace=1); GOPKG_NO_KERNEL_BUILD=1 (reuse out/kernel-*).
#
# Prints per package and arch:
#   <pkg> <arch> (cpus=N): P passed, S skipped (K by our list), F failed, exit <rc>
# followed by the failing top-level tests and the unique `[patina] ENOSYS`
# lines, then a summary block. Only top-level `--- PASS/SKIP/FAIL` lines
# are counted (subtests are indented). Exits non-zero on any FAIL, a Go
# crash (`panic:` / `fatal error:`), a kernel fatal/panic, a missing final
# PASS line or a timeout. Logs: out/test/gopkg/<pkg>-<arch>-cpuN.log.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
usage() {
  echo "usage: $0 <pkg>... [--arch aarch64|x86_64|both] [--cpus N] [--run REGEX] [--no-short] [--mem MiB]" >&2
  exit 2
}

pkgs=()
arch_opt=both
cpus=${PATINA_CPUS:-1}
run_re=
short=1
mem_opt=
while [[ $# -gt 0 ]]; do
  case $1 in
    --arch) arch_opt=${2:?}; shift 2 ;;
    --cpus) cpus=${2:?}; shift 2 ;;
    --run) run_re=${2:?}; shift 2 ;;
    --no-short) short=0; shift ;;
    --mem) mem_opt=${2:?}; shift 2 ;;
    -h|--help) usage ;;
    -*) echo "test-gopkg: unknown option $1" >&2; usage ;;
    *) pkgs+=("$1"); shift ;;
  esac
done
[[ ${#pkgs[@]} -gt 0 ]] || usage
case $arch_opt in
  both) arches=(aarch64 x86_64) ;;
  *) IFS=, read -ra arches <<<"$arch_opt" ;;
esac
for a in "${arches[@]}"; do
  [[ $a == aarch64 || $a == x86_64 ]] || { echo "test-gopkg: unknown arch '$a'" >&2; exit 2; }
done

out=$root/out/test/gopkg
mkdir -p "$out"
export PATINA_NET=${PATINA_NET:-user}
export PATINA_CPUS=$cpus
goroot=$(go env GOROOT)
ipline=ip=10.0.2.15::10.0.2.2:255.255.255.0:patina:eth0:off:10.0.2.3

# pkg_defaults <pkg>: sets d_mem (MiB), d_timeout (QEMU, s), d_gotimeout.
pkg_defaults() {
  d_mem=1024 d_timeout=900 d_gotimeout=13m
  case $1 in
    runtime) d_mem=2048 d_timeout=1500 d_gotimeout=23m ;;
  esac
}

# pkg_extras <pkg> <dir>: extra initrd content under <dir> (mapped to /),
# and sets x_env (extra comma-separated patina.env items).
pkg_extras() {
  local pkg=$1 dir=$2
  x_env=
  case $pkg in
    os)
      # Test data, not a base directory: on Linux TestChdirAndGetwd and
      # TestReaddirnamesOneAtATime use /usr/bin. Plain, non-executable
      # entries: nothing should try to run them.
      mkdir -p "$dir/usr/bin"
      for f in README a b c; do echo "$f" >"$dir/usr/bin/$f"; done
      ln -s README "$dir/usr/bin/link"
      ;;
    time)
      mkdir -p "$dir/lib/time"
      cp "$goroot/lib/time/zoneinfo.zip" "$dir/lib/time/"
      ;;
  esac
}

# Skip names from the package's list: first word of each non-comment line.
# Test names are Go identifiers, so they need no escaping.
read_skips() {
  skip_names=()
  local f=$1 line name
  [[ -f $f ]] || return 0
  while IFS= read -r line; do
    line=${line%%#*}
    name=
    read -r name _ <<<"$line" || true
    [[ -n $name ]] && skip_names+=("$name")
  done <"$f"
  return 0
}

failed=0
summaries=()
for arch in "${arches[@]}"; do
  case $arch in
    aarch64) goarch=arm64 ;;
    x86_64) goarch=amd64 ;;
  esac
  if [[ ${GOPKG_NO_KERNEL_BUILD:-0} != 1 ]] &&
    ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    tail -n 30 "$out/build-$arch.log" >&2
    summaries+=("$arch: kernel build failed")
    failed=1
    continue
  fi
  for pkg in "${pkgs[@]}"; do
    pname=${pkg//\//_}
    vname=${pname//./_}
    [[ -d $goroot/src/$pkg ]] || { summaries+=("$pkg: no such package in $goroot/src"); failed=1; continue; }
    pkg_defaults "$pkg"
    v=GOPKG_MEM_$vname; mem=${mem_opt:-${PATINA_MEM:-${!v:-$d_mem}}}
    v=GOPKG_TIMEOUT_$vname; tmo=${PATINA_TIMEOUT:-${!v:-$d_timeout}}
    v=GOPKG_GOTIMEOUT_$vname; gotimeout=${GOPKG_TEST_TIMEOUT:-${!v:-$d_gotimeout}}
    read_skips "$root/testapps/skiplist/$pname.txt"

    tag=$pname-$arch-cpu$cpus
    log=$out/$tag.log
    bin=$out/$pname-$arch.test
    cpio=$out/$pname-$arch.cpio
    extra=$out/root-$pname
    rm -rf "$extra"
    mkdir -p "$extra/src/$(dirname "$pkg")"
    cp -R "$goroot/src/$pkg" "$extra/src/$(dirname "$pkg")/"
    [[ $pkg == testdata ]] || cp -R "$goroot/src/testdata" "$extra/src/"
    pkg_extras "$pkg" "$extra"
    chmod -R u+w "$extra"

    if ! (cd "$root/testapps" && GOOS=linux GOARCH=$goarch CGO_ENABLED=0 \
      go test -c -trimpath -ldflags=-extldflags=-static -o "$bin" "$pkg") >"$out/gobuild-$pname-$arch.log" 2>&1; then
      tail -n 30 "$out/gobuild-$pname-$arch.log" >&2
      summaries+=("$pkg $arch: go test -c failed")
      failed=1
      continue
    fi
    maps=()
    for d in "$extra"/*; do maps+=("$d:/$(basename "$d")/"); done
    "$root/scripts/mkinitrd.sh" "$cpio" "$bin" "${maps[@]}" >/dev/null

    env=HOME=/root${x_env:+,$x_env}
    args=(-test.v -test.timeout "$gotimeout")
    [[ $short == 0 ]] || args+=(-test.short)
    if [[ ${#skip_names[@]} -gt 0 ]]; then
      args+=(-test.skip "^($(IFS='|'; echo "${skip_names[*]}"))\$")
    fi
    [[ -n $run_re ]] && args+=(-test.run "$run_re")
    rc=0
    # shellcheck disable=SC2086
    PATINA_MEM=$mem PATINA_TIMEOUT=$tmo PATINA_LOG=$log.raw \
      "$root/scripts/run-qemu.sh" "$arch" "$cpio" -- \
      patina.cwd=/src/$pkg patina.env=$env $ipline ${PATINA_PARAMS:-} -- "${args[@]}" \
      >/dev/null 2>"$log.err" || rc=$?
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
    echo "$summary"
    grep -E '^--- FAIL: ' "$log" | sed 's/^--- FAIL: /  FAIL /' || true
    grep -oE '\[patina\] ENOSYS.*' "$log" | sort -u | sed 's/^/  /' || true
    if [[ ${#problems[@]} -gt 0 ]]; then
      echo "  problems: ${problems[*]} (log $log)"
      summary+="  [${problems[*]}]"
      failed=1
    fi
    summaries+=("$summary")
  done
done

echo "== test-gopkg summary"
printf '%s\n' "${summaries[@]}"
exit $failed
