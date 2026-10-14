#!/usr/bin/env bash
# Milestone A on QEMU (design §13): testapps/httphello served to the host
# through `patina run --net user --publish`, plus DHCP, DNS and virtio-pci.
#
#   scripts/test-net.sh [aarch64] [x86_64]     (default: both)
#
# Cases per arch:
#   serve       patina run --net user --publish P:8080: GET / == "hello from
#               patina", 20 concurrent GETs, GET /big (4 MiB) sha256, then
#               `patina ctl <vm> stop` -> {"ev":"exit","code":0,...},
#               "PASS httphello (graceful shutdown)", patina run exits 0
#   nohandler   the same with -nohandler: ctl stop -> code 143 + "signal":
#               "SIGTERM", patina run exits 143
#   selftest    patina run --net user -- -selftest: loopback GET / /big /echo,
#               Go resolver via /etc/hosts (localhost, the guest host name)
#               and, with PATINA_NET_EXTERNAL=1, -dns <PATINA_NET_DNS_NAME,
#               default example.com> through slirp's 10.0.2.3 -> exit 0
#   dhcp        run-qemu.sh, PATINA_NET=user, ip=dhcp -selftest: the
#               "dhcp lease 10.0.2.15/24 via 10.0.2.2, dns 10.0.2.3" line
#   dhcp-nonic  ip=dhcp without a NIC: "no network device" line, program runs
#   dhcp-nolease ip=dhcp with a NIC on a dead link (-netdev socket,udp to the
#               discard port): "no lease after 10 s" on serial and as a
#               control-channel log event, program runs, boot bounded
#   pci         run-qemu.sh with virtio-pci (x86_64: PATINA_MACHINE=q35;
#               aarch64: PATINA_VIRTIO=pci), static ip=, PATINA_NET=user:P-8080:
#               curl works, stop via scripts/ctlclient.py -> exit 0
#
# Host ports: PATINA_NET_PORT (default derived from the PID) + 0..9 for
# aarch64 and + 10..19 for x86_64, each checked free before use, so two
# invocations (or both arches) can run side by side. Every QEMU run has a
# hard timeout (PATINA_TIMEOUT, default 120 s; patina run gets --timeout);
# every QEMU this script started is killed on exit (matched by this run's
# private state directory, never other QEMUs). Logs in out/test/net/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
out=$root/out/test/net
mkdir -p "$out"
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-120}
tmo=$PATINA_TIMEOUT
PY=$(command -v python3) || { echo "error: python3 not found" >&2; exit 2; }
command -v curl >/dev/null || { echo "error: curl not found" >&2; exit 2; }
TIMEOUT_BIN=$(command -v timeout || command -v gtimeout || true)
[[ -n $TIMEOUT_BIN ]] || { echo "error: coreutils 'timeout' not found" >&2; exit 2; }
client=$root/scripts/ctlclient.py
port_base=${PATINA_NET_PORT:-$((20000 + ($$ % 400) * 50))}
ext=${PATINA_NET_EXTERNAL:-0}
dns_name=${PATINA_NET_DNS_NAME:-example.com}
static_ip=ip=10.0.2.15::10.0.2.2:255.255.255.0:patina:eth0:off:10.0.2.3

# Private state: patina run's run dirs and the ctl sockets live here, so
# every QEMU this script starts has this path on its command line. Short:
# Unix socket paths are limited to 103 bytes.
state=$(mktemp -d /tmp/ptnet.XXXXXX)
export XDG_RUNTIME_DIR=$state
export PATINA_KERNEL_DIR=$root/out

bgpids=()
kill_ours() {
  local p
  for p in "${bgpids[@]}"; do kill "$p" 2>/dev/null || true; done
  # Only QEMUs whose command line names our state directory.
  for p in $(pgrep -f "qemu-system.*$state" 2>/dev/null || true); do
    kill "$p" 2>/dev/null || true
  done
  sleep 0.5
  for p in $(pgrep -f "qemu-system.*$state" 2>/dev/null || true); do
    kill -9 "$p" 2>/dev/null || true
  done
  bgpids=()
}
trap 'kill_ours; rm -rf "$state"' EXIT
trap 'exit 130' INT TERM

# free_port <candidate>: the first port >= candidate nobody listens on.
free_port() {
  local p=$1
  "$PY" -I - "$p" <<'EOF'
import socket, sys
p = int(sys.argv[1])
for q in range(p, p + 200):
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", q))
        s.close()
        print(q)
        break
    except OSError:
        s.close()
EOF
}

big_sha=$("$PY" -I -c 'import hashlib;print(hashlib.sha256(bytes(((i*31+(i>>12))&255) for i in range(4<<20))).hexdigest())')

failed=0
results=()
pass() { results+=("PASS $1"); }
fail() {
  results+=("FAIL $1: $2"); failed=1
  echo "---- FAIL $1: $2" >&2
  [[ -n ${log:-} && -f $log ]] && tail -n 25 "$log" >&2
  true
}

# wait_log <file> <regex> <secs> [pid]: wait until file has a matching line
# (fails early when pid exits).
wait_log() {
  local f=$1 re=$2 secs=$3 pid=${4:-} i
  for ((i = 0; i < secs * 10; i++)); do
    [[ -f $f ]] && tr -d "\r" <"$f" | grep -qE -- "$re" && return 0
    if [[ -n $pid ]] && ! kill -0 "$pid" 2>/dev/null; then
      [[ -f $f ]] && tr -d "\r" <"$f" | grep -qE -- "$re" && return 0
      return 1
    fi
    sleep 0.1
  done
  return 1
}

has() { tr -d '\r' <"$1" | grep -qE -- "$2"; }

# check_http <port> <label>: GET /, 20 concurrent GETs, /big sha256.
# Echoes an error and returns 1 on failure.
check_http() {
  local port=$1 d=$2 i b
  b=$(curl -s -m 20 "http://127.0.0.1:$port/") || { echo "GET / failed"; return 1; }
  [[ $b == "hello from patina" ]] || { echo "GET / body '$b'"; return 1; }
  local pids=()
  for i in $(seq 1 20); do
    curl -s -m 60 -o "$d.c$i" "http://127.0.0.1:$port/" &
    pids+=($!)
  done
  for i in "${pids[@]}"; do wait "$i" || { echo "concurrent GET failed"; return 1; }; done
  for i in $(seq 1 20); do
    [[ $(cat "$d.c$i") == "hello from patina" ]] || { echo "concurrent GET $i body '$(cat "$d.c$i")'"; return 1; }
    rm -f "$d.c$i"
  done
  curl -s -m 120 -o "$d.big" "http://127.0.0.1:$port/big" || { echo "GET /big failed"; return 1; }
  local sha
  sha=$(shasum -a 256 "$d.big" | cut -d' ' -f1)
  [[ $sha == "$big_sha" ]] || { echo "GET /big sha256 $sha ($(wc -c <"$d.big") bytes), want $big_sha"; return 1; }
  rm -f "$d.big"
  return 0
}

# serve_case <label> <vm> <port> <want code> <ctl regex> <console regex> [prog args...]
serve_case() {
  local label=$1 vm=$2 port=$3 want=$4 ctlre=$5 conre=$6
  shift 6
  log=$out/$vm.log
  local clog=$out/$vm.ctl
  rm -f "$log" "$clog"
  "$TIMEOUT_BIN" -k 5 "$tmo" "$cli" run --name "$vm" --mem 256 --net user --publish "$port:8080" \
    --timeout "$((tmo - 10))s" "$img" ${1+-- "$@"} >"$log" 2>"$log.err" &
  local rpid=$!
  bgpids+=("$rpid")
  if ! wait_log "$log" '^httphello: (listening|no signal handler)' 90 "$rpid"; then
    fail "$label" "no 'httphello: listening' line"; kill_ours; return
  fi
  wait_log "$log" '^httphello: no signal handler' 1 >/dev/null || true
  local err
  if ! err=$(check_http "$port" "$out/$vm"); then
    fail "$label" "$err"; kill_ours; return
  fi
  local crc=0
  "$TIMEOUT_BIN" -k 5 60 "$cli" ctl "$vm" stop >"$clog" 2>&1 || crc=$?
  local rc=0
  wait "$rpid" || rc=$?
  bgpids=()
  if [[ $crc -ne $want ]]; then fail "$label" "ctl stop exit $crc, want $want"; return; fi
  if [[ $rc -ne $want ]]; then fail "$label" "patina run exit $rc, want $want"; return; fi
  [[ $(tail -n 1 "$clog") =~ $ctlre ]] || { fail "$label" "ctl output does not end with /$ctlre/: $(tail -n 1 "$clog")"; return; }
  if [[ -n $conre ]] && ! has "$log" "$conre"; then fail "$label" "console: no /$conre/"; return; fi
  pass "$label"
}

echo "building the CLI" >&2
cli=$out/patina
(cd "$root/cli" && go build -o "$cli" .) || { echo "error: CLI build failed" >&2; exit 1; }

for arch in "${arches[@]}"; do
  if ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    results+=("FAIL $arch: kernel build failed"); failed=1
    tail -n 30 "$out/build-$arch.log" >&2
    continue
  fi
  img=$out/httphello-$arch.patina
  if ! (cd "$root/testapps" && "$TIMEOUT_BIN" 300 "$cli" build ./httphello --target "qemu-$arch" -o "$img") \
      >"$out/pbuild-$arch.log" 2>&1; then
    results+=("FAIL $arch: patina build failed"); failed=1
    tail -n 30 "$out/pbuild-$arch.log" >&2
    continue
  fi
  "$root/scripts/build-testapp.sh" httphello "$arch" "$out/httphello-$arch" >/dev/null
  "$root/scripts/mkinitrd.sh" "$out/httphello-$arch.cpio" "$out/httphello-$arch" >/dev/null
  cpio=$out/httphello-$arch.cpio
  off=0
  [[ $arch == x86_64 ]] && off=10

  # serve
  port=$(free_port $((port_base + off)))
  echo "== $arch serve (port $port)" >&2
  serve_case "$arch serve" "net-$arch-serve" "$port" 0 \
    '\{"ev":"exit","code":0,.*\}$' '^PASS httphello \(graceful shutdown\)'

  # nohandler
  port=$(free_port $((port_base + off + 1)))
  echo "== $arch nohandler (port $port)" >&2
  serve_case "$arch nohandler" "net-$arch-nohandler" "$port" 143 \
    '\{"ev":"exit","code":143,"signal":"SIGTERM".*\}$' '^\[patina\] exit code 143$' -nohandler

  # selftest (CLI, static ip=)
  echo "== $arch selftest" >&2
  log=$out/net-$arch-selftest.log
  sargs=(-selftest)
  [[ $ext == 1 ]] && sargs+=(-dns "$dns_name")
  rc=0
  "$TIMEOUT_BIN" -k 5 "$tmo" "$cli" run --name "net-$arch-selftest" --mem 256 --net user \
    --timeout "$((tmo - 10))s" "$img" -- "${sargs[@]}" >"$log" 2>"$log.err" || rc=$?
  if [[ $rc -ne 0 ]]; then fail "$arch selftest" "patina run exit $rc"
  elif ! has "$log" '^ok dns localhost -> 127\.0\.0\.1'; then fail "$arch selftest" "no localhost lookup"
  elif ! has "$log" '^ok dns patina -> 10\.0\.2\.15$'; then fail "$arch selftest" "no host name lookup"
  elif [[ $ext == 1 ]] && ! has "$log" "^ok dns $dns_name -> [0-9a-f]"; then fail "$arch selftest" "no $dns_name lookup"
  elif ! has "$log" '^PASS httphello \(selftest\)'; then fail "$arch selftest" "no PASS line"
  else pass "$arch selftest$([[ $ext == 1 ]] && echo " (+dns $dns_name)")"
  fi

  # dhcp (run-qemu.sh, slirp DHCP)
  echo "== $arch dhcp" >&2
  log=$out/net-$arch-dhcp.log
  sargs=(-selftest)
  [[ $ext == 1 ]] && sargs+=(-dns "$dns_name")
  rc=0
  PATINA_NET=user PATINA_CTL_SOCK=$state/dhcp-$arch.sock PATINA_LOG=$log \
    "$root/scripts/run-qemu.sh" "$arch" "$cpio" -- ip=dhcp -- "${sargs[@]}" >/dev/null 2>"$log.err" || rc=$?
  if [[ $rc -ne 0 ]]; then fail "$arch dhcp" "run-qemu exit $rc"
  elif ! has "$log" '^\[patina\] net: dhcp lease 10\.0\.2\.15/24 via 10\.0\.2\.2, dns 10\.0\.2\.3 \([0-9]+ ms\)$'; then
    fail "$arch dhcp" "no dhcp lease line"
  elif ! has "$log" '^ok dns patina -> 10\.0\.2\.15$'; then fail "$arch dhcp" "host name does not resolve to the lease"
  elif ! has "$log" '^PASS httphello \(selftest\)'; then fail "$arch dhcp" "no PASS line"
  else pass "$arch dhcp ($(tr -d '\r' <"$log" | sed -n 's/^\[patina\] net: dhcp lease .*(\([0-9]* ms\))$/\1/p'))"
  fi

  # dhcp-nonic
  echo "== $arch dhcp-nonic" >&2
  log=$out/net-$arch-dhcp-nonic.log
  rc=0
  PATINA_CTL_SOCK=$state/nonic-$arch.sock PATINA_LOG=$log \
    "$root/scripts/run-qemu.sh" "$arch" "$cpio" -- ip=dhcp -- -selftest >/dev/null 2>"$log.err" || rc=$?
  if [[ $rc -ne 0 ]]; then fail "$arch dhcp-nonic" "run-qemu exit $rc"
  elif ! has "$log" '^\[patina\] net: ip=dhcp: no network device, continuing without an address$'; then
    fail "$arch dhcp-nonic" "no 'no network device' line"
  elif ! has "$log" '^PASS httphello \(selftest\)'; then fail "$arch dhcp-nonic" "no PASS line"
  else pass "$arch dhcp-nonic"
  fi

  # dhcp-nolease: a NIC whose frames go to the host's UDP discard port.
  echo "== $arch dhcp-nolease" >&2
  log=$out/net-$arch-dhcp-nolease.log
  clog=$out/net-$arch-dhcp-nolease.ctl
  sock=$state/nolease-$arch.sock
  vsuf=-device
  "$PY" -I "$client" "$sock" --connect-timeout 30 --linger 3 --log "$log" \
    'wait@60:"msg":"net: dhcp: no lease after 10 s' 'wait@60:"ev":"exit"' >"$clog" 2>&1 &
  cpid=$!
  bgpids+=("$cpid")
  sleep 0.5
  rc=0
  t0=$(date +%s)
  PATINA_CTL_SOCK=$sock PATINA_LOG=$log \
    PATINA_QEMU_EXTRA="-netdev socket,id=nx,udp=127.0.0.1:9,localaddr=127.0.0.1:0 -device virtio-net$vsuf,netdev=nx" \
    "$root/scripts/run-qemu.sh" "$arch" "$cpio" -- ip=dhcp -- -selftest >/dev/null 2>"$log.err" || rc=$?
  t1=$(date +%s)
  crc=0
  wait "$cpid" || crc=$?
  bgpids=()
  if [[ $rc -ne 0 ]]; then fail "$arch dhcp-nolease" "run-qemu exit $rc"
  elif ! has "$log" '^\[patina\] net: dhcp: no lease after 10 s, continuing without an address$'; then
    fail "$arch dhcp-nolease" "no 'no lease' line"
  elif [[ $crc -ne 0 ]]; then fail "$arch dhcp-nolease" "no ctl log event (client $crc)"
  elif ! has "$log" '^PASS httphello \(selftest\)'; then fail "$arch dhcp-nolease" "no PASS line"
  else pass "$arch dhcp-nolease ($((t1 - t0)) s)"
  fi

  # pci
  echo "== $arch pci" >&2
  log=$out/net-$arch-pci.log
  clog=$out/net-$arch-pci.ctl
  sock=$state/pci-$arch.sock
  port=$(free_port $((port_base + off + 2)))
  if [[ $arch == x86_64 ]]; then penv=(PATINA_MACHINE=q35); else penv=(PATINA_VIRTIO=pci); fi
  rm -f "$log"
  env "${penv[@]}" PATINA_NET="user:$port-8080" PATINA_CTL_SOCK=$sock PATINA_LOG=$log \
    "$root/scripts/run-qemu.sh" "$arch" "$cpio" -- "$static_ip" >/dev/null 2>"$log.err" &
  qpid=$!
  bgpids+=("$qpid")
  if ! wait_log "$log" '^httphello: listening' 90 "$qpid"; then
    fail "$arch pci" "no 'httphello: listening' line"; kill_ours
  elif ! has "$log" '^\[patina\] net: virtio-net hwaddr .* 10\.0\.2\.15/24, gateway 10\.0\.2\.2'; then
    fail "$arch pci" "no net boot line"; kill_ours
  elif ! has "$log" '^\[patina\] virtio-net: .* \(pci [0-9a-f:.]+, hdr 12 B\)'; then
    fail "$arch pci" "virtio-net is not on virtio-pci"; kill_ours
  elif ! err=$(check_http "$port" "$out/net-$arch-pci"); then
    fail "$arch pci" "$err"; kill_ours
  else
    crc=0
    "$PY" -I "$client" "$sock" --connect-timeout 10 --linger 3 \
      'send:{"op":"stop","grace_ms":5000}' 'wait@30:"ev":"exit"' >"$clog" 2>&1 || crc=$?
    rc=0
    wait "$qpid" || rc=$?
    bgpids=()
    if [[ $crc -ne 0 ]]; then fail "$arch pci" "ctlclient status $crc"
    elif ! grep -qE '^<< \{"ev":"exit","code":0,"forced":false\}' "$clog"; then fail "$arch pci" "no exit 0 event"
    elif [[ $rc -ne 0 ]]; then fail "$arch pci" "run-qemu exit $rc"
    elif ! has "$log" '^PASS httphello \(graceful shutdown\)'; then fail "$arch pci" "no graceful shutdown"
    else pass "$arch pci"
    fi
  fi
done

printf '%s\n' "${results[@]}"
exit $failed
