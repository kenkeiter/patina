#!/usr/bin/env bash
# Phase 5 robustness tests: build the kernel and testapps/robust for each
# arch and run every case, checking output lines and the exit status.
#
#   scripts/test-robust.sh [aarch64] [x86_64]     (default: both)
#
# Cases per arch (vCPUs from PATINA_CPUS, default 1, unless noted):
#   default   exit 0    every non-fatal check ("ok ..." lines), PASS robust
#   idle      exit 0    4 vCPUs (or PATINA_CPUS if more), patina.stats=1:
#                       time.Sleep(1s) within [1.0, 1.05] s, then 3 s idle;
#                       every CPU < 300 timer IRQs and all but one < 60
#                       (tickless idle; a 100 Hz tick would give ~700 each)
#   restart   exit 0    nanosleep continued through restart_syscall
#   reclaim   exit 0    kernel stack + page-table frame reclaim (1 and 4 vCPUs)
#   kheap     exit 0    PATINA_MEM=1024, 1 and 4 vCPUs: 50k pipes, 20k sockets,
#                       10k epolls, 200 MiB of tmpfs, each closed/deleted;
#                       the kernel heap returns its frames (D87): kernel-held
#                       frames and MemFree back to within 4 MiB of baseline
#   sigbus    exit 2    aarch64: [signal SIGBUS: bus error code=0x1 addr=<the
#                       unaligned address> pc=...]
#   sigill    exit 2    SIGILL: illegal instruction, sigcode=1 (arm64) / 2 (amd64)
#   gp        exit 2    x86_64: [signal SIGSEGV: segmentation violation code=0x80 addr=0x0 ...]
#   accerr    exit 2    [signal SIGSEGV: ... code=0x2 addr=<page+8> ...]
#   sigkill   exit 137  [patina] exit signal SIGKILL
#   sigpipe   exit 141  [patina] exit signal SIGPIPE
#   oom       exit 2    PATINA_MEM=256: the commit limit refuses Go's heap
#                       growth (D78) and Go prints "fatal error: runtime: out
#                       of memory"; no kernel panic / [patina] fatal
#   oomfault  exit 137  PATINA_MEM=256: touching a MAP_NORESERVE mapping
#                       past RAM: "[patina] out of memory: no free frame
#                       ..." and SIGKILL (the fault-time backstop)
#
# Program flags use the "-flag value" form. Honors PATINA_PROFILE and the
# run-qemu.sh environment. Logs go to out/test/robust/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test/robust
mkdir -p "$out"
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-180}
base_cpus=${PATINA_CPUS:-1}
idle_cpus=$(( base_cpus > 4 ? base_cpus : 4 ))

reason=
# run_case <arch> <log> <cpus> <mem> <want rc> <expected line>... -- <args>
# A line starting with "re:" is an extended regex matched against whole
# lines, "!re:" must match no line; anything else must appear verbatim.
run_case() {
  local arch=$1 log=$2 cpus=$3 mem=$4 want_rc=$5
  shift 5
  local expect=()
  while [[ $# -gt 0 && $1 != -- ]]; do expect+=("$1"); shift; done
  [[ $# -gt 0 ]] && shift
  local rc=0
  PATINA_CPUS=$cpus PATINA_MEM=$mem PATINA_LOG=$log.raw \
    "$root/scripts/run-qemu.sh" "$arch" "$out/robust-$arch.cpio" -- "$@" \
    >/dev/null 2>"$log.err" || rc=$?
  tr -d '\r' <"$log.raw" >"$log"
  local line
  for line in "${expect[@]}"; do
    if [[ $line == '!re:'* ]]; then
      if grep -qE -- "${line#!re:}" "$log"; then
        reason="unexpected line matching /${line#!re:}/ (run-qemu exit $rc)"
        return
      fi
    elif [[ $line == re:* ]]; then
      if ! grep -qxE -- "${line#re:}" "$log"; then
        reason="no line matching /${line#re:}/ (run-qemu exit $rc)"
        return
      fi
    elif ! grep -qxF -- "$line" "$log"; then
      reason="missing line '$line' (run-qemu exit $rc)"
      return
    fi
  done
  if [[ $rc -ne $want_rc ]]; then
    reason="run-qemu exit $rc, want $want_rc"
  fi
}

# The fault address Go reports must be the one the program printed.
check_addr() {
  local log=$1 printed=$2
  local want got
  want=$(sed -nE "s/^robust: $printed (0x[0-9a-f]+)$/\1/p" "$log" | head -1)
  got=$(sed -nE 's/^\[signal SIG[A-Z]+: .* addr=(0x[0-9a-f]+) pc=.*$/\1/p' "$log" | head -1)
  [[ -n $want && -n $got ]] || { reason="no printed/reported address (printed '$want', reported '$got')"; return; }
  if [[ $printed == "read-only page" ]]; then
    want=$(printf '0x%x' $(( want + 8 )))
  fi
  [[ $want == "$got" ]] || reason="fault address $got, want $want"
}

# Idle CPUs must not tick: every CPU < 300 timer IRQs, all but one < 60.
check_idle() {
  local log=$1 cpus=$2
  local counts n quiet=0 c
  counts=$(sed -nE 's/^\[patina\] stats cpu[0-9]+: timer irqs ([0-9]+),.*/\1/p' "$log")
  n=$(wc -l <<<"$counts" | tr -d ' ')
  [[ -n $counts && $n -eq $cpus ]] || { reason="expected $cpus '[patina] stats' lines, got $n"; return; }
  for c in $counts; do
    (( c < 300 )) || { reason="a CPU took $c timer IRQs (>= 300) over a ~6 s idle run"; return; }
    (( c < 60 )) && quiet=$((quiet + 1))
  done
  (( quiet >= cpus - 1 )) || reason="only $quiet of $cpus CPUs stayed under 60 timer IRQs: $(tr '\n' ' ' <<<"$counts")"
}

failed=0
results=()
for arch in "${arches[@]}"; do
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    results+=("FAIL $arch: kernel build failed")
    tail -n 30 "$out/build-$arch.log" >&2
    failed=1
    continue
  fi
  "$root/scripts/build-testapp.sh" robust "$arch" "$out/robust-$arch" >/dev/null
  "$root/scripts/mkinitrd.sh" "$out/robust-$arch.cpio" "$out/robust-$arch" >/dev/null
  if [[ $arch == aarch64 ]]; then sigcode=1; else sigcode=2; fi
  # No '|' inside patterns: it separates the case fields.
  nofatal='!re:^\[patina\] fatal|!re:^\[patina\] unhandled|!re:panicked at'
  pcre='pc=0x[0-9a-f]+'
  # name|cpus|mem|rc|expect...|--|args   (empty args field = no args)
  cases=(
    "default|$base_cpus|512|0|PASS robust|$nofatal|--"
    "idle|$idle_cpus|512|0|PASS robust|re:ok idle 3s .*|--|patina.stats=1|--|-mode|idle"
    "restart|$base_cpus|512|0|PASS robust|re:ok restart_syscall: .*|--|-mode|restart"
    "reclaim|1|512|0|PASS robust|re:ok kstack reclaim: .*|re:ok page-table reclaim: .*|--|-mode|reclaim"
    "reclaim-4cpu|4|512|0|PASS robust|re:ok kstack reclaim: .*|re:ok page-table reclaim: .*|--|-mode|reclaim"
    "kheap|1|1024|0|PASS robust|re:ok kheap shrink: .*|$nofatal|--|-mode|kheap"
    "kheap-4cpu|4|1024|0|PASS robust|re:ok kheap shrink: .*|$nofatal|--|-mode|kheap"
    "sigill|$base_cpus|512|2|SIGILL: illegal instruction|re:PC=0x[0-9a-f]+ m=[0-9]+ sigcode=$sigcode|[patina] exit code 2|--|-mode|sigill"
    "accerr|$base_cpus|512|2|re:\[signal SIGSEGV: segmentation violation code=0x2 addr=0x[0-9a-f]+ $pcre\]|[patina] exit code 2|--|-mode|accerr"
    "sigkill|$base_cpus|512|137|[patina] exit signal SIGKILL|[patina] exit code 137|--|-mode|sigkill"
    "sigpipe|$base_cpus|512|141|[patina] exit signal SIGPIPE|[patina] exit code 141|--|-mode|sigpipe"
    "oom|$base_cpus|256|2|re:fatal error: (runtime: )?out of memory|re:ok oom: .*|re:\[patina\] mmap: .* refused, commit limit reached .*|$nofatal|--|-mode|oom"
    "oomfault|$base_cpus|256|137|re:\[patina\] out of memory: no free frame for user page .*|[patina] exit signal SIGKILL|!re:panicked at|--|-mode|oomfault"
  )
  if [[ $arch == aarch64 ]]; then
    cases+=("sigbus|$base_cpus|512|2|re:\[signal SIGBUS: bus error code=0x1 addr=0x[0-9a-f]+ $pcre\]|[patina] exit code 2|--|-mode|sigbus")
  else
    cases+=("gp|$base_cpus|512|2|re:\[signal SIGSEGV: segmentation violation code=0x80 addr=0x0 $pcre\]|[patina] exit code 2|--|-mode|gp")
  fi
  for c in "${cases[@]}"; do
    IFS='|' read -r -a f <<<"$c"
    name=${f[0]}
    reason=
    log=$out/$name-$arch.log
    run_case "$arch" "$log" "${f[1]}" "${f[2]}" "${f[3]}" "${f[@]:4}"
    if [[ -z $reason ]]; then
      case $name in
        sigbus) check_addr "$log" "unaligned addr" ;;
        accerr) check_addr "$log" "read-only page" ;;
        idle) check_idle "$log" "${f[1]}" ;;
      esac
    fi
    if [[ -n $reason ]]; then
      echo "---- $name $arch: $reason (last 40 lines of $log) ----" >&2
      tail -n 40 "$log" >&2
      results+=("FAIL $arch $name: $reason")
      failed=1
    else
      results+=("PASS $arch $name")
      grep -E '^(ok |skip |PASS|robust: kheap |\[signal |SIGILL|PC=|fatal error|\[patina\] (exit|stats))' "$log" | sed "s|^|  $name $arch: |"
    fi
  done
done

printf '%s\n' "${results[@]}"
exit $failed
