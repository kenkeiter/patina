#!/usr/bin/env bash
# Phase 2 umbrella test: the Go-runtime test programs on each arch and CPU
# count.
#
#   scripts/test-runtime.sh [aarch64] [x86_64]     (default: both)
#
# For each arch the kernel is built once (a build failure skips the arch),
# then for each PATINA_CPUS in ${PATINA_CPUS_LIST:-1 2 4}:
#   hello                      scripts/test-hello.sh (no args -> 0, `3 a b` -> 3)
#   threads, smpstress         scripts/test-smp.sh --app <name> --cpus N
#                              (both get `-cpus N`: GOMAXPROCS/NumCPU must match)
#   fdtest, pproftest          scripts/test-hello.sh --app <name>
#   signals / sigstress        scripts/test-signals.sh (signals handler and
#                              nohandler -self, sigstress, sigstress -procs 2,
#                              -crash, -breakpoint)
# The sub-scripts' per-case PASS/FAIL lines are collected into one table;
# their full output is kept in out/test/runtime/<arch>-cpus<n>-<suite>.out.
# Exits non-zero on any failure.
#
# Every QEMU run goes through scripts/run-qemu.sh with PATINA_TIMEOUT
# (default here 120 s per run). Honors PATINA_ACCEL, PATINA_PROFILE and the
# rest of the run-qemu.sh environment.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
read -r -a cpus_list <<<"${PATINA_CPUS_LIST:-1 2 4}"
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-120}
out=$root/out/test/runtime
mkdir -p "$out"

# suite name -> command (@N@ = the CPU count; arch appended)
suites=(
  "hello|scripts/test-hello.sh"
  "threads|scripts/test-smp.sh --app threads --cpus @N@"
  "smpstress|scripts/test-smp.sh --app smpstress --cpus @N@"
  "fdtest|scripts/test-hello.sh --app fdtest"
  "signals|scripts/test-signals.sh"
  "pproftest|scripts/test-hello.sh --app pproftest"
)

rows=()
failed=0
row() { rows+=("$(printf '%-4s  %-8s %-5s %-24s %s' "$1" "$2" "$3" "$4" "$5")"); }

for arch in "${arches[@]}"; do
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    tail -n 30 "$out/build-$arch.log" >&2
    row FAIL "$arch" - kernel-build "see $out/build-$arch.log"
    failed=1
    continue
  fi
  for n in "${cpus_list[@]}"; do
    for s in "${suites[@]}"; do
      name=${s%%|*}
      cmdline=${s#*|}
      read -r -a cmd <<<"${cmdline//@N@/$n}"
      log=$out/$arch-cpus$n-$name.out
      echo "== $arch PATINA_CPUS=$n $name" >&2
      rc=0
      PATINA_CPUS=$n "$root/${cmd[0]}" "${cmd[@]:1}" "$arch" >"$log" 2>&1 || rc=$?
      # The sub-script's result block: its trailing PASS/FAIL lines.
      results=$(awk '/^(PASS|FAIL) /{b = b $0 "\n"; next} {b = ""} END{printf "%s", b}' "$log")
      if [[ -z $results ]]; then
        row FAIL "$arch" "$n" "$name" "script exit $rc, no result lines (see $log)"
        failed=1
        continue
      fi
      while IFS= read -r line; do
        status=${line%% *}
        rest=${line#* }
        # Drop the arch word the sub-scripts include; "case: reason".
        # test-smp.sh adds "cpus=N run R" and a "(run-qemu exit N)" note.
        rest=$(sed -E "s/ cpus=[0-9]+ run [0-9]+//" <<<"${rest/$arch/}")
        case_name=$rest detail=
        if [[ $rest == *:* ]]; then
          case_name=${rest%%:*}
          detail=${rest#*:}
        elif [[ $rest == *" ("* ]]; then
          case_name=${rest%% (*}
          detail="(${rest#* (}"
        fi
        case_name=$(sed -E 's/^ +//; s/ +$//' <<<"$case_name")
        detail=$(sed -E 's/^ +//' <<<"$detail")
        [[ -n $case_name ]] || case_name=$name
        [[ $status == PASS ]] || failed=1
        row "$status" "$arch" "$n" "$case_name" "$detail"
      done <<<"$results"
      if [[ $rc -ne 0 ]] && ! grep -q '^FAIL' <<<"$results"; then
        row FAIL "$arch" "$n" "$name" "script exit $rc (see $log)"
        failed=1
      fi
    done
  done
done

echo
printf '%-4s  %-8s %-5s %-24s %s\n' RES ARCH CPUS CASE DETAIL
printf '%s\n' "${rows[@]}"
if [[ $failed -eq 0 ]]; then
  echo "test-runtime: all passed"
else
  echo "test-runtime: FAILURES (logs in $out)"
fi
exit $failed
