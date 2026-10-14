# HTTP load test: native macOS vs patina on VZ vs patina on QEMU/HVF

The same Go server (`testapps/httphello`) run three ways on one Apple
Silicon Mac, loaded with `wrk` from the same host. Script:
`scripts/bench-http.sh`. Run 2026-10-14.

## Method

- **Server**: `testapps/httphello` (`GET /` → `hello from patina\n`, 18 B
  body; `GET /big` → a 4 MiB body from memory; `GET /info` →
  `GOMAXPROCS=… NumCPU=… GOOS=… GOARCH=…`, added for this check). One
  source for all three targets, Go 1.27.1.
- **Targets**, each with N ∈ {1, 2, 4}:

  | target | build | run | URL wrk hits | network path |
  |---|---|---|---|---|
  | native | `GOOS=darwin go build` | `GOMAXPROCS=N httphello -addr 127.0.0.1:8080` | `127.0.0.1:8080` | macOS TCP over `lo0` (no NIC) |
  | vz | `patina build --target vz-aarch64` | `patina run --vmm vz --net user --cpus N --mem 512` | the leased guest address (`patina: guest address A`), `:8080` | macOS TCP → VZ NAT (vmnet bridge) → virtio-net (INTx) → patina's smoltcp |
  | qemu | `patina build --target qemu-aarch64` | `patina run --net user --publish 18080:8080 --cpus N --mem 512` (`virt` + HVF) | `127.0.0.1:18080` | macOS TCP over `lo0` → QEMU slirp (userspace NAT, `hostfwd`) → virtio-mmio virtio-net → smoltcp |

  The three network paths are different; none is a like-for-like
  comparison with another. Native loopback has no device at all; QEMU's
  path is a userspace TCP proxy (slirp terminates the host connection and
  opens its own to the guest).
- **Sanity, per (target, N)**: `GET /` returns `hello from patina`, and
  `/info` reports `GOMAXPROCS=N` on all three (NumCPU is 10 natively and N
  under patina). After the workloads the server is stopped — native with
  SIGTERM, patina with `patina ctl <vm> stop` — and must exit 0 with
  `PASS httphello (graceful shutdown)`. All 9 servers passed both checks.
- **Load tool**: wrk 4.2.0 (kqueue, Homebrew). Keep-alive connections,
  `-t min(C,4) -c C --timeout 30s`, a Lua `done` hook that prints request
  and byte counts, error counts and the p50/p90/p99 latency.
- **Workloads**: `GET /` at C ∈ {1, 16, 64}; `GET /big` at C ∈ {1, 8}.
  Each workload: 3 s warm-up (discarded), then 3 runs of 10 s against the
  same server instance. Tables show the median of the 3 runs; for
  throughput the min–max is in parentheses. MB/s = 10^6 B/s of response
  bytes (headers included, negligible).
- **Nothing is pinned.** wrk's threads, the server or the VM's vCPU threads,
  VZ's NAT and QEMU's main loop all share the 10 host cores; at N=4 with 4
  wrk threads, 8+ host threads are busy. The host was otherwise idle
  (no other VMs, load average ~3 before the run).

## Environment

| | |
|---|---|
| Host | Apple M1 Max, 10 cores (8P + 2E), 64 GiB |
| macOS | 26.6.2 (25G83) |
| Go | go1.27.1 darwin/arm64 (server built with it for all targets) |
| patina | kernel and CLI at `fe0eb31` (release kernel, `scripts/build-kernel.sh aarch64`; CLI ad-hoc signed by `cli/scripts/build.sh`) |
| QEMU | 11.1.2, `-M virt,gic-version=3 -accel hvf -cpu host`, `-netdev user,hostfwd=… -device virtio-net-device` |
| Load tool | wrk 4.2.0 [kqueue] |

## Results

### GET / — requests/s

| target | N | C=1 | C=16 | C=64 |
|---|---|---|---|---|
| native macOS (loopback) | 1 | 34439 (33943–34890) | 81238 (81029–81541) | 82076 (81759–82648) |
| native macOS (loopback) | 2 | 30445 (30207–31512) | 124930 (124651–125770) | 136988 (135508–137888) |
| native macOS (loopback) | 4 | 30705 (30704–30880) | 131948 (131840–134236) | 148836 (148714–150698) |
| patina VZ (NAT, guest IP) | 1 | 13322 (13286–13646) | 25670 (25173–25737) | 20734 (19715–20747) |
| patina VZ (NAT, guest IP) | 2 | 11711 (11673–11839) | 46756 (46543–46941) | 35965 (35805–36066) |
| patina VZ (NAT, guest IP) | 4 | 9560 (9534–9607) | 52004 (51712–52281) | 42603 (39498–47363) |
| patina QEMU/HVF (slirp, --publish) | 1 | 10788 (10666–11295) | 15927 (15554–15941) | 14662 (14617–14974) |
| patina QEMU/HVF (slirp, --publish) | 2 | 9449 (9008–9543) | 17601 (17063–17719) | 17257 (17034–17458) |
| patina QEMU/HVF (slirp, --publish) | 4 | 8030 (7969–8076) | 17942 (17471–18003) | 16711 (16015–16737) |

### GET / — p50 latency (ms)

| target | N | C=1 | C=16 | C=64 |
|---|---|---|---|---|
| native macOS (loopback) | 1 | 0.03 | 0.19 | 0.77 |
| native macOS (loopback) | 2 | 0.03 | 0.11 | 0.45 |
| native macOS (loopback) | 4 | 0.03 | 0.11 | 0.40 |
| patina VZ (NAT, guest IP) | 1 | 0.07 | 0.62 | 3.10 |
| patina VZ (NAT, guest IP) | 2 | 0.08 | 0.32 | 1.70 |
| patina VZ (NAT, guest IP) | 4 | 0.10 | 0.29 | 1.38 |
| patina QEMU/HVF (slirp, --publish) | 1 | 0.09 | 0.97 | 1.62 |
| patina QEMU/HVF (slirp, --publish) | 2 | 0.10 | 0.85 | 1.08 |
| patina QEMU/HVF (slirp, --publish) | 4 | 0.12 | 0.75 | 1.12 |

### GET / — p99 latency (ms)

| target | N | C=1 | C=16 | C=64 |
|---|---|---|---|---|
| native macOS (loopback) | 1 | 0.07 | 0.40 | 1.26 |
| native macOS (loopback) | 2 | 0.07 | 0.30 | 0.99 |
| native macOS (loopback) | 4 | 0.06 | 0.27 | 0.71 |
| patina VZ (NAT, guest IP) | 1 | 0.14 | 1.27 | 6.17 |
| patina VZ (NAT, guest IP) | 2 | 0.14 | 0.95 | 6.55 |
| patina VZ (NAT, guest IP) | 4 | 0.18 | 0.68 | 11.97 |
| patina QEMU/HVF (slirp, --publish) | 1 | 0.18 | 2.06 | 5.49 |
| patina QEMU/HVF (slirp, --publish) | 2 | 0.20 | 3.01 | 6.30 |
| patina QEMU/HVF (slirp, --publish) | 4 | 0.20 | 3.49 | 8.95 |

One VZ N=1 C=64 run had a p99 of 49.3 ms (the other two 6.1–6.2 ms); the
median hides it.

### GET /big (4 MiB) — MB/s

| target | N | C=1 | C=8 |
|---|---|---|---|
| native macOS (loopback) | 1 | 6145 (5726–6338) | 7385 (7340–7423) |
| native macOS (loopback) | 2 | 5843 (5754–5984) | 7558 (7460–7663) |
| native macOS (loopback) | 4 | 5661 (5211–5860) | 7944 (7818–8082) |
| patina VZ (NAT, guest IP) | 1 | 361 (360–368) | 360 (355–364) |
| patina VZ (NAT, guest IP) | 2 | 350 (343–356) | 394 (394–399) |
| patina VZ (NAT, guest IP) | 4 | 349 (348–355) | 429 (421–430) |
| patina QEMU/HVF (slirp, --publish) | 1 | 75 (74–75) | 65 (61–68) |
| patina QEMU/HVF (slirp, --publish) | 2 | 69 (69–71) | 66 (65–66) |
| patina QEMU/HVF (slirp, --publish) | 4 | 73 (72–73) | 54 (51–58) |

p50 time per 4 MiB response at C=1: native 0.59–0.74 ms, VZ 11.4–11.8 ms,
QEMU 55–60 ms. At C=8: native 3.3 ms, VZ 69–91 ms, QEMU 452–541 ms.

### Errors

wrk reported no errors for native and VZ in any run. QEMU at `GET /` C=64
reported 103–125 socket **read** errors per (N, 3 runs) — about 40 per 10 s
run, i.e. connections closed or reset under the client, which wrk then
reopens; none at C=1/16 or on `/big`. A follow-up on QEMU N=1 C=64 saw 0
errors in runs of 1, 1 and 5 s and 33 in a 10 s run, and none at C=16/32/48
for 3 s. Where the connections are closed (slirp or the guest) was not
measured.

In a `--quick` trial run before the full run, one VZ N=1 `GET /` C=1 run
completed 0 requests in 3 s with 0 errors (one connection got no response
for > 3 s). It did not recur: 2 more `--quick` VZ runs, 30 consecutive 1 s
C=1 runs on one VM, 5 fresh VMs, and the 9 full-run C=1 runs on VZ were all
normal. Cause not determined. The script prints a `no response completed`
line and counts such runs in the error table if it happens again.

## Observations

Measured ratios (patina ÷ native, same N, median req/s):

| | C=1 | C=16 | C=64 |
|---|---|---|---|
| VZ, N=1 / 2 / 4 | 0.39 / 0.38 / 0.31 | 0.32 / 0.37 / 0.39 | 0.25 / 0.26 / 0.29 |
| QEMU, N=1 / 2 / 4 | 0.31 / 0.31 / 0.26 | 0.20 / 0.14 / 0.14 | 0.18 / 0.13 / 0.11 |

1. **Native is fastest everywhere, but its path is not a NIC path.**
   Native requests go over macOS loopback (`lo0`, no device, large MTU,
   no virtualization exits); VZ goes through a virtio-net device and macOS's
   NAT bridge; QEMU goes through slirp. The native column is an upper bound
   for the Go server on this host, not a like-for-like baseline. (Inferred:
   the native /big numbers, 5.7–7.9 GB/s, are essentially loopback
   memcpy.)

2. **Single-connection round trip (C=1).** Per request: native ~29 µs,
   VZ ~75 µs, QEMU ~93 µs (1 / req/s at N=1). VZ adds ~46 µs per
   request/response pair. Inferred contributors: a VM exit per virtio
   notify, an INTx interrupt (VZ has no MSI-X; one level line for all
   queues, routed to CPU 0 — D88) waking the net task, smoltcp under one
   lock (D67), and the NAT bridge hop on the host. QEMU adds slirp's
   userspace proxy on top of its own virtio-mmio exits.

3. **More vCPUs make C=1 slower on every target** (native 34.4k → 30.7k,
   VZ 13.3k → 9.6k, QEMU 10.8k → 8.0k req/s from N=1 to N=4). Natively that
   is Go's scheduler handing work between Ps. Under patina the drop is
   larger (−28% VZ, −26% QEMU vs −11% native); inferred: cross-CPU wakeups
   (Reschedule IPIs to CPUs idling in WFI; D79 measured them doubling
   `signal-rtt` at 2 vCPUs) between the CPU taking the NIC interrupt / net
   task and the CPU running the goroutine.

4. **VZ scales with vCPUs at C=16 but loses throughput from C=16 to C=64.**
   C=16: 25.7k → 46.8k → 52.0k req/s (1.8× at N=2, 2.0× at N=4). C=64 is
   below C=16 at every N (20.7k / 36.0k / 42.6k), and its p99 grows to
   12 ms at N=4. Native gains from C=16 to C=64 at every N. Inferred: all
   packet processing is serialized in one smoltcp `Interface` behind one
   lock with one net task (D67), and after each poll the stack recomputes
   readiness for every socket, so per-packet work grows with the number of
   connections; the VZ NIC's single INTx line delivers every interrupt to
   CPU 0. Neither cost was profiled here. The p50 at C=64 matches
   Little's law (64 / req/s: 3.1 / 1.8 / 1.5 ms vs measured
   3.1 / 1.7 / 1.4 ms), i.e. the latency is queueing behind the throughput
   limit, not a timeout.

5. **QEMU is flat at ~15–18k req/s regardless of N** at C ≥ 16 (17.9k at
   N=4 is 1.13× N=1), and /big at C=8 is no faster than C=1. Inferred: the
   bottleneck is host-side — slirp runs in QEMU's single main-loop thread
   and proxies every connection through a host socket — so guest vCPUs do
   not help. It also showed the only wrk errors (read errors at C=64; see
   above).

6. **Bulk transfer (/big).** VZ moves ~350–360 MB/s on one connection
   (11.4 ms per 4 MiB) and does not gain from 8 connections at N=1
   (360 MB/s), gaining modestly with vCPUs at C=8 (394 / 429 MB/s at
   N=2 / 4). Inferred: 1500-byte frames with no offloads (the driver
   negotiates no TSO/checksum offload — D88 "offloads stay off" — so every
   ~1.4 KiB segment is checksummed in software and costs a descriptor), a
   64 KiB TCP send buffer per socket, and every segment passing through
   the one locked stack and net task. QEMU's slirp path moves 54–75 MB/s,
   ~5× less than VZ.

7. **Load generator on the same host.** wrk used up to 4 threads on the
   same 10 cores; VZ's NAT and QEMU's main loop/vCPU threads also use host
   CPU. Native numbers at high C may be partly limited by wrk itself
   (not measured).

## Re-running

```
scripts/bench-http.sh            # full run: N=1,2,4 × 3 targets, ~30 min
scripts/bench-http.sh --quick    # N=1, C=1/64, /big C=1, 1 × 3 s run each
scripts/bench-http.sh --cpus 2 --targets vz,qemu --runs 5
```

Needs macOS on Apple Silicon, `wrk` (`brew install wrk`), Go, QEMU,
`curl`, `python3`. The script builds the kernel, the signed CLI and the
three server binaries itself, uses a private `XDG_RUNTIME_DIR` for
`patina ctl` names, and only kills processes it started. Output:
`out/bench-http/tables.md` (the tables above), `results.tsv` (one row per
run: target, N, path, C, run, requests, bytes, duration µs, errors,
p50/p90/p99/max µs), `info.tsv`, `env.txt`, `wrk.log` and per-server logs.
Exit status is non-zero if a sanity check, a run or a teardown failed.
