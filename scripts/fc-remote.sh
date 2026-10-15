#!/usr/bin/env bash
# Firecracker x86_64 smoke test on a remote Linux/KVM host.
#
#   PATINA_BENCH_HOST=user@host scripts/fc-remote.sh [cpus...]   (default: 1 2 4)
#
# On this machine: builds out/kernel-x86_64-pvh.elf, the CLI for linux/amd64
# (out/patina-linux-amd64) and testapps hello + httphello as
# firecracker-x86_64 images, then copies them to ~/patina-bench/ on the host.
# On the host (needs firecracker in PATH, /dev/kvm access, passwordless sudo):
#   - creates tap0 (owned by the ssh user) with 172.20.0.1/24 and a MASQUERADE
#     rule for 172.20.0.0/24, idempotently (both are left in place);
#   - per vCPU count: `patina run hello --vmm firecracker --net tap0 --ip
#     172.20.0.2/24,...` must exit 0 with the ready and exit events,
#     `ready` must report the vCPU count and the program print "PASS hello";
#     httphello: curl GET / == "hello from patina", /info NumCPU == cpus,
#     /big is 4194304 bytes with the right sha256, then `patina ctl <vm> stop`
#     -> exit 0 and "PASS httphello (graceful shutdown)".
#   - once (first vCPU count): httphello -selftest -dns example.com, a UDP
#     lookup through 1.1.1.1 over the tap + NAT (needs internet on the host;
#     PATINA_FC_DNS=0 skips it).
# Every remote command runs under timeout(1) and patina run gets --timeout,
# so a hung guest cannot block. Firecracker processes started by this run are
# killed at the end. Logs: ~/patina-bench/log/ on the host.
set -euo pipefail

host=${PATINA_BENCH_HOST:?set PATINA_BENCH_HOST=user@host}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cpus=("$@")
[[ ${#cpus[@]} -gt 0 ]] || cpus=(1 2 4)

"$root/scripts/build-kernel.sh" x86_64 >/dev/null
(cd "$root/cli" && GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o "$root/out/patina-linux-amd64" .)
(cd "$root/cli" && go build -o "$root/out/patina-host" .)
for app in hello httphello; do
  (cd "$root/testapps" && PATINA_KERNEL_DIR=$root/out "$root/out/patina-host" build "./$app" \
    --target firecracker-x86_64 -o "$root/out/$app-fc.patina")
done
ssh -o BatchMode=yes "$host" 'mkdir -p ~/patina-bench/log'
scp -q "$root/out/patina-linux-amd64" "$root/out/hello-fc.patina" "$root/out/httphello-fc.patina" "$host:patina-bench/"

# shellcheck disable=SC2087 # expand nothing locally except via args
ssh -o BatchMode=yes "$host" DNS="${PATINA_FC_DNS:-1}" bash -s -- "${cpus[@]}" <<'REMOTE'
set -uo pipefail
cd ~/patina-bench
chmod +x patina-linux-amd64
P=$PWD/patina-linux-amd64
GUEST=172.20.0.2
IP=$GUEST/24,172.20.0.1,1.1.1.1
export XDG_RUNTIME_DIR=$PWD/run
mkdir -p "$XDG_RUNTIME_DIR" log

ip link show tap0 >/dev/null 2>&1 || sudo ip tuntap add tap0 mode tap user "$(id -un)"
ip -4 addr show tap0 | grep -q 172.20.0.1/24 || sudo ip addr add 172.20.0.1/24 dev tap0
sudo ip link set tap0 up
sudo iptables -t nat -C POSTROUTING -s 172.20.0.0/24 -j MASQUERADE 2>/dev/null \
  || sudo iptables -t nat -A POSTROUTING -s 172.20.0.0/24 -j MASQUERADE

big_sha=$(python3 -I -c 'import hashlib;print(hashlib.sha256(bytes(((i*31+(i>>12))&255) for i in range(4<<20))).hexdigest())')
failed=0 results=()
pass() { results+=("PASS $1"); }
fail() { results+=("FAIL $1: $2"); failed=1; }
cleanup() { pkill -f "firecracker --api-sock $XDG_RUNTIME_DIR/" 2>/dev/null || true; }
trap cleanup EXIT

for n in "$@"; do
  log=log/hello-$n.log
  timeout 90 "$P" run hello-fc.patina --vmm firecracker --net tap0 --ip "$IP" \
    --cpus "$n" --mem 256 --timeout 60s -v >"$log" 2>&1
  rc=$?
  if [[ $rc != 0 ]]; then fail "hello cpus=$n" "exit $rc (see ~/patina-bench/$log)"
  elif ! grep -q "control: ready: go[0-9.]*, $n cpus" "$log" || ! grep -q 'control: exit code 0' "$log"; then
    fail "hello cpus=$n" "missing ready ($n cpus) / exit events"
  elif ! grep -q '^PASS hello (exit 0)' "$log"; then fail "hello cpus=$n" "no PASS line"
  else pass "hello cpus=$n"; fi

  log=log/httphello-$n.log vm=fc-http-$n
  timeout 150 "$P" run httphello-fc.patina --name "$vm" --vmm firecracker --net tap0 --ip "$IP" \
    --cpus "$n" --mem 256 --timeout 120s -v >"$log" 2>&1 &
  rpid=$!
  for _ in $(seq 1 300); do grep -q '^httphello: listening' "$log" && break; kill -0 $rpid 2>/dev/null || break; sleep 0.2; done
  err=
  b=$(curl -s -m 20 "http://$GUEST:8080/") || err="GET / failed"
  [[ -n $err || $b == "hello from patina" ]] || err="GET / body '$b'"
  if [[ -z $err ]]; then
    info=$(curl -s -m 20 "http://$GUEST:8080/info")
    [[ $info == *"NumCPU=$n "* ]] || err="/info '$info'"
  fi
  if [[ -z $err ]]; then
    curl -s -m 60 -o log/big "http://$GUEST:8080/big" || err="GET /big failed"
    sz=$(stat -c %s log/big 2>/dev/null || echo 0)
    [[ -n $err || ( $sz == 4194304 && $(sha256sum log/big | cut -d' ' -f1) == "$big_sha" ) ]] || err="/big $sz bytes or bad sha256"
  fi
  ctlout=$(timeout 30 "$P" ctl "$vm" stop 2>&1); crc=$?
  wait $rpid; rc=$?
  if [[ -n $err ]]; then fail "httphello cpus=$n" "$err"
  elif [[ $crc != 0 || $ctlout != *'"ev":"exit","code":0'* ]]; then fail "httphello cpus=$n" "ctl stop rc $crc: $ctlout"
  elif [[ $rc != 0 ]]; then fail "httphello cpus=$n" "patina run exit $rc"
  elif ! grep -q '^PASS httphello (graceful shutdown)' "$log"; then fail "httphello cpus=$n" "no graceful shutdown line"
  else pass "httphello cpus=$n (/, /info, /big, ctl stop)"; fi
done
if [[ $DNS != 0 ]]; then
  log=log/dns.log
  timeout 90 "$P" run httphello-fc.patina --vmm firecracker --net tap0 --ip "$IP" --cpus "$1" \
    --mem 256 --timeout 60s -- -selftest -dns example.com >"$log" 2>&1
  rc=$?
  if [[ $rc == 0 ]] && grep -q '^ok dns example.com ->' "$log"; then pass "dns via NAT (selftest -dns example.com)"
  else fail "dns via NAT" "exit $rc: $(grep -a 'dns example.com\|FAIL' "$log" | head -1)"; fi
fi
printf '%s\n' "${results[@]}"
exit $failed
REMOTE
