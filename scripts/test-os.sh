#!/usr/bin/env bash
# Run Go's own `os` package tests under patina (design §13 Milestone B).
#
#   scripts/test-os.sh [aarch64] [x86_64]     (default: both)
#
# Builds the kernel and `go test -c os` for each arch, packs the test binary
# as /app together with a copy of the package directory at /src/os (with
# testdata/: `go test` runs a test binary in the package directory,
# and several tests read ".") and a small /usr/bin (TestChdirAndGetwd and
# TestReaddirnamesOneAtATime use it on Linux), and runs it with cwd /src/os
# (patina.cwd), HOME=/root and
#   -test.short -test.v -test.timeout <n> -test.skip ^(A|B|...)$
# where the skip regex comes from testapps/skiplist/os.txt (one top-level
# test per line, `TestName  # reason`). Flags use the "-flag value" form:
# run-qemu.sh treats raw k=v words after `--` as kernel params.
#
# Environment: PATINA_CPUS, PATINA_ACCEL, ... as for run-qemu.sh;
# PATINA_MEM (default 1024), PATINA_TIMEOUT (default 900),
# OS_TEST_TIMEOUT (go -test.timeout, default 12m), OS_TEST_RUN (optional
# -test.run regex, for debugging), OS_TEST_SHORT=0 (drop -test.short),
# PATINA_PARAMS (extra kernel params,
# e.g. patina.strace=1).
#
# Prints per arch:
#   os <arch>: N passed, M skipped (K by our list), F failed, exit <code>
# followed by the failing tests. Exits non-zero on any FAIL, a crash, a
# kernel fatal/panic, a missing final PASS line, or a timeout.
# Logs go to out/test/os/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test/os
mkdir -p "$out"
export PATINA_MEM=${PATINA_MEM:-1024}
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-900}
gotimeout=${OS_TEST_TIMEOUT:-12m}
skipfile=$root/testapps/skiplist/os.txt
goroot=$(go env GOROOT)

# Skip regex: first word of each non-comment line. Test names are Go
# identifiers, so they need no escaping.
skip_names=()
while IFS= read -r line; do
  line=${line%%#*}
  read -r name _ <<<"$line" || true
  [[ -n ${name:-} ]] && skip_names+=("$name")
done <"$skipfile"
skip_regex=
if [[ ${#skip_names[@]} -gt 0 ]]; then
  skip_regex="^($(IFS='|'; echo "${skip_names[*]}"))\$"
fi

# Extra initrd content, shared by both arches.
extra=$out/root
rm -rf "$extra"
mkdir -p "$extra/src" "$extra/usr/bin"
cp -R "$goroot/src/os" "$extra/src/"    # 1.7 MB; TestReadDir wants exec/
chmod -R u+w "$extra/src"
# Plain, non-executable entries: nothing should try to run them.
for f in README a b c; do echo "$f" >"$extra/usr/bin/$f"; done
ln -s README "$extra/usr/bin/link"

failed=0
summaries=()
for arch in "${arches[@]}"; do
  case $arch in
    aarch64) goarch=arm64 ;;
    x86_64) goarch=amd64 ;;
    *) echo "test-os: unknown arch '$arch'" >&2; exit 2 ;;
  esac
  cpus=${PATINA_CPUS:-1}
  tag=$arch-cpu$cpus
  log=$out/os-$tag.log
  bin=$out/os-$arch.test
  cpio=$out/os-$arch.cpio
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    tail -n 30 "$out/build-$arch.log" >&2
    summaries+=("os $arch: kernel build failed")
    failed=1
    continue
  fi
  (cd "$root/testapps" && GOOS=linux GOARCH=$goarch CGO_ENABLED=0 \
    go test -c -trimpath -ldflags=-extldflags=-static -o "$bin" os)
  "$root/scripts/mkinitrd.sh" "$cpio" "$bin" "$extra/src:/src/" "$extra/usr:/usr/" >/dev/null

  args=(-test.v -test.timeout "$gotimeout")
  [[ ${OS_TEST_SHORT:-1} == 0 ]] || args+=(-test.short)
  [[ -n $skip_regex ]] && args+=(-test.skip "$skip_regex")
  [[ -n ${OS_TEST_RUN:-} ]] && args+=(-test.run "$OS_TEST_RUN")
  rc=0
  # shellcheck disable=SC2086
  PATINA_LOG=$log.raw "$root/scripts/run-qemu.sh" "$arch" "$cpio" -- \
    patina.cwd=/src/os patina.env=HOME=/root ${PATINA_PARAMS:-} -- "${args[@]}" >/dev/null 2>"$log.err" || rc=$?
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
  summary="os $arch (cpus=$cpus): $npass passed, $((nskip + nlist)) skipped ($nlist by our list), $nfail failed, exit $rc"
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
