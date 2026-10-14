#!/usr/bin/env bash
# Per-syscall benchmarks (design §14 Phase 5, decision D79): build the kernel
# and testapps/bench for each arch, run it under run-qemu.sh at each vCPU
# count, several times, and print a table of medians per arch.
#
#   scripts/bench.sh [arch...] [--cpus N[,N...]] [--runs R] [--novdso]
#                    [-n F] [--only name,name...]
#
#   arch       aarch64 and/or x86_64 (default: both)
#   --cpus     vCPU counts, comma-separated or repeated (default 1)
#   --runs     runs per configuration; the table shows the median (default 3)
#   --novdso   also run the clock/getrandom rows with patina.novdso=1 (an
#              extra "novdso" column per vCPU count)
#   -n F       scale every iteration count and duration in the program
#   --only L   only these benchmarks (testapps/bench -list shows the names)
#
# Environment: PATINA_MEM (default 512), PATINA_TIMEOUT (default 600 s per
# run), GO (Go command for the build, default go), and the rest of the
# run-qemu.sh environment (PATINA_ACCEL, ...). Logs go to out/bench/.
# Exits non-zero if any run fails to print 'PASS bench' and exit 0.
#
# Numbers on a shared host are noisy: other QEMU guests compete for the
# CPUs, hence the median. x86_64 runs under TCG (an emulator) on Apple
# Silicon, so its absolute numbers are not comparable with aarch64/HVF.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=()
cpus_list=()
runs=3
novdso=0
scale=
only=
while [[ $# -gt 0 ]]; do
  case $1 in
    --cpus) [[ $# -ge 2 ]] || { echo "bench: --cpus needs a value" >&2; exit 2; }
            IFS=, read -r -a c <<<"$2"; cpus_list+=("${c[@]}"); shift 2 ;;
    --runs) runs=$2; shift 2 ;;
    --novdso) novdso=1; shift ;;
    -n) scale=$2; shift 2 ;;
    --only) only=$2; shift 2 ;;
    aarch64|x86_64) arches+=("$1"); shift ;;
    *) echo "usage: $0 [arch...] [--cpus N[,N...]] [--runs R] [--novdso] [-n F] [--only L]" >&2; exit 2 ;;
  esac
done
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
[[ ${#cpus_list[@]} -gt 0 ]] || cpus_list=(1)
export PATINA_MEM=${PATINA_MEM:-512}
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-600}

clock_rows=clock_gettime-syscall,time.Now,getrandom32-syscall,crypto-rand-read32
out=$root/out/bench
mkdir -p "$out"
failed=0

# run_one <arch> <cpus> <tag> <params...> -- <args...>: appends
# "<name> <value> <unit>" lines to $out/<arch>-c<cpus>-<tag>.results.
run_one() {
  local arch=$1 ncpu=$2 tag=$3 i log rc
  shift 3
  for ((i = 1; i <= runs; i++)); do
    log=$out/$arch-c$ncpu-$tag-r$i.log
    rc=0
    PATINA_CPUS=$ncpu PATINA_LOG=$log.raw "$root/scripts/run-qemu.sh" "$arch" \
      "$out/bench-$arch.cpio" "$@" >/dev/null 2>"$log.err" || rc=$?
    tr -d '\r' <"$log.raw" >"$log"
    rm -f "$log.raw"
    if [[ $rc -ne 0 ]] || ! grep -qx 'PASS bench' "$log"; then
      echo "bench: $arch cpus=$ncpu $tag run $i failed (run-qemu exit $rc, log $log)" >&2
      grep -E '^FAIL|panic' "$log" >&2 || tail -n 10 "$log" >&2
      failed=1
      continue
    fi
    sed -nE 's/^bench ([^ :]+): ([0-9.]+) (.+)$/\1 \2 \3/p' "$log" \
      >>"$out/$arch-c$ncpu-$tag.results"
    echo "bench: $arch cpus=$ncpu $tag run $i/$runs done" >&2
  done
}

for arch in "${arches[@]}"; do
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    tail -n 30 "$out/build-$arch.log" >&2; exit 1
  fi
  "$root/scripts/build-testapp.sh" bench "$arch" "$out/bench-$arch" >/dev/null
  "$root/scripts/mkinitrd.sh" "$out/bench-$arch.cpio" "$out/bench-$arch" >/dev/null
  rm -f "$out/$arch"-c*.results
  args=()
  [[ -n $scale ]] && args+=(-n "$scale")
  [[ -n $only ]] && args+=(-only "$only")
  cols=()
  for ncpu in "${cpus_list[@]}"; do
    run_one "$arch" "$ncpu" vdso -- ${args[@]+"${args[@]}"}
    cols+=("c$ncpu-vdso")
    if [[ $novdso -eq 1 ]]; then
      nargs=()
      [[ -n $scale ]] && nargs+=(-n "$scale")
      nargs+=(-only "${only:-$clock_rows}")
      run_one "$arch" "$ncpu" novdso -- patina.novdso=1 -- "${nargs[@]}"
      cols+=("c$ncpu-novdso")
    fi
  done

  # Table: one row per benchmark (program order), one column per config.
  accel=${PATINA_ACCEL:-}
  if [[ -z $accel ]]; then
    if [[ $arch == aarch64 && $(uname -s) == Darwin && $(uname -m) == arm64 ]]; then accel=hvf
    elif [[ -w /dev/kvm ]]; then accel=kvm
    else accel=tcg; fi
  fi
  echo
  echo "== $arch ($accel; median of $runs runs; logs in out/bench/) =="
  {
    printf 'benchmark'
    for col in "${cols[@]}"; do
      printf '\t%s' "cpus=${col#c}" | sed 's/-vdso$//'
    done
    printf '\tunit\n'
    names=$(awk '{print $1}' "$out/$arch-c${cpus_list[0]}-vdso.results" 2>/dev/null | awk '!seen[$0]++')
    for name in $names; do
      printf '%s' "$name"
      unit=
      for col in "${cols[@]}"; do
        f=$out/$arch-$col.results
        vals=$(awk -v n="$name" '$1 == n {print $2}' "$f" 2>/dev/null | sort -g)
        u=$(awk -v n="$name" '$1 == n {print $3; exit}' "$f" 2>/dev/null)
        [[ -n $u ]] && unit=$u
        if [[ -z $vals ]]; then printf '\t-'; continue; fi
        cnt=$(wc -l <<<"$vals" | tr -d ' ')
        printf '\t%s' "$(sed -n "$(((cnt + 1) / 2))p" <<<"$vals")"
      done
      printf '\t%s\n' "$unit"
    done
  } | column -t -s $'\t'
done
exit $failed
