#!/usr/bin/env bash
# Multi-queue virtio-net (decision D88): testapps/httphello behind a
# virtio-net with `mq=on`, fetched from the host. Also the NIC-path
# benchmark (GET /big timings) for before/after comparisons, since
# scripts/bench.sh's tcp rows only use loopback.
#
#   scripts/test-netmq.sh [aarch64] [x86_64] [--cpus "1 2 4"] [--queues N]
#                         [--transport mmio|pci|both] [--backend user|tap]
#                         [--runs R] [--no-build] [-- kernel params...]
#
#   --backend user   (default) `-netdev user` (slirp) with hostfwd. QEMU
#                    offers VIRTIO_NET_F_MQ with max_virtqueue_pairs 1 (a
#                    netdev without `queues=`), so this checks negotiation
#                    and VQ_PAIRS_SET; every run uses one pair.
#   --backend tap    Linux only, as root (e.g. a privileged container):
#                    `-netdev tap,queues=N,vhost=off`, the host end
#                    10.0.2.2/24. QEMU offers N pairs and Linux tun steers
#                    each flow to the queue the guest sent it on. (macOS
#                    QEMU cannot do this: BSD tap aborts with queues > 1,
#                    and Homebrew's build has no vhost-user netdev.)
#   --queues N       pairs the tap backend offers (default 4)
#   --cpus L         vCPU counts (default "1 2 4")
#   --transport T    virtio-mmio (`virtio-net-device`), virtio-pci
#                    (aarch64 PATINA_VIRTIO=pci, x86_64 q35; `vectors=2N+2`)
#                    or both (default)
#   --runs R         GET /big repetitions per configuration (default 5)
#   --no-build       use the kernels in out/ and out/test/netmq/httphello-*.cpio
#                    as they are (no cargo/Go needed, e.g. in a container:
#                    see D88 for the Docker recipe used for --backend tap)
#   -- params        extra kernel params (e.g. patina.net.queues=1)
#
# Per configuration: GET / == "hello from patina", 20 concurrent GETs,
# GET /big (4 MiB) R times (sha256 checked, timed), 4 parallel GET /big
# R times (4 flows; wall time of each batch), then a control-channel
# stop -> exit 0. Checked from the kernel's lines: `virtio-net: ... pairs
# P` with P = min(cpus, offered pairs) (user: 1), and from its
# `patina.stats` net line: with P > 1, more than one receive queue and
# more than one transmit queue carried frames, and with an MSI-X vector per
# pair more than one CPU took NIC interrupts (unless the kernel logged that
# a vector cannot be routed). Prints one row per configuration with the
# median /big time.
#
# PATINA_QEMU_EXTRA is appended to the QEMU arguments (e.g. `-M
# virt,gic-version=2` for the GICv2m path under TCG). Logs in
# out/test/netmq/. Every QEMU started here is killed on exit (matched by
# this run's private state directory).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=()
cpus_list="1 2 4"
queues=4
transports=both
backend=user
runs=5
nobuild=0
kparams=()
while [[ $# -gt 0 ]]; do
  case $1 in
    aarch64|x86_64) arches+=("$1"); shift ;;
    --cpus) cpus_list=${2//,/ }; shift 2 ;;
    --queues) queues=$2; shift 2 ;;
    --transport) transports=$2; shift 2 ;;
    --backend) backend=$2; shift 2 ;;
    --runs) runs=$2; shift 2 ;;
    --no-build) nobuild=1; shift ;;
    --) shift; kparams=("$@"); break ;;
    *) echo "usage: $0 [aarch64] [x86_64] [--cpus L] [--queues N] [--transport mmio|pci|both] [--backend user|tap] [--runs R] [--no-build] [-- params]" >&2; exit 2 ;;
  esac
done
[[ ${#arches[@]} -gt 0 ]] || arches=(aarch64 x86_64)
case $transports in both) transports="mmio pci" ;; mmio|pci) ;; *) echo "bad --transport" >&2; exit 2 ;; esac
case $backend in
  user) ;;
  tap) [[ $(uname -s) == Linux && $(id -u) == 0 ]] || { echo "error: --backend tap needs Linux and root" >&2; exit 2; } ;;
  *) echo "bad --backend" >&2; exit 2 ;;
esac
out=$root/out/test/netmq
mkdir -p "$out"
export PATINA_TIMEOUT=${PATINA_TIMEOUT:-300}
PY=$(command -v python3) || { echo "error: python3 not found" >&2; exit 2; }
command -v curl >/dev/null || { echo "error: curl not found" >&2; exit 2; }
if command -v shasum >/dev/null; then sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else sha256() { sha256sum "$1" | cut -d' ' -f1; }; fi
client=$root/scripts/ctlclient.py
static_ip=ip=10.0.2.15::10.0.2.2:255.255.255.0:patina:eth0:off:10.0.2.3

state=$(mktemp -d /tmp/ptmq.XXXXXX)
bgpids=()
kill_ours() {
  local p
  for p in "${bgpids[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in $(pgrep -f "qemu-system.*$state" 2>/dev/null || true); do kill "$p" 2>/dev/null || true; done
  sleep 0.5
  for p in $(pgrep -f "qemu-system.*$state" 2>/dev/null || true); do kill -9 "$p" 2>/dev/null || true; done
  bgpids=()
}
trap 'kill_ours; rm -rf "$state"' EXIT
trap 'exit 130' INT TERM

free_port() {
  "$PY" -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}
wait_log() { # wait_log <file> <regex> <secs> <pid>
  local f=$1 re=$2 secs=$3 pid=$4 i
  for ((i = 0; i < secs * 10; i++)); do
    [[ -f $f ]] && tr -d "\r" <"$f" | grep -qE -- "$re" && return 0
    kill -0 "$pid" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}
has() { tr -d '\r' <"$1" | grep -qE -- "$2"; }
# nonzero <a/b/c>: how many of the slash-separated counts are > 0
nonzero() { tr '/' '\n' <<<"$1" | awk '$1 > 0 {n++} END {print n + 0}'; }
big_sha=$("$PY" -I -c 'import hashlib;print(hashlib.sha256(bytes(((i*31+(i>>12))&255) for i in range(4<<20))).hexdigest())')

failed=0
results=()
rows=()
fail() {
  results+=("FAIL $1: $2"); failed=1
  echo "---- FAIL $1: $2" >&2
  [[ -n ${log:-} && -f $log ]] && tail -n 25 "$log" >&2
  true
}

for arch in "${arches[@]}"; do
  if [[ $nobuild -eq 0 ]] && ! "$root/scripts/build-kernel.sh" "$arch" >"$out/build-$arch.log" 2>&1; then
    tail -n 30 "$out/build-$arch.log" >&2; exit 1
  fi
  if [[ $nobuild -eq 0 || ! -f $out/httphello-$arch.cpio ]]; then
    "$root/scripts/build-testapp.sh" httphello "$arch" "$out/httphello-$arch" >/dev/null
    "$root/scripts/mkinitrd.sh" "$out/httphello-$arch.cpio" "$out/httphello-$arch" >/dev/null
  fi
  cpio=$out/httphello-$arch.cpio
  for tr in $transports; do
    for ncpu in $cpus_list; do
      label="$arch $tr $backend cpus=$ncpu"
      tag=$arch-$tr-$backend-c$ncpu
      log=$out/$tag.log
      echo "== $label" >&2
      penv=(PATINA_CPUS="$ncpu")
      if [[ $tr == pci ]]; then
        if [[ $arch == x86_64 ]]; then penv+=(PATINA_MACHINE=q35); else penv+=(PATINA_VIRTIO=pci); fi
        dev="virtio-net-pci,mq=on"
      else
        dev="virtio-net-device,mq=on"
      fi
      if [[ $backend == tap ]]; then
        tap=ptmq$$c$ncpu${tr:0:1}
        want_pairs=$((ncpu < queues ? ncpu : queues))
        [[ $tr == pci ]] && dev+=",vectors=$((2 * queues + 2))"
        extra="-netdev tap,id=n0,ifname=$tap,script=no,downscript=no,vhost=off,queues=$queues -device $dev,netdev=n0"
        url=http://10.0.2.15:8080
      else
        want_pairs=1
        hport=$(free_port)
        extra="-netdev user,id=n0,hostfwd=tcp:127.0.0.1:$hport-:8080 -device $dev,netdev=n0"
        url=http://127.0.0.1:$hport
      fi
      extra+=${PATINA_QEMU_EXTRA:+ $PATINA_QEMU_EXTRA}
      rm -f "$log"
      env "${penv[@]}" PATINA_QEMU_EXTRA="$extra" PATINA_CTL_SOCK="$state/ctl-$tag.sock" PATINA_LOG="$log" \
        "$root/scripts/run-qemu.sh" "$arch" "$cpio" -- "$static_ip" patina.stats=1 ${kparams[@]+"${kparams[@]}"} -- \
        >/dev/null 2>"$log.err" &
      qpid=$!
      bgpids+=("$qpid")
      if [[ $backend == tap ]]; then
        for _ in $(seq 100); do ip link show "$tap" >/dev/null 2>&1 && break; sleep 0.1; done
        ip addr add 10.0.2.2/24 dev "$tap" && ip link set "$tap" up || { fail "$label" "cannot configure $tap"; kill_ours; continue; }
      fi
      if ! wait_log "$log" '^httphello: listening' 240 "$qpid"; then
        fail "$label" "no 'httphello: listening' line"; kill_ours; continue
      fi
      err=
      b=$(curl -s -m 30 "$url/") || err="GET / failed"
      [[ -z $err && $b != "hello from patina" ]] && err="GET / body '$b'"
      if [[ -z $err ]]; then
        cps=()
        for i in $(seq 1 20); do curl -s -m 60 -o "$state/c$i" "$url/" & cps+=($!); done
        for p in "${cps[@]}"; do wait "$p" || err="concurrent GET failed"; done
        for i in $(seq 1 20); do [[ $(cat "$state/c$i" 2>/dev/null) == "hello from patina" ]] || err="concurrent GET $i wrong"; done
      fi
      times=()
      if [[ -z $err ]]; then
        for ((r = 0; r < runs; r++)); do
          t=$(curl -s -m 120 -o "$state/big" -w '%{time_total}' "$url/big") || { err="GET /big failed"; break; }
          [[ $(sha256 "$state/big") == "$big_sha" ]] || { err="GET /big sha256 mismatch ($(wc -c <"$state/big") bytes)"; break; }
          times+=("$t")
        done
      fi
      # 4 flows at once: 4 x /big in parallel, wall time of the batch.
      ptimes=()
      if [[ -z $err ]]; then
        for ((r = 0; r < runs; r++)); do
          t0=$("$PY" -I -c 'import time; print(time.time())')
          cps=()
          for i in 1 2 3 4; do curl -s -m 120 -o "$state/pbig$i" "$url/big" & cps+=($!); done
          for p in "${cps[@]}"; do wait "$p" || err="parallel GET /big failed"; done
          t1=$("$PY" -I -c 'import time; print(time.time())')
          for i in 1 2 3 4; do
            [[ $(sha256 "$state/pbig$i") == "$big_sha" ]] || err="parallel GET /big $i sha256 mismatch"
          done
          [[ -n $err ]] && break
          ptimes+=("$(awk -v a="$t0" -v b="$t1" 'BEGIN {print b - a}')")
        done
      fi
      "$PY" -I "$client" "$state/ctl-$tag.sock" --connect-timeout 10 --linger 3 \
        'send:{"op":"stop","grace_ms":5000}' 'wait@30:"ev":"exit"' >"$out/$tag.ctl" 2>&1 || true
      rc=0
      wait "$qpid" || rc=$?
      bgpids=()
      kill_ours
      pairs=$(tr -d '\r' <"$log" | sed -nE 's/^\[patina\] virtio-net: .*, pairs ([0-9]+).*/\1/p' | head -n1)
      stats=$(tr -d '\r' <"$log" | grep -E '^\[patina\] stats net:' | head -n1 || true)
      rxf=$(sed -nE 's/.*rx frames ([0-9/]+),.*/\1/p' <<<"$stats")
      txf=$(sed -nE 's/.*tx frames ([0-9/]+),.*/\1/p' <<<"$stats")
      irqc=$(sed -nE 's/.*irqs per cpu ([0-9/]+),.*/\1/p' <<<"$stats")
      [[ -z $err && $rc -ne 0 ]] && err="run-qemu exit $rc"
      [[ -z $err ]] && ! has "$log" '^PASS httphello \(graceful shutdown\)' && err="no graceful shutdown"
      [[ -z $err && ${pairs:-?} != "$want_pairs" ]] && err="pairs ${pairs:-?}, want $want_pairs"
      [[ -z $err && -z $stats ]] && err="no 'stats net' line"
      if [[ -z $err && $pairs -gt 1 ]]; then
        if [[ $(nonzero "$rxf") -lt 2 || $(nonzero "$txf") -lt 2 ]]; then
          err="pairs $pairs but rx frames $rxf, tx frames $txf"
        elif has "$log" 'msi-x vector per pair' && ! has "$log" 'cannot be routed' && [[ $(nonzero "$irqc") -lt 2 ]]; then
          err="pairs $pairs but NIC interrupts per cpu $irqc"
        fi
      fi
      med=-
      if [[ ${#times[@]} -gt 0 ]]; then
        med=$(printf '%s\n' "${times[@]}" | sort -g | sed -n "$(((${#times[@]} + 1) / 2))p")
        med=$(awk -v t="$med" 'BEGIN {printf "%.1f ms (%.0f MB/s)", t * 1000, 4.194304 / t}')
      fi
      pmed=-
      if [[ ${#ptimes[@]} -gt 0 ]]; then
        pmed=$(printf '%s\n' "${ptimes[@]}" | sort -g | sed -n "$(((${#ptimes[@]} + 1) / 2))p")
        pmed=$(awk -v t="$pmed" 'BEGIN {printf "%.1f ms (%.0f MB/s)", t * 1000, 4 * 4.194304 / t}')
      fi
      rows+=("$(printf '%-26s pairs %-2s rx %-20s tx %-20s irqs/cpu %-16s /big %s, 4x/big %s' "$label" "${pairs:-?}" "${rxf:--}" "${txf:--}" "${irqc:--}" "$med" "$pmed")")
      if [[ -n $err ]]; then fail "$label" "$err"; else results+=("PASS $label"); fi
    done
  done
done

echo
printf '%s\n' "${rows[@]}"
echo
printf '%s\n' "${results[@]}"
exit $failed
