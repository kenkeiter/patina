#!/usr/bin/env bash
# End-to-end control-channel tests (design §10) over the QEMU virtio-console
# port "patina.ctl", driven by scripts/ctlclient.py.
#
#   scripts/test-ctl.sh [aarch64] [x86_64]     (default: both)
#
# Cases per arch (testapps/signals unless noted):
#   ready-ping      PATINA_CPUS=2: ready event with a go1.* version, cpus 2;
#                   ping id 42 -> pong id 42; malformed line / unknown op /
#                   SIGKILL -> error events; -enosys -> a log (warn) event
#                   "ENOSYS 9999 (?)"
#   stop-handler    ping, disconnect, reconnect, ping again (the channel
#                   survives a host reconnect); stop (grace 5 s) -> exit code
#                   0 event + "[patina] exit code 0"
#   stop-nohandler  -mode nohandler: stop -> exit 143 "signal":"SIGTERM"
#   stop-forced     -mode ignore: stop grace 1000 ms -> exit 137 forced:true
#                   within 2.5 s of the stop, "[patina] exit code 137"
#   dump            dump -> goroutine dump on serial ("goroutine 1 [") and exit 2
#   usr1-time       -mode usr1: signal SIGUSR1 -> "signals: SIGUSR1 at <now>";
#                   time 2000-01-01 -> next SIGUSR1 prints 9466848xx; stop -> 0
#   hvc0            PATINA_CONSOLE=hvc0: kernel + program output on the virtio
#                   console port 0 while ctl works on port 1 (ready, ping, stop)
#   panic           patina.selftest=panic (no initrd): {"ev":"panic","msg":"selftest"},
#                   then the dump and exit 255 on serial
#
# PATINA_VMM=vz (aarch64 only) runs the same cases under `patina run --vmm vz`
# through run-qemu.sh -> run-vz.sh: PATINA_CTL_SOCK becomes a symlink to the
# run's control proxy socket. Differences: the console is always hvc0 and
# the channel is virtio-vsock ("[patina] ctl: virtio-vsock port 4242"
# instead of the virtio-console port line); panic cannot hold the VM until
# the client connects (no PATINA_CTL_WAIT on VZ), so it checks the exit
# status (255, from `patina run`'s own panic event) and the console only.
#
# Requests are sent only after the program printed its "signals: ready" line
# on serial (ctlclient.py log:), so its handlers are installed even under TCG.
# Every QEMU run has PATINA_TIMEOUT (default 60 s). Artifacts in out/test/ctl/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test/ctl
mkdir -p "$out"
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-60}
PY=$(command -v python3) || { echo "error: python3 not found" >&2; exit 2; }
client=$root/scripts/ctlclient.py
vz=0
[[ ${PATINA_VMM:-qemu} == vz ]] && vz=1

# run_case <name> <arch> <cpio> <want rc> <client actions...> -- <program args...>
# Env for run-qemu.sh can be set by the caller. Leaves $log, $clog, $rc, $crc.
run_case() {
  local name=$1 arch=$2 cpio=$3
  shift 3
  local acts=()
  while [[ $# -gt 0 && $1 != -- ]]; do acts+=("$1"); shift; done
  [[ $# -gt 0 ]] && shift
  log=$out/$name-$arch.log
  clog=$out/$name-$arch.client
  local sock
  sock=$(mktemp -u "${TMPDIR:-/tmp}/patina-ctl-test.XXXXXX")
  rm -f "$log.raw"
  # The client first (it retries until QEMU creates the socket), so even a
  # guest that exits quickly is seen.
  "$PY" -I "$client" "$sock" --connect-timeout 30 --linger 3 --log "$log.raw" "${acts[@]}" >"$clog" 2>&1 &
  local cpid=$!
  sleep 0.5
  rc=0
  PATINA_CTL_SOCK=$sock PATINA_LOG=$log.raw "$root/scripts/run-qemu.sh" "$arch" ${cpio:+"$cpio"} \
    ${1+-- "$@"} >/dev/null 2>"$log.err" || rc=$?
  crc=0
  wait $cpid || crc=$?
  tr -d '\r' <"$log.raw" >"$log"
  rm -f "$sock"
}

failed=0
results=()
pass() { results+=("PASS $1"); }
fail() { results+=("FAIL $1: $2"); failed=1; echo "---- FAIL $1: $2" >&2; tail -n 15 "$clog" >&2 || true; tail -n 25 "$log" >&2 || true; }

# check <case label> <want rc> <client line regexes...> [-- <serial line regexes...>]
check() {
  local label=$1 want=$2
  shift 2
  if [[ $crc -ne 0 ]]; then fail "$label" "client status $crc"; return; fi
  if [[ $rc -ne $want ]]; then fail "$label" "run-qemu exit $rc, want $want"; return; fi
  local in_serial=0 re
  for re in "$@"; do
    if [[ $re == -- ]]; then in_serial=1; continue; fi
    if [[ $in_serial == 0 ]]; then
      grep -qE -- "$re" "$clog" || { fail "$label" "client: no line matching /$re/"; return; }
    else
      grep -qE -- "$re" "$log" || { fail "$label" "serial: no line matching /$re/"; return; }
    fi
  done
  pass "$label"
}

for arch in "${arches[@]}"; do
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    results+=("FAIL $arch: kernel build failed"); failed=1
    tail -n 30 "$out/build-$arch.log" >&2
    continue
  fi
  "$root/scripts/build-testapp.sh" signals "$arch" "$out/signals-$arch" >/dev/null
  "$root/scripts/mkinitrd.sh" "$out/signals-$arch.cpio" "$out/signals-$arch" >/dev/null
  cpio=$out/signals-$arch.cpio

  PATINA_CPUS=2 run_case ready-ping "$arch" "$cpio" \
    'wait@30:"ev":"ready"' 'log:signals: ready' \
    'send:{"op":"ping","id":42}' 'wait:"ev":"pong","id":42' \
    'send:this is not json' 'wait:"msg":"invalid json"' \
    'send:{"op":"reboot","id":5}' 'wait:"msg":"unknown op"' \
    'send:{"op":"signal","sig":"SIGKILL","id":6}' 'wait:"msg":"signal not injectable"' \
    'send:{"op":"stop","grace_ms":5000}' 'wait:"ev":"exit"' \
    -- -enosys
  check "$arch ready-ping" 0 '"ev":"ready","go":"go1\.[0-9]+' '"cpus":2' '"ev":"pong","id":42,"uptime_ms":[0-9]+' \
    '"ev":"log","level":"warn","msg":"ENOSYS 9999 ' \
    '"ev":"exit","code":0,"forced":false' -- '^\[patina\] smp: 2 cpus online' '^signals: syscall 9999: function not implemented' \
    '^PASS signals \(handler\)'

  run_case stop-handler "$arch" "$cpio" \
    'wait@30:"ev":"ready"' 'log:signals: ready' 'send:{"op":"ping","id":1}' 'wait:"id":1' 'reconnect' \
    'send:{"op":"ping","id":2}' 'wait:"id":2' 'send:{"op":"stop","grace_ms":5000}' 'wait:"ev":"exit"'
  check "$arch stop-handler" 0 '^<< \{"ev":"exit","code":0,"forced":false\}' 'ctlclient: reconnected' '"ev":"pong","id":2' \
    -- '^PASS signals \(handler\)' '^\[patina\] exit code 0$'

  run_case stop-nohandler "$arch" "$cpio" \
    'wait@30:"ev":"ready"' 'log:signals: ready' 'sleep:1' 'send:{"op":"stop","grace_ms":5000}' 'wait:"ev":"exit"' \
    -- -mode nohandler
  check "$arch stop-nohandler" 143 '^<< \{"ev":"exit","code":143,"signal":"SIGTERM","forced":false\}' \
    -- '^\[patina\] exit signal SIGTERM$' '^\[patina\] exit code 143$'

  run_case stop-forced "$arch" "$cpio" \
    'wait@30:"ev":"ready"' 'log:signals: ready' 'sleep:2' 'send:{"op":"stop","grace_ms":1000}' 'wait@5:"ev":"exit"' \
    -- -mode ignore
  check "$arch stop-forced" 137 '^<< \{"ev":"exit","code":137,"forced":true\}' \
    "matched '\"ev\":\"exit\"' (0\.|1\.|2\.[0-4])[0-9]*s after" \
    -- '^signals: ready, ignoring SIGTERM' '^\[patina\] ctl: grace period over' '^\[patina\] exit code 137$'

  run_case dump "$arch" "$cpio" \
    'wait@30:"ev":"ready"' 'log:signals: ready' 'sleep:1' 'send:{"op":"dump"}' 'wait:"ev":"exit"'
  check "$arch dump" 2 '^<< \{"ev":"exit","code":2,"forced":false\}' \
    -- '^SIGQUIT: quit' '^goroutine 1 .*\[' '^\[patina\] exit code 2$'

  run_case usr1-time "$arch" "$cpio" \
    'wait@30:"ev":"ready"' 'log:signals: ready' 'sleep:1' \
    'send:{"op":"signal","sig":"SIGUSR1"}' 'sleep:1' \
    'send:{"op":"time","epoch_ns":946684800000000000}' \
    'send:{"op":"signal","sig":"USR1"}' 'sleep:1' \
    'send:{"op":"stop","grace_ms":5000}' 'wait:"ev":"exit"' \
    -- -mode usr1
  now=$(date +%s)
  check "$arch usr1-time" 0 '"ev":"exit","code":0' \
    -- "^signals: SIGUSR1 at ${now:0:6}" '^signals: SIGUSR1 at 9466848[0-9][0-9]$' '^signals: got SIGTERM, exiting'

  PATINA_CONSOLE=hvc0 run_case hvc0 "$arch" "$cpio" \
    'wait@30:"ev":"ready"' 'log:signals: ready' 'send:{"op":"ping","id":1}' 'wait:"ev":"pong","id":1' \
    'send:{"op":"stop","grace_ms":5000}' 'wait:"ev":"exit"'
  ctl_re='^\[patina\] ctl: virtio-console port 1 "patina.ctl"'
  [[ $vz == 1 ]] && ctl_re='^\[patina\] ctl: virtio-vsock port 4242$'
  check "$arch hvc0" 0 '"ev":"exit","code":0' \
    -- '^\[patina\] console: hvc0 \(virtio-console port 0\)' "$ctl_re" \
    '^PASS signals \(handler\)' '^\[patina\] exit code 0$'

  if [[ $vz == 1 ]]; then
    # No client: the panic may precede any connection.
    log=$out/panic-$arch.log clog=/dev/null crc=0 rc=0
    PATINA_LOG=$log.raw "$root/scripts/run-qemu.sh" "$arch" -- patina.selftest=panic >/dev/null 2>"$log.err" || rc=$?
    tr -d '\r' <"$log.raw" >"$log"
    check "$arch panic" 255 -- '^\[patina\] panic: selftest at ' '^\[patina\] exit code 255$'
  else
    PATINA_CTL_WAIT=1 run_case panic "$arch" "" 'wait@30:"ev":"panic"' -- patina.selftest=panic
    check "$arch panic" 255 '^<< \{"ev":"panic","msg":"selftest"\}' \
      -- '^\[patina\] panic: selftest at ' '^\[patina\] exit code 255$'
  fi
done

printf '%s\n' "${results[@]}"
exit $failed
