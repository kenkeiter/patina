#!/usr/bin/env bash
# SMP end-to-end test: run a test program at several vCPU counts.
#
#   scripts/test-smp.sh [--app <name>] [--cpus "1 2 4"] [--repeat N] [arch...]
#
# Default app: smpstress (passed `-cpus N`, must print 'PASS smpstress' and
# exit 0); `--app threads` also passes `-cpus N`; other apps get no args.
# Default arches: aarch64 x86_64. Honors the run-qemu.sh environment
# (PATINA_ACCEL, PATINA_TIMEOUT, ...) and PATINA_PROFILE (dev = debug
# assertions). Prints the app's own lines per run and PASS/FAIL per
# (arch, cpus, run); exits non-zero if any run fails. Logs: out/test/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
app=smpstress
cpus_list="1 2 4"
repeat=1
while [[ $# -gt 0 ]]; do
  case $1 in
    --app) app=$2; shift 2 ;;
    --cpus) cpus_list=$2; shift 2 ;;
    --repeat) repeat=$2; shift 2 ;;
    *) break ;;
  esac
done
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test
mkdir -p "$out"
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-180}

failed=0
results=()
for arch in "${arches[@]}"; do
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    tail -n 30 "$out/build-$arch.log" >&2
    results+=("FAIL $app $arch: kernel build failed")
    failed=1
    continue
  fi
  if ! "$root/scripts/build-testapp.sh" "$app" "$arch" "$out/$app-$arch" >"$out/build-$app-$arch.log" 2>&1 \
      || ! "$root/scripts/mkinitrd.sh" "$out/$app-$arch.cpio" "$out/$app-$arch" >>"$out/build-$app-$arch.log" 2>&1; then
    tail -n 30 "$out/build-$app-$arch.log" >&2
    results+=("FAIL $app $arch: build/initrd failed")
    failed=1
    continue
  fi
  for n in $cpus_list; do
    for r in $(seq 1 "$repeat"); do
      log=$out/$app-$arch-smp$n-$r.log
      args=()
      case $app in
        smpstress|threads) args=(-cpus "$n") ;;
      esac
      rc=0
      t0=$(date +%s)
      PATINA_CPUS=$n PATINA_LOG=$log.raw "$root/scripts/run-qemu.sh" "$arch" "$out/$app-$arch.cpio" \
        -- ${args[@]+"${args[@]}"} >/dev/null 2>"$log.err" || rc=$?
      t1=$(date +%s)
      tr -d '\r' <"$log.raw" >"$log"
      echo "---- $app $arch cpus=$n run $r (run-qemu exit $rc, $((t1 - t0)) s, log $log) ----"
      grep -E "^(\[patina\] (smp|random):|$app:|PASS|FAIL|\[patina\] (panic|fatal|exit code))" "$log" || true
      if grep -qxF "PASS $app" "$log" && [[ $rc -eq 0 ]]; then
        results+=("PASS $app $arch cpus=$n run $r")
      else
        results+=("FAIL $app $arch cpus=$n run $r (run-qemu exit $rc)")
        tail -n 25 "$log" >&2
        failed=1
      fi
    done
  done
done
printf '%s\n' "${results[@]}"
exit $failed
