#!/usr/bin/env bash
# The Apple Virtualization.framework milestone (`patina run --vmm vz`,
# doc/cli-contract.md "Apple Virtualization.framework"): macOS on Apple
# Silicon only.
#
#   scripts/test-vz.sh
#
# Builds the aarch64 kernel, the ad-hoc signed CLI (cli/scripts/build.sh)
# and vz-aarch64 images of hello, signals, httphello, threads, sockets.
# Cases (all `patina run --vmm vz`, 2 vCPUs unless noted):
#   hello-exit       -- 3 a b: exit 3, "PASS hello (exit 3)"
#   console          no args: kernel banner, "hello from patina", the
#                    "[patina] exit code 0" sentinel, exit 0
#   ctl              signals -mode usr1: patina ctl ping (pong), time
#                    (2000-01-01), signal SIGUSR1 (console shows the stepped
#                    clock 9466848xx), dump on a second VM (goroutine dump,
#                    exit 2); stop -> 0
#   stop-handler     signals: ctl stop -> 0, "PASS signals (handler)"
#   stop-nohandler   signals -mode nohandler: ctl stop -> 143, "signal":"SIGTERM"
#   stop-forced      signals -mode ignore: ctl stop --grace 1s -> 137,
#                    "forced":true
#   net-serve        httphello with --net user: the guest address from the
#                    console's "[patina] net: dhcp lease A/P via GW" line (or
#                    a CLI "guest address A" line / state.json "guest_addr",
#                    whichever appears), GET http://A:8080/, 20 concurrent
#                    GETs, /big sha256, ctl stop -> 0
#   threads / signals-self / sockets   the Phase 2-4 programs at 2 vCPUs
#
# Environment: PATINA_TIMEOUT (per run, default 60 s; every run also has a
# hard outer timeout), PATINA_PARAMS (read by `patina run` itself: extra
# kernel params, e.g. a temporary workaround knob). Private XDG_RUNTIME_DIR;
# on exit only this script's `patina run` processes are killed (matched by
# its parent PID). Logs in out/test/vz/. One PASS/FAIL row per case; exits
# non-zero on any failure.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=$root/out/test/vz
mkdir -p "$out"
tmo=${PATINA_TIMEOUT:-60}
[[ $(uname -s) == Darwin && $(uname -m) == arm64 ]] || { echo "test-vz: needs macOS on Apple Silicon" >&2; exit 2; }
PY=$(command -v python3) || { echo "error: python3 not found" >&2; exit 2; }
command -v curl >/dev/null || { echo "error: curl not found" >&2; exit 2; }
TIMEOUT_BIN=$(command -v timeout || command -v gtimeout || true)
[[ -n $TIMEOUT_BIN ]] || { echo "error: coreutils 'timeout' not found" >&2; exit 2; }

state=$(mktemp -d /tmp/ptvz.XXXXXX)
export XDG_RUNTIME_DIR=$state
export PATINA_KERNEL_DIR=$root/out
cli=$root/cli/bin/patina

bgpids=()
kill_ours() {
  local p
  # `patina run` hosts the VZ VM in-process (no VMM PID). Our background
  # runs are `timeout ... patina run` processes in $bgpids: SIGTERM to
  # timeout(1) is forwarded to its `patina run` child (a graceful stop);
  # whatever is left is killed by parent PID, so other users' VMs are
  # never touched.
  [[ ${#bgpids[@]} -gt 0 ]] || return 0
  for p in "${bgpids[@]}"; do kill "$p" 2>/dev/null || true; done
  sleep 1
  for p in "${bgpids[@]}"; do
    pkill -9 -P "$p" 2>/dev/null || true
    kill -9 "$p" 2>/dev/null || true
  done
  bgpids=()
}
trap 'kill_ours; rm -rf "$state"' EXIT
trap 'exit 130' INT TERM

failed=0
results=()
pass() { results+=("PASS $1${2:+ ($2)}"); }
fail() {
  results+=("FAIL $1: $2"); failed=1
  echo "---- FAIL $1: $2" >&2
  [[ -n ${log:-} && -f $log ]] && tail -n 25 "$log" >&2
  true
}
has() { tr -d '\r' <"$1" | grep -qE -- "$2"; }

# wait_log <file> <regex> <secs> [pid]
wait_log() {
  local f=$1 re=$2 secs=$3 pid=${4:-} i
  for ((i = 0; i < secs * 10; i++)); do
    [[ -f $f ]] && has "$f" "$re" && return 0
    if [[ -n $pid ]] && ! kill -0 "$pid" 2>/dev/null; then
      [[ -f $f ]] && has "$f" "$re" && return 0
      return 1
    fi
    sleep 0.1
  done
  return 1
}

# vzrun <vm> <image> [run flags...] [-- args]: foreground run; sets $rc, $log.
vzrun() {
  local vm=$1 img=$2
  shift 2
  log=$out/$vm.log
  rc=0
  "$TIMEOUT_BIN" -k 5 $((tmo + 20)) "$cli" run "$out/$img.patina" --vmm vz --name "$vm" --cpus 2 \
    --timeout "${tmo}s" -v "$@" </dev/null >"$log" 2>"$log.err" || rc=$?
}

# vzbg <vm> <image> [run flags...] [-- args]: background run; sets $rpid, $log.
vzbg() {
  local vm=$1 img=$2
  shift 2
  log=$out/$vm.log
  rm -f "$log"
  "$TIMEOUT_BIN" -k 5 $((tmo + 20)) "$cli" run "$out/$img.patina" --vmm vz --name "$vm" --cpus 2 \
    --timeout "${tmo}s" -v "$@" </dev/null >"$log" 2>"$log.err" &
  rpid=$!
  bgpids+=("$rpid")
}

# finish <want rc>: wait for the background run; sets $rc.
finish() {
  rc=0
  wait "$rpid" || rc=$?
  bgpids=()
}

# ---- build
echo "== build" >&2
export PATH=/opt/homebrew/opt/rustup/bin:$PATH
if ! "$root/scripts/build-kernel.sh" aarch64 >"$out/build-kernel.log" 2>&1; then
  tail -n 30 "$out/build-kernel.log" >&2; echo "FAIL build: kernel"; exit 1
fi
if ! "$root/cli/scripts/build.sh" >"$out/build-cli.log" 2>&1; then
  tail -n 30 "$out/build-cli.log" >&2; echo "FAIL build: cli"; exit 1
fi
for app in hello signals httphello threads sockets; do
  if ! (cd "$root/testapps" && "$TIMEOUT_BIN" 300 "$cli" build "./$app" --target vz-aarch64 -o "$out/$app.patina") \
      >"$out/build-$app.log" 2>&1; then
    tail -n 30 "$out/build-$app.log" >&2; echo "FAIL build: $app"; exit 1
  fi
done

# ---- hello
echo "== hello-exit" >&2
vzrun vz-hello-args hello -- 3 a b
if [[ $rc -ne 3 ]]; then fail hello-exit "patina run exit $rc, want 3"
elif ! has "$log" '^PASS hello \(exit 3\)$'; then fail hello-exit "no 'PASS hello (exit 3)'"
else pass hello-exit
fi

echo "== console" >&2
vzrun vz-hello hello
if [[ $rc -ne 0 ]]; then fail console "patina run exit $rc, want 0"
elif ! has "$log" '^\[patina\] patina-kernel [0-9.]+ \(aarch64\)$'; then fail console "no kernel banner"
elif ! has "$log" '^\[patina\] console: hvc0'; then fail console "no hvc0 console line"
elif ! has "$log" '^hello from patina$'; then fail console "no program output"
elif ! has "$log" '^\[patina\] exit code 0$'; then fail console "no exit sentinel"
else pass console
fi

# ---- control channel (signals)
# ctl_ready <vm>: wait for the program's ready line and the channel.
ctl_ready() {
  wait_log "$log" '^signals: ready' 30 "$rpid" || return 1
  local i
  for ((i = 0; i < 50; i++)); do
    "$TIMEOUT_BIN" 10 "$cli" ctl "$1" ping --timeout 1s >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

echo "== ctl" >&2
vzbg vz-ctl signals -- -mode usr1
if ! ctl_ready vz-ctl; then
  fail ctl "program or control channel not ready"; kill_ours
else
  clog=$out/vz-ctl.ctl
  {
    "$TIMEOUT_BIN" 10 "$cli" ctl vz-ctl ping
    "$TIMEOUT_BIN" 10 "$cli" ctl vz-ctl signal SIGUSR1
    sleep 0.5
    "$TIMEOUT_BIN" 10 "$cli" ctl vz-ctl time
    "$TIMEOUT_BIN" 10 "$cli" ctl vz-ctl signal SIGUSR1
    sleep 0.5
  } >"$clog" 2>&1 || true
  # `ctl time` sends the host's clock; to see it step, send a fixed epoch
  # through the raw socket as test-ctl.sh does.
  "$PY" -I "$root/scripts/ctlclient.py" "$state/patina/vz-ctl/ctl.sock" --linger 1 \
    'send:{"op":"time","epoch_ns":946684800000000000}' 'send:{"op":"signal","sig":"USR1"}' 'sleep:1' \
    >>"$clog" 2>&1 || true
  crc=0
  "$TIMEOUT_BIN" 30 "$cli" ctl vz-ctl stop --grace 5s >>"$clog" 2>&1 || crc=$?
  finish
  now=$(date +%s)
  if [[ $rc -ne 0 || $crc -ne 0 ]]; then fail ctl "patina run exit $rc, ctl stop exit $crc, want 0/0"
  elif ! has "$clog" '"ev":"pong","id":[0-9]+,"uptime_ms":[0-9]+'; then fail ctl "no pong"
  elif ! has "$log" "^signals: SIGUSR1 at ${now:0:6}"; then fail ctl "no SIGUSR1 at host time"
  elif ! has "$log" '^signals: SIGUSR1 at 9466848[0-9][0-9]$'; then fail ctl "time op did not step the clock"
  elif ! has "$clog" '"ev":"exit","code":0'; then fail ctl "no exit 0 event"
  else pass ctl "ping, signal, time"
  fi
fi

echo "== ctl-dump" >&2
vzbg vz-dump signals
if ! ctl_ready vz-dump; then
  fail ctl-dump "program or control channel not ready"; kill_ours
else
  "$TIMEOUT_BIN" 10 "$cli" ctl vz-dump dump --linger 3s >"$out/vz-dump.ctl" 2>&1 || true
  finish
  if [[ $rc -ne 2 ]]; then fail ctl-dump "patina run exit $rc, want 2"
  elif ! has "$log" '^goroutine 1 .*\['; then fail ctl-dump "no goroutine dump"
  else pass ctl-dump
  fi
fi

# stop_case <label> <vm> <want> <ctl regex> <console regex> <ctl stop flags> -- <args>
stop_case() {
  local label=$1 vm=$2 want=$3 ctlre=$4 conre=$5 flags=$6
  shift 6
  vzbg "$vm" signals "$@"
  if ! ctl_ready "$vm"; then fail "$label" "program or control channel not ready"; kill_ours; return; fi
  local clog=$out/$vm.ctl crc=0 t0 t1
  t0=$(date +%s)
  # shellcheck disable=SC2086
  "$TIMEOUT_BIN" 60 "$cli" ctl "$vm" stop $flags >"$clog" 2>&1 || crc=$?
  t1=$(date +%s)
  finish
  if [[ $crc -ne $want ]]; then fail "$label" "ctl stop exit $crc, want $want"
  elif [[ $rc -ne $want ]]; then fail "$label" "patina run exit $rc, want $want"
  elif ! [[ $(tail -n 1 "$clog") =~ $ctlre ]]; then fail "$label" "ctl output: $(tail -n 1 "$clog")"
  elif ! has "$log" "$conre"; then fail "$label" "console: no /$conre/"
  else pass "$label" "$((t1 - t0)) s"
  fi
}

echo "== stop" >&2
stop_case stop-handler vz-stop-h 0 '"ev":"exit","code":0,"forced":false' '^PASS signals \(handler\)$' "--grace 5s" --
stop_case stop-nohandler vz-stop-n 143 '"ev":"exit","code":143,"signal":"SIGTERM"' '^\[patina\] exit code 143$' "--grace 5s" -- -mode nohandler
stop_case stop-forced vz-stop-f 137 '"ev":"exit","code":137,"forced":true' '^\[patina\] exit code 137$' "--grace 1s" -- -mode ignore

# ---- network
# guest_addr: the guest's IPv4 address, from (in order) state.json, a CLI
# line, or the kernel's DHCP line.
guest_addr() {
  local a=
  if [[ -f $state/patina/vz-net/state.json ]]; then
    a=$(grep -oE '"guest_addr(ess)?" *: *"[0-9.]+' "$state/patina/vz-net/state.json" | grep -oE '[0-9]+(\.[0-9]+){3}' | head -n 1 || true)
  fi
  [[ -n $a ]] || a=$(tr -d '\r' <"$log.err" "$log" 2>/dev/null | grep -ioE 'guest address:? *[0-9]+(\.[0-9]+){3}' | grep -oE '[0-9]+(\.[0-9]+){3}' | head -n 1 || true)
  [[ -n $a ]] || a=$(tr -d '\r' <"$log" | grep -oE '^\[patina\] net: dhcp lease [0-9]+(\.[0-9]+){3}' | grep -oE '[0-9]+(\.[0-9]+){3}' | head -n 1 || true)
  echo "$a"
}

big_sha=$("$PY" -I -c 'import hashlib;print(hashlib.sha256(bytes(((i*31+(i>>12))&255) for i in range(4<<20))).hexdigest())')
check_http() {
  local base=$1 d=$2 i b
  b=$(curl -s -m 20 "$base/") || { echo "GET / failed"; return 1; }
  [[ $b == "hello from patina" ]] || { echo "GET / body '$b'"; return 1; }
  local pids=()
  for i in $(seq 1 20); do
    curl -s -m 60 -o "$d.c$i" "$base/" &
    pids+=($!)
  done
  for i in "${pids[@]}"; do wait "$i" || { echo "concurrent GET failed"; return 1; }; done
  for i in $(seq 1 20); do
    [[ $(cat "$d.c$i") == "hello from patina" ]] || { echo "concurrent GET $i body '$(cat "$d.c$i")'"; return 1; }
    rm -f "$d.c$i"
  done
  curl -s -m 120 -o "$d.big" "$base/big" || { echo "GET /big failed"; return 1; }
  local sha
  sha=$(shasum -a 256 "$d.big" | cut -d' ' -f1)
  [[ $sha == "$big_sha" ]] || { echo "GET /big sha256 $sha ($(wc -c <"$d.big") bytes)"; return 1; }
  rm -f "$d.big"
}

echo "== net-serve" >&2
vzbg vz-net httphello --net user
if ! wait_log "$log" '^httphello: listening' 60 "$rpid"; then
  fail net-serve "no 'httphello: listening' line"; kill_ours
else
  addr=$(guest_addr)
  if [[ -z $addr ]]; then
    why="no dhcp lease line"
    has "$log" 'virtio-net: device rejected the feature set' && why="virtio-net: device rejected the feature set"
    fail net-serve "no guest address ($why)"; kill_ours
  elif ! err=$(check_http "http://$addr:8080" "$out/vz-net"); then
    fail net-serve "$addr: $err"; kill_ours
  else
    crc=0
    "$TIMEOUT_BIN" 60 "$cli" ctl vz-net stop --grace 5s >"$out/vz-net.ctl" 2>&1 || crc=$?
    finish
    if [[ $crc -ne 0 || $rc -ne 0 ]]; then fail net-serve "ctl stop exit $crc, patina run exit $rc, want 0/0"
    elif ! has "$log" '^PASS httphello \(graceful shutdown\)$'; then fail net-serve "no graceful shutdown line"
    else pass net-serve "guest $addr"
    fi
  fi
fi

# ---- Phase 2-4 subset at 2 vCPUs
# prog_case <label> <vm> <image> <want> <console regex> [-- args]
prog_case() {
  local label=$1 vm=$2 img=$3 want=$4 re=$5
  shift 5
  vzrun "$vm" "$img" "$@"
  if [[ $rc -ne $want ]]; then fail "$label" "patina run exit $rc, want $want"
  elif ! has "$log" "$re"; then fail "$label" "console: no /$re/"
  else pass "$label"
  fi
}
echo "== programs" >&2
prog_case threads vz-threads threads 0 '^PASS threads$' -- -cpus 2
prog_case signals-self vz-signals signals 0 '^PASS signals \(handler\)$' -- -self
prog_case signals-nohandler-self vz-signals-n signals 143 '^\[patina\] exit signal SIGTERM$' -- -mode nohandler -self
prog_case sockets vz-sockets sockets 0 '^PASS sockets$'

echo
printf '%s\n' "${results[@]}"
if [[ $failed -eq 0 ]]; then echo "test-vz: all passed"; else echo "test-vz: FAILURES (logs in $out)"; fi
exit $failed
