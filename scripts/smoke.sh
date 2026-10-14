#!/usr/bin/env bash
# Boot smoke test: build and boot the kernel for each requested arch and
# check the banner and a clean exit.
#
#   scripts/smoke.sh [aarch64] [x86_64]     (default: both)
#
# Honors the run-qemu.sh environment (PATINA_ACCEL, PATINA_TIMEOUT, ...).
# Prints PASS/FAIL per arch; exits non-zero if any arch fails.
set -uo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-60}

failed=0
results=()
for arch in "${arches[@]}"; do
  log=$(mktemp "${TMPDIR:-/tmp}/patina-smoke-$arch.XXXXXX")
  reason=
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$log.build" 2>&1; then
    reason="build failed (see below)"
    tail -n 30 "$log.build" >&2
  else
    PATINA_LOG=$log "$root/scripts/run-qemu.sh" "$arch" -- patina.smoke=1 >/dev/null 2>"$log.err"
    rc=$?
    clean=$(tr -d '\r' < "$log")
    if ! grep -qE "^\[patina\] patina-kernel [^ ]+ \($arch\)$" <<<"$clean"; then
      reason="banner missing (run-qemu exit $rc)"
    elif ! grep -qx "\[patina\] exit code 0" <<<"$clean"; then
      reason="no '[patina] exit code 0' (run-qemu exit $rc)"
    elif [[ $rc -ne 0 ]]; then
      reason="run-qemu exit $rc"
    fi
    if [[ -n $reason ]]; then
      echo "---- $arch serial output (last 40 lines) ----" >&2
      tail -n 40 <<<"$clean" >&2
      cat "$log.err" >&2
    fi
  fi
  rm -f "$log" "$log.build" "$log.err"
  if [[ -z $reason ]]; then
    results+=("PASS $arch")
  else
    results+=("FAIL $arch: $reason")
    failed=1
  fi
done

printf '%s\n' "${results[@]}"
exit $failed
