#!/usr/bin/env bash
# End-to-end signal tests: build the kernel, testapps/signals and
# testapps/sigstress for each arch and check output and exit status.
#
#   scripts/test-signals.sh [aarch64] [x86_64]     (default: both)
#
# Cases per arch:
#   signals -self                    exit 0,   PASS signals (handler)
#   signals -mode nohandler -self    exit 143, [patina] exit signal SIGTERM
#   sigstress                        exit 0,   PASS sigstress
#   sigstress -procs 2               exit 0,   PASS sigstress
#   sigstress -crash                 exit 2,   [signal SIGSEGV: ... code=0x1 addr=0x0 ...]
#   sigstress -breakpoint            exit 2,   SIGTRAP: trace trap
#
# Program flags use the "-flag value" form: run-qemu.sh treats raw k=v
# words after `--` as kernel params. Honors PATINA_PROFILE (dev|release)
# and the run-qemu.sh environment. Artifacts go to out/test/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test
mkdir -p "$out"
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-120}

# run_case <arch> <cpio> <log> <expected status> <expected line>... [-- args]
# A line starting with "re:" is an extended regex matched against whole
# lines; anything else must appear verbatim as a whole line. Sets $reason.
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
    if [[ $line == re:* ]]; then
      if ! grep -qxE -- "${line#re:}" "$log"; then
        reason="no line matching /${line#re:}/ (args: ${args[*]:-none}, run-qemu exit $rc)"
        return
      fi
    elif ! grep -qxF -- "$line" "$log"; then
      reason="missing line '$line' (args: ${args[*]:-none}, run-qemu exit $rc)"
      return
    fi
  done
  if [[ $rc -ne $want_rc ]]; then
    reason="run-qemu exit $rc, want $want_rc (args: ${args[*]:-none})"
  fi
}

failed=0
results=()
for arch in "${arches[@]}"; do
  reason=
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    results+=("FAIL $arch: kernel build failed")
    tail -n 30 "$out/build-$arch.log" >&2
    failed=1
    continue
  fi
  for app in signals sigstress; do
    "$root/scripts/build-testapp.sh" $app "$arch" "$out/$app-$arch" >/dev/null
    "$root/scripts/mkinitrd.sh" "$out/$app-$arch.cpio" "$out/$app-$arch" >/dev/null
  done
  s=$out/signals-$arch.cpio
  t=$out/sigstress-$arch.cpio
  cases=(
    "signals-handler|$s|0|PASS signals (handler)|[patina] exit code 0|--|-self"
    "signals-nohandler|$s|143|[patina] exit signal SIGTERM|[patina] exit code 143|--|-mode|nohandler|-self"
    "sigstress|$t|0|PASS sigstress|[patina] exit code 0"
    "sigstress-procs2|$t|0|sigstress: GOMAXPROCS=2|PASS sigstress|--|-procs|2"
    "sigstress-crash|$t|2|re:\[signal SIGSEGV: segmentation violation code=0x1 addr=0x0 pc=0x[0-9a-f]+\]|[patina] exit code 2|--|-crash"
    "sigstress-breakpoint|$t|2|SIGTRAP: trace trap|[patina] exit code 2|--|-breakpoint"
  )
  for c in "${cases[@]}"; do
    IFS='|' read -r -a f <<<"$c"
    name=${f[0]}
    reason=
    log=$out/$name-$arch.log
    run_case "$arch" "${f[1]}" "$log" "${f[2]}" "${f[@]:3}"
    if [[ -n $reason ]]; then
      echo "---- $name $arch: $reason (last 40 lines of $log) ----" >&2
      tail -n 40 "$log" >&2
      results+=("FAIL $arch $name: $reason")
      failed=1
    else
      results+=("PASS $arch $name")
      grep -E '^(sigstress:|signals:|PASS|\[signal |SIGTRAP|\[patina\] exit)' "$log" | sed "s|^|  $name: |"
    fi
  done
done

printf '%s\n' "${results[@]}"
exit $failed
