#!/usr/bin/env bash
# End-to-end test: build the kernel and testapps/hello for each arch, boot
# it from an initrd and check the program's output and exit status.
#
#   scripts/test-hello.sh [aarch64] [x86_64]     (default: both)
#   scripts/test-hello.sh --threads [arch...]    run testapps/threads instead
#                                                (120 s timeout, output shown;
#                                                not a pass/fail test yet)
#   scripts/test-hello.sh --app <name> [arch...] run testapps/<name>: pass if it
#                                                prints 'PASS <name>' and exits 0
#                                                (PATINA_PARAMS: extra kernel
#                                                params, e.g. patina.strace=1)
#
# hello is run twice per arch: without arguments (expect exit 0) and with
# `3 a b` (expect exit 3). Honors the run-qemu.sh environment
# (PATINA_ACCEL, PATINA_TIMEOUT, ...). Artifacts go to out/test/.
# Prints PASS/FAIL per arch; exits non-zero if any arch fails.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
threads=0
app=
if [[ ${1:-} == --threads ]]; then
  threads=1
  shift
elif [[ ${1:-} == --app ]]; then
  [[ $# -ge 2 ]] || { echo "usage: $0 --app <name> [arch...]" >&2; exit 2; }
  app=$2
  shift 2
fi
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test
mkdir -p "$out"

# run_case <arch> <cpio> <log> <expected status> <expected line>... [-- args]
# Sets $reason on failure.
run_case() {
  local arch=$1 cpio=$2 log=$3 want_rc=$4
  shift 4
  local expect=()
  while [[ $# -gt 0 && $1 != -- ]]; do expect+=("$1"); shift; done
  local args=()
  [[ $# -gt 0 ]] && args=("$@")
  local rc=0
  PATINA_LOG=$log.raw "$root/scripts/run-qemu.sh" "$arch" "$cpio" ${args[@]+"${args[@]}"} \
    >/dev/null 2>"$log.err" || rc=$?
  tr -d '\r' <"$log.raw" >"$log"
  local line
  for line in "${expect[@]}"; do
    if ! grep -qxF -- "$line" "$log"; then
      reason="missing line '$line' (args: ${args[*]:-none}, run-qemu exit $rc)"
      return
    fi
  done
  if [[ $rc -ne $want_rc ]]; then
    reason="run-qemu exit $rc, want $want_rc (args: ${args[*]:-none})"
  fi
}

if [[ $threads -eq 1 ]]; then
  export PATINA_TIMEOUT=${PATINA_TIMEOUT:-120}
  for arch in "${arches[@]}"; do
    "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1 \
      || { tail -n 30 "$out/build-$arch.log" >&2; exit 1; }
    "$root/scripts/build-testapp.sh" threads "$arch" "$out/threads-$arch" >/dev/null
    "$root/scripts/mkinitrd.sh" "$out/threads-$arch.cpio" "$out/threads-$arch" >/dev/null
    rc=0
    PATINA_LOG=$out/threads-$arch.log "$root/scripts/run-qemu.sh" "$arch" \
      "$out/threads-$arch.cpio" >/dev/null 2>&1 || rc=$?
    echo "---- threads $arch (run-qemu exit $rc; full log $out/threads-$arch.log) ----"
    clean=$(tr -d '\r' <"$out/threads-$arch.log")
    sed -n '/^\[patina\] loader:/,$p' <<<"$clean" | head -n 40
    echo "..."
    tail -n 3 <<<"$clean"
  done
  exit 0
fi

if [[ -n $app ]]; then
  export PATINA_TIMEOUT=${PATINA_TIMEOUT:-120}
  failed=0
  results=()
  for arch in "${arches[@]}"; do
    reason=
    log=$out/$app-$arch.log
    if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
      reason="kernel build failed"
      tail -n 30 "$out/build-$arch.log" >&2
    elif ! "$root/scripts/build-testapp.sh" "$app" "$arch" "$out/$app-$arch" >"$out/build-$app-$arch.log" 2>&1 \
        || ! "$root/scripts/mkinitrd.sh" "$out/$app-$arch.cpio" "$out/$app-$arch" >>"$out/build-$app-$arch.log" 2>&1; then
      reason="$app build/initrd failed"
      tail -n 30 "$out/build-$app-$arch.log" >&2
    else
      # shellcheck disable=SC2086
      run_case "$arch" "$out/$app-$arch.cpio" "$log" 0 "PASS $app" '[patina] exit code 0' \
        ${PATINA_PARAMS:+-- $PATINA_PARAMS --}
      echo "---- $app $arch (log $log) ----"
      grep -vE '^\[(strace|run-qemu)\]' "$log" | sed -n '/^\[patina\] loader:/,$p' | tail -n 60
    fi
    if [[ -z $reason ]]; then
      results+=("PASS $app $arch")
    else
      results+=("FAIL $app $arch: $reason")
      failed=1
    fi
  done
  printf '%s\n' "${results[@]}"
  exit $failed
fi

export PATINA_TIMEOUT=${PATINA_TIMEOUT:-60}
failed=0
results=()
for arch in "${arches[@]}"; do
  reason=
  log=$out/hello-$arch
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    reason="kernel build failed"
    tail -n 30 "$out/build-$arch.log" >&2
  elif ! "$root/scripts/build-testapp.sh" hello "$arch" "$out/hello-$arch" >"$out/build-hello-$arch.log" 2>&1 \
      || ! "$root/scripts/mkinitrd.sh" "$out/hello-$arch.cpio" "$out/hello-$arch" >>"$out/build-hello-$arch.log" 2>&1; then
    reason="hello build/initrd failed"
    tail -n 30 "$out/build-hello-$arch.log" >&2
  else
    run_case "$arch" "$out/hello-$arch.cpio" "$log-noargs.log" 0 \
      'hello from patina' 'argv: ["/app"]' 'PASS hello (exit 0)' '[patina] exit code 0'
    failed_log=$log-noargs.log
    if [[ -z $reason ]]; then
      run_case "$arch" "$out/hello-$arch.cpio" "$log-args.log" 3 \
        'hello from patina' 'argv: ["/app" "3" "a" "b"]' 'PASS hello (exit 3)' '[patina] exit code 3' \
        -- 3 a b
      failed_log=$log-args.log
    fi
    if [[ -n $reason ]]; then
      echo "---- $arch serial output (last 40 lines of $failed_log) ----" >&2
      tail -n 40 "$failed_log" >&2
      cat "$failed_log.err" >&2 || true
    else
      grep -xE 'hello from patina|argv: .*|PASS hello .*|\[patina\] exit code [0-9]+' "$log-noargs.log" "$log-args.log" \
        | sed "s|^$out/||"
    fi
  fi
  if [[ -z $reason ]]; then
    results+=("PASS $arch")
  else
    results+=("FAIL $arch: $reason")
    failed=1
  fi
done

printf '%s\n' "${results[@]}"
exit $failed
