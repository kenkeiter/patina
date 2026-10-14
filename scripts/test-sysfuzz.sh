#!/usr/bin/env bash
# Syscall input-validation tests (testapps/sysfuzz): build the kernel and
# the program for each arch and run every mode. The program runs in user
# mode and must never be able to crash, hang or corrupt the kernel; every
# case FAILs on a kernel panic ("panicked at"), a "[patina] fatal" line, a
# timeout (run-qemu exit 124) or a missing exit sentinel (125).
#
#   scripts/test-sysfuzz.sh [aarch64] [x86_64]     (default: both)
#
# Cases per arch (1 vCPU unless noted):
#   cases            exit 0    ~230 deterministic argument combinations, each
#                              checked against the errno Linux returns
#                              (testapps/sysfuzz/cases.go cites the Linux
#                              source); "PASS sysfuzz cases (N)"
#   fuzz-s<S>-1cpu   exit 0    seeded random syscalls with random/edge
#   fuzz-s<S>-4cpu             arguments (fuzz.go documents the exclusion
#                              list) for seeds S in PATINA_FUZZ_SEEDS
#                              (default "1 2 3"), SYSFUZZ_ITERS calls
#                              (default 20000); 4 vCPUs run 4 workers.
#                              Only "PASS sysfuzz fuzz" and exit 0 are
#                              asserted: the kernel survived
#   sigreturn-*      see below hand-built rt_sigreturn frames (crash.go):
#                              a frame the kernel must accept (after
#                              sanitizing) runs exit_group(42); a frame it
#                              must reject kills the program by SIGSEGV
#                              (Go reports it and exits 2, or the kernel
#                              reports "exit signal" and 128+n); frames
#                              whose Linux outcome depends on the CPU may
#                              end either way
#   lastpage         x86_64 43 (Linux refuses the last user page) / aarch64
#                              a signal: a syscall from the end of user space
#
# SYSFUZZ_SKIP=mincore-huge skips the 16 TiB mincore case (a kernel without
# the chunked walk panics on it; used to list the other failures).
# Program flags use the "-flag value" form. Honors PATINA_PROFILE and the
# run-qemu.sh environment; PATINA_TIMEOUT defaults to 180 s (x86_64 runs
# under TCG; raise it on a busy host). Logs go to out/test/sysfuzz/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test/sysfuzz
mkdir -p "$out"
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-180}
seeds=${PATINA_FUZZ_SEEDS:-1 2 3}
iters=${SYSFUZZ_ITERS:-20000}

# rc_ok <rc> <spec>: spec is "/"-separated tokens: a number, "sig"
# (killed by a signal: 129-191) or "any" (anything but timeout / no
# sentinel).
rc_ok() {
  local rc=$1 tok
  IFS='/' read -r -a toks <<<"$2"
  for tok in "${toks[@]}"; do
    case $tok in
      any) [[ $rc -ne 124 && $rc -ne 125 ]] && return 0 ;;
      sig) [[ $rc -ge 129 && $rc -le 191 ]] && return 0 ;;
      *) [[ $rc -eq $tok ]] && return 0 ;;
    esac
  done
  return 1
}

reason=
# run_case <arch> <log> <cpus> <mem> <rc spec> <expected line>... -- <args>
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
    "$root/scripts/run-qemu.sh" "$arch" "$out/sysfuzz-$arch.cpio" -- "$@" \
    >/dev/null 2>"$log.err" || rc=$?
  LC_ALL=C tr -d '\r' <"$log.raw" >"$log"   # random path bytes reach the console
  if [[ $rc -eq 124 ]]; then reason="timeout (run-qemu exit 124)"; return; fi
  if [[ $rc -eq 125 ]]; then reason="no exit sentinel (run-qemu exit 125)"; return; fi
  local line
  for line in "${expect[@]}"; do
    if [[ $line == '!re:'* ]]; then
      if grep -aqE -- "${line#!re:}" "$log"; then
        reason="unexpected line matching /${line#!re:}/ (run-qemu exit $rc)"
        return
      fi
    elif [[ $line == re:* ]]; then
      if ! grep -aqxE -- "${line#re:}" "$log"; then
        reason="no line matching /${line#re:}/ (run-qemu exit $rc)"
        return
      fi
    elif ! grep -aqxF -- "$line" "$log"; then
      reason="missing line '$line' (run-qemu exit $rc)"
      return
    fi
  done
  if ! rc_ok "$rc" "$want_rc"; then
    reason="run-qemu exit $rc, want $want_rc"
  fi
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
  "$root/scripts/build-testapp.sh" sysfuzz "$arch" "$out/sysfuzz-$arch" >/dev/null
  "$root/scripts/mkinitrd.sh" "$out/sysfuzz-$arch.cpio" "$out/sysfuzz-$arch" >/dev/null
  # No '|' inside patterns: it separates the case fields.
  # Kernel failure signatures. "[patina] fatal: SIGSEGV ..." is the
  # kernel killing the program when a forced signal cannot be delivered
  # (a rejected sigreturn frame with a bad stack), i.e. a program death.
  nofatal='!re:^\[patina\] panic|!re:panicked at|!re:^\[patina\] fatal: user |!re:^\[patina\] unhandled'
  # name|cpus|mem|rc spec|expect...|--|args
  skip=
  [[ -n ${SYSFUZZ_SKIP:-} ]] && skip="|-skip|$SYSFUZZ_SKIP"
  cases=("cases|1|512|0|re:PASS sysfuzz cases \([0-9]+\)|PASS sysfuzz|$nofatal|--|-mode|cases$skip")
  for s in $seeds; do
    cases+=("fuzz-s$s-1cpu|1|512|0|PASS sysfuzz fuzz|PASS sysfuzz|$nofatal|--|-mode|fuzz|-seed|$s|-iters|$iters|-procs|2")
    cases+=("fuzz-s$s-4cpu|4|512|0|PASS sysfuzz fuzz|PASS sysfuzz|$nofatal|--|-mode|fuzz|-seed|$s|-iters|$iters|-procs|4")
  done
  # Crash modes: accepted frames exit 42, rejected ones die by SIGSEGV.
  accept="42"; reject="2/sig"; either="2/42/sig"
  cases+=("sigreturn-ok|1|512|$accept|$nofatal|--|-mode|sigreturn-ok")
  cases+=("sigreturn-garbage|1|512|$either|$nofatal|--|-mode|sigreturn-garbage|-seed|7")
  if [[ $arch == aarch64 ]]; then
    for m in il; do cases+=("sigreturn-$m|1|512|$accept|$nofatal|--|-mode|sigreturn-$m"); done
    for m in el1 daif aarch32 kpc nofp unaligned; do
      cases+=("sigreturn-$m|1|512|$reject|$nofatal|--|-mode|sigreturn-$m")
    done
    cases+=("lastpage|1|512|$reject|$nofatal|--|-mode|lastpage")
  else
    for m in krsp iopl ac; do cases+=("sigreturn-$m|1|512|$accept|$nofatal|--|-mode|sigreturn-$m"); done
    for m in krip noncanon-rip tf; do
      cases+=("sigreturn-$m|1|512|$reject|$nofatal|--|-mode|sigreturn-$m")
    done
    for m in mxcsr xsave fpunaligned fpkernel; do
      cases+=("sigreturn-$m|1|512|$either|$nofatal|--|-mode|sigreturn-$m")
    done
    cases+=("lastpage|1|512|43|$nofatal|--|-mode|lastpage")
  fi
  for c in "${cases[@]}"; do
    IFS='|' read -r -a f <<<"$c"
    name=${f[0]}
    reason=
    log=$out/$name-$arch.log
    run_case "$arch" "$log" "${f[1]}" "${f[2]}" "${f[3]}" "${f[@]:4}"
    if [[ -n $reason ]]; then
      echo "---- $name $arch: $reason (last 40 lines of $log) ----" >&2
      tail -n 40 "$log" >&2
      results+=("FAIL $arch $name: $reason")
      failed=1
    else
      results+=("PASS $arch $name")
      grep -aE '^(FAIL |PASS|sysfuzz (fuzz|cases):|\[signal |\[patina\] exit)' "$log" | sed "s|^|  $name $arch: |"
    fi
  done
done

printf '%s\n' "${results[@]}"
if [[ $failed -eq 0 ]]; then
  echo "test-sysfuzz: all passed (${#results[@]} cases)"
else
  echo "test-sysfuzz: $(printf '%s\n' "${results[@]}" | grep -c '^FAIL') of ${#results[@]} cases failed"
fi
exit $failed
