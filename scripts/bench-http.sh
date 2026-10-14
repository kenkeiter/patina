#!/usr/bin/env bash
# HTTP load test: testapps/httphello natively on macOS, under patina on Apple
# Virtualization.framework, and under patina on QEMU/HVF, driven by wrk from
# the same host. Results: doc/bench-http.md (method and observations).
#
#   scripts/bench-http.sh [--quick] [--cpus N[,N...]] [--targets T[,T...]]
#                         [--runs R] [--duration S] [--warmup S]
#
#   --quick     a smoke-sized run: N=1, C in {1,64}, /big at C=1, 1 run of
#               3 s after a 1 s warm-up
#   --cpus      N values: GOMAXPROCS (native) / --cpus (patina) (default 1,2,4)
#   --targets   any of native, vz, qemu (default all three)
#   --runs      repetitions per workload; tables show the median (default 3)
#   --duration  seconds per measured run (default 10)
#   --warmup    seconds of discarded load before each workload (default 3)
#
# Per (N, target) one server is started, checked (GET / body, /info
# GOMAXPROCS == N), loaded with every workload, then stopped and its exit
# checked (native: SIGTERM, patina: `patina ctl <vm> stop`; both must exit 0
# with "PASS httphello (graceful shutdown)").
#
#   native  GOOS=darwin build, GOMAXPROCS=N, http://127.0.0.1:8080 (loopback)
#   vz      patina run --vmm vz --net user --cpus N --mem 512; wrk hits the
#           leased guest address (VZ NAT, a virtio-net device) on :8080
#   qemu    patina run --net user --publish 18080:8080 --cpus N --mem 512;
#           http://127.0.0.1:18080 goes through QEMU's slirp (userspace NAT)
#
# Workloads: GET / at C in {1,16,64} connections, GET /big (4 MiB) at C in
# {1,8}; wrk with keep-alive, min(C,4) threads, --timeout 30s. The load
# generator shares the host's cores with the server/VM; nothing is pinned.
#
# Output: out/bench-http/results.tsv (one row per run), env.txt, logs, and
# out/bench-http/tables.md (also printed). Exits non-zero if a sanity check,
# a run, or a teardown fails. Needs wrk (brew install wrk), curl, python3.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=$root/out/bench-http
cpus_list=(1 2 4)
targets=(native vz qemu)
runs=3 dur=10 warm=3
root_conns=(1 16 64)
big_conns=(1 8)
quick=0
while [[ $# -gt 0 ]]; do
  case $1 in
    --quick) quick=1; shift ;;
    --cpus) IFS=, read -r -a cpus_list <<<"$2"; shift 2 ;;
    --targets) IFS=, read -r -a targets <<<"$2"; shift 2 ;;
    --runs) runs=$2; shift 2 ;;
    --duration) dur=$2; shift 2 ;;
    --warmup) warm=$2; shift 2 ;;
    *) echo "usage: $0 [--quick] [--cpus N,..] [--targets native,vz,qemu] [--runs R] [--duration S] [--warmup S]" >&2; exit 2 ;;
  esac
done
if [[ $quick -eq 1 ]]; then
  cpus_list=(1) runs=1 dur=3 warm=1 root_conns=(1 64) big_conns=(1)
fi
[[ $(uname -s) == Darwin && $(uname -m) == arm64 ]] || { echo "bench-http: needs macOS on Apple Silicon" >&2; exit 2; }
for t in wrk curl python3 go; do
  command -v $t >/dev/null || { echo "bench-http: $t not found${t/#wrk/ (brew install wrk)}" >&2; exit 2; }
done

mkdir -p "$out"
rm -f "$out"/results.tsv "$out"/info.tsv "$out"/*.log "$out"/*.err
state=$(mktemp -d /tmp/ptbh.XXXXXX)
export XDG_RUNTIME_DIR=$state # private `patina ctl` namespace
export PATINA_KERNEL_DIR=$root/out
cli=$root/cli/bin/patina

# --- build -------------------------------------------------------------------
"$root/scripts/build-kernel.sh" aarch64 >"$out/build-kernel.log" 2>&1 || { tail -n 30 "$out/build-kernel.log" >&2; exit 1; }
"$root/cli/scripts/build.sh" >"$out/build-cli.log" 2>&1 || { tail -n 30 "$out/build-cli.log" >&2; exit 1; }
(cd "$root/testapps" && GOOS=darwin GOARCH=arm64 CGO_ENABLED=0 go build -o "$out/httphello-darwin" ./httphello)
(cd "$root/testapps" && "$cli" build ./httphello --target vz-aarch64 -o "$out/httphello-vz.patina" >/dev/null)
(cd "$root/testapps" && "$cli" build ./httphello --target qemu-aarch64 -o "$out/httphello-qemu.patina" >/dev/null)

{
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "macos: $(sw_vers -productVersion) ($(sw_vers -buildVersion))"
  echo "chip: $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cores ($(sysctl -n hw.perflevel0.physicalcpu 2>/dev/null)P+$(sysctl -n hw.perflevel1.physicalcpu 2>/dev/null)E), $(($(sysctl -n hw.memsize) >> 30)) GiB"
  echo "go: $(go version)"
  echo "kernel commit: $(git -C "$root" rev-parse --short HEAD)$(git -C "$root" diff --quiet HEAD -- kernel crates || echo '+dirty')"
  echo "qemu: $(qemu-system-aarch64 --version | head -n 1)"
  echo "load tool: $(wrk -v 2>&1 | head -n 1)"
  echo "runs=$runs duration=${dur}s warmup=${warm}s cpus=${cpus_list[*]} targets=${targets[*]}"
} >"$out/env.txt"
cat "$out/env.txt" >&2

# wrk script: one machine-readable line at the end (latencies in µs).
cat >"$out/report.lua" <<'EOF'
done = function(s, lat, req)
  local e = s.errors
  io.write(string.format("RESULT %d %d %d %d %d %d %d %d\n", s.requests,
    s.bytes, s.duration, e.connect + e.read + e.write + e.status + e.timeout,
    lat:percentile(50), lat:percentile(90), lat:percentile(99), lat.max))
end
EOF

failed=0
srv_pid= vm=
# kill_srv: SIGTERM (patina run: a graceful stop), then SIGKILL; only our
# own process is touched, so other VMs on the host are left alone.
kill_srv() {
  [[ -n $srv_pid ]] || return 0
  kill -TERM "$srv_pid" 2>/dev/null && sleep 2
  pkill -9 -P "$srv_pid" 2>/dev/null
  kill -9 "$srv_pid" 2>/dev/null
  wait "$srv_pid" 2>/dev/null
  srv_pid=
  true
}
trap 'kill_srv; rm -rf "$state"' EXIT
trap 'exit 130' INT TERM

# start <target> <N>: sets $url, $srv_pid, $log, $vm.
start() {
  local t=$1 n=$2 i
  log=$out/$t-n$n.log
  vm=
  case $t in
    native)
      GOMAXPROCS=$n "$out/httphello-darwin" -addr 127.0.0.1:8080 >"$log" 2>"$log.err" &
      srv_pid=$!
      url=http://127.0.0.1:8080 ;;
    vz)
      vm=bh-vz-$n
      "$cli" run "$out/httphello-vz.patina" --vmm vz --net user --cpus "$n" --mem 512 \
        --name "$vm" </dev/null >"$log" 2>"$log.err" &
      srv_pid=$!
      url=
      for ((i = 0; i < 300; i++)); do
        a=$(sed -nE 's/^patina: guest address ([0-9.]+).*/\1/p' "$log.err" | tail -n 1)
        [[ -n $a ]] && { url=http://$a:8080; break; }
        kill -0 "$srv_pid" 2>/dev/null || break
        sleep 0.1
      done ;;
    qemu)
      vm=bh-qemu-$n
      "$cli" run "$out/httphello-qemu.patina" --net user --publish 18080:8080 --cpus "$n" --mem 512 \
        --name "$vm" </dev/null >"$log" 2>"$log.err" &
      srv_pid=$!
      url=http://127.0.0.1:18080 ;;
  esac
  [[ -n $url ]] || return 1
  for ((i = 0; i < 300; i++)); do
    [[ $(curl -s -m 2 "$url/" 2>/dev/null) == "hello from patina" ]] && return 0
    kill -0 "$srv_pid" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}

# stop <target>: graceful stop, checks exit 0 and the shutdown line.
stop() {
  local t=$1 rc=0
  if [[ $t == native ]]; then
    kill -TERM "$srv_pid"
  else
    "$cli" ctl "$vm" stop --timeout 30s >>"$log.err" 2>&1 || true
  fi
  wait "$srv_pid" || rc=$?
  srv_pid=
  if [[ $rc -ne 0 ]] || ! tr -d '\r' <"$log" | grep -q 'PASS httphello (graceful shutdown)'; then
    echo "bench-http: $t: stop: exit $rc, log $log" >&2; tail -n 5 "$log" "$log.err" >&2; failed=1
  else
    echo "bench-http: $t: stopped, exit 0 (graceful shutdown)" >&2
  fi
}

# load <target> <N> <path> <C>: warm-up, then $runs measured runs.
load() {
  local t=$1 n=$2 path=$3 c=$4 th r line
  th=$((c < 4 ? c : 4))
  wrk -t"$th" -c"$c" -d"${warm}s" --timeout 30s "$url$path" >/dev/null 2>&1 || true
  for ((r = 1; r <= runs; r++)); do
    line=$(wrk -t"$th" -c"$c" -d"${dur}s" --timeout 30s -s "$out/report.lua" "$url$path" 2>&1 \
      | tee -a "$out/wrk.log" | sed -n 's/^RESULT //p')
    if [[ -z $line ]]; then
      echo "bench-http: $t N=$n $path C=$c run $r: no result" >&2; failed=1; continue
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$t" "$n" "$path" "$c" "$r" "$line" | tr ' ' '\t' >>"$out/results.tsv"
    echo "bench-http: $t N=$n GET $path C=$c run $r/$runs: $line" >&2
    [[ ${line%% *} != 0 ]] || echo "bench-http: $t N=$n GET $path C=$c run $r: no response completed (counted as 0 req/s)" >&2
  done
}

for n in "${cpus_list[@]}"; do
  for t in "${targets[@]}"; do
    echo "== $t N=$n" >&2
    if ! start "$t" "$n"; then
      echo "bench-http: $t N=$n: server not reachable (log $log)" >&2
      tail -n 10 "$log" "$log.err" >&2; failed=1; kill_srv; continue
    fi
    info=$(curl -s -m 5 "$url/info")
    echo "bench-http: $t N=$n: $url /info: $info" >&2
    printf '%s\tN=%s\t%s\t%s\n' "$t" "$n" "$url" "$info" >>"$out/info.tsv"
    if [[ $info != "GOMAXPROCS=$n "* ]]; then
      echo "bench-http: $t N=$n: GOMAXPROCS mismatch" >&2; failed=1
    fi
    for c in "${root_conns[@]}"; do load "$t" "$n" / "$c"; done
    for c in "${big_conns[@]}"; do load "$t" "$n" /big "$c"; done
    stop "$t"
  done
done

# --- tables ------------------------------------------------------------------
python3 -I - "$out/results.tsv" "$runs" >"$out/tables.md" <<'EOF'
import sys, statistics as st
rows = [l.rstrip("\n").split("\t") for l in open(sys.argv[1]) if l.strip()]
runs = sys.argv[2]
label = {"native": "native macOS (loopback)", "vz": "patina VZ (NAT, guest IP)",
         "qemu": "patina QEMU/HVF (slirp, --publish)"}
order = [t for t in ("native", "vz", "qemu") if any(r[0] == t for r in rows)]
ns = sorted({int(r[1]) for r in rows})
def cells(t, n, path, c):
    return [r for r in rows if r[0] == t and int(r[1]) == n and r[2] == path and int(r[3]) == c]
def table(title, path, conns, fmt):
    print(f"### {title}\n")
    print("| target | N | " + " | ".join(f"C={c}" for c in conns) + " |")
    print("|---|---|" + "---|" * len(conns))
    for t in order:
        for n in ns:
            out = []
            for c in conns:
                rs = cells(t, n, path, c)
                out.append(fmt(rs) if rs else "-")
            print(f"| {label[t]} | {n} | " + " | ".join(out) + " |")
    print()
def rps(r): return int(r[5]) / (int(r[7]) / 1e6)
def mbps(r): return int(r[6]) / (int(r[7]) / 1e6) / 1e6
def med_range(f, nd):
    def g(rs):
        v = [f(r) for r in rs]
        s = f"{st.median(v):.{nd}f}"
        return s if len(v) == 1 else f"{s} ({min(v):.{nd}f}–{max(v):.{nd}f})"
    return g
def lat(i):
    return lambda rs: f"{st.median(int(r[i]) for r in rs) / 1000:.2f}"
def errs(rs):
    e = sum(int(r[8]) for r in rs)
    z = sum(1 for r in rs if int(r[5]) == 0)
    return f"{e}" + (f", {z} run(s) with 0 responses" if z else "")
rc = sorted({int(r[3]) for r in rows if r[2] == "/"})
bc = sorted({int(r[3]) for r in rows if r[2] == "/big"})
print(f"Median of {runs} run(s); (min–max) in parentheses.\n")
table("GET / — requests/s", "/", rc, med_range(rps, 0))
table("GET / — p50 latency (ms)", "/", rc, lat(9))
table("GET / — p99 latency (ms)", "/", rc, lat(11))
table("GET /big (4 MiB) — MB/s (10^6 B/s)", "/big", bc, med_range(mbps, 0))
table("GET /big (4 MiB) — p50 latency per response (ms)", "/big", bc, lat(9))
table("Errors (wrk connect/read/write/status/timeout, summed over runs)", "/", rc, errs)
table("Errors /big", "/big", bc, errs)
EOF
cat "$out/tables.md"
exit $failed
