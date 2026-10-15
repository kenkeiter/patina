# HTTP load test: native macOS vs patina on VZ vs patina on QEMU/HVF

The same Go server (`testapps/httphello`) run three ways on one Apple
Silicon Mac, loaded with `wrk` from the same host. Script:
`scripts/bench-http.sh`. Run 2026-10-14 on kernel `fe0eb31` — the
"before" measurements that motivated decision D89 (the network stack's
scaling work). The re-run on the D89 kernel is in [After D89](#after-d89)
below; the sections up to "Observations" describe the before run.

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

## After D89

The same script, same host and tools, re-run 2026-10-15 on kernel
`c9bc3cf` (decision D89: one smoltcp socket set per socket with an exact
ingress demux, deferred virtio doorbells with `EVENT_IDX` and a 128-slot
transmit ring, per-socket locks with a short interface lock). The host
was otherwise idle (no other VMs). `scripts/bench-http.sh --profile`
(one fresh VM per workload with `patina.stats=1`) produced the profile
that drove the design; its findings are in D89. Native is unchanged
within noise (it is the same binary on the same macOS), so the tables
show all three targets again.

### GET / — requests/s

| target | N | C=1 | C=16 | C=64 |
|---|---|---|---|---|
| native macOS (loopback) | 1 | 33851 (33329–33869) | 79285 (79160–80291) | 79573 (79236–79876) |
| native macOS (loopback) | 2 | 31522 (30831–31714) | 125393 (124510–125455) | 137677 (137617–138062) |
| native macOS (loopback) | 4 | 30791 (30492–31030) | 131627 (130603–131865) | 148382 (147847–148851) |
| patina VZ (NAT, guest IP) | 1 | 14156 (13988–15109) | 43230 (43113–43443) | 42534 (42424–42585) |
| patina VZ (NAT, guest IP) | 2 | 12338 (12066–12445) | 69696 (68859–70060) | 117565 (117328–118585) |
| patina VZ (NAT, guest IP) | 4 | 9128 (9059–9583) | 65251 (63740–65365) | 116315 (109221–121299) |
| patina QEMU/HVF (slirp, --publish) | 1 | 11062 (11048–11432) | 36489 (36313–36493) | 36617 (36347–36664) |
| patina QEMU/HVF (slirp, --publish) | 2 | 10388 (10290–10490) | 55239 (54548–55845) | 65419 (62926–67406) |
| patina QEMU/HVF (slirp, --publish) | 4 | 7772 (7645–8047) | 53045 (52156–53218) | 63322 (59896–64006) |

### GET / — p50 latency (ms)

| target | N | C=1 | C=16 | C=64 |
|---|---|---|---|---|
| native macOS (loopback) | 1 | 0.03 | 0.19 | 0.79 |
| native macOS (loopback) | 2 | 0.03 | 0.11 | 0.45 |
| native macOS (loopback) | 4 | 0.03 | 0.11 | 0.40 |
| patina VZ (NAT, guest IP) | 1 | 0.07 | 0.35 | 1.51 |
| patina VZ (NAT, guest IP) | 2 | 0.07 | 0.21 | 0.49 |
| patina VZ (NAT, guest IP) | 4 | 0.10 | 0.23 | 0.51 |
| patina QEMU/HVF (slirp, --publish) | 1 | 0.09 | 0.40 | 0.73 |
| patina QEMU/HVF (slirp, --publish) | 2 | 0.09 | 0.25 | 0.34 |
| patina QEMU/HVF (slirp, --publish) | 4 | 0.12 | 0.26 | 0.37 |

### GET / — p99 latency (ms)

| target | N | C=1 | C=16 | C=64 |
|---|---|---|---|---|
| native macOS (loopback) | 1 | 0.08 | 0.46 | 1.42 |
| native macOS (loopback) | 2 | 0.06 | 0.29 | 0.94 |
| native macOS (loopback) | 4 | 0.06 | 0.24 | 0.70 |
| patina VZ (NAT, guest IP) | 1 | 0.15 | 0.79 | 3.09 |
| patina VZ (NAT, guest IP) | 2 | 0.14 | 0.58 | 1.75 |
| patina VZ (NAT, guest IP) | 4 | 0.66 | 0.49 | 1.27 |
| patina QEMU/HVF (slirp, --publish) | 1 | 0.17 | 0.86 | 2.16 |
| patina QEMU/HVF (slirp, --publish) | 2 | 0.18 | 0.64 | 1.01 |
| patina QEMU/HVF (slirp, --publish) | 4 | 0.28 | 0.58 | 0.88 |

VZ N=4 C=1: two of the three runs had a p99 of 0.66 and 0.77 ms, the
third 0.19 ms (p50 0.10 ms in all three); see observation 5 below.

### GET /big (4 MiB) — MB/s

| target | N | C=1 | C=8 |
|---|---|---|---|
| native macOS (loopback) | 1 | 5420 (5315–5951) | 7740 (7655–7935) |
| native macOS (loopback) | 2 | 6192 (6002–6258) | 7667 (7621–7942) |
| native macOS (loopback) | 4 | 5469 (5062–5485) | 7819 (7487–8228) |
| patina VZ (NAT, guest IP) | 1 | 403 (395–407) | 583 (580–592) |
| patina VZ (NAT, guest IP) | 2 | 426 (396–427) | 849 (847–852) |
| patina VZ (NAT, guest IP) | 4 | 407 (401–417) | 917 (895–930) |
| patina QEMU/HVF (slirp, --publish) | 1 | 257 (256–261) | 261 (258–263) |
| patina QEMU/HVF (slirp, --publish) | 2 | 234 (234–236) | 252 (248–254) |
| patina QEMU/HVF (slirp, --publish) | 4 | 217 (201–224) | 231 (225–233) |

p50 per 4 MiB response at C=1: VZ 9.8–10.3 ms, QEMU 15.8–18.9 ms; at
C=8: VZ 56 / 39 / 36 ms, QEMU 126–144 ms.

### Errors

None for native and VZ in any run. QEMU `GET /` C=64: 110–120 socket read
errors per (N, 3 runs), the same ~40 per 10 s run as before (slirp side,
unchanged); one at N=1 C=16; none on `/big`.

### Before → after

Median req/s (p99 ms), `GET /`:

| target | N | C=1 | C=16 | C=64 |
|---|---|---|---|---|
| VZ | 1 | 13.3k (0.14) → 14.2k (0.15) | 25.7k (1.27) → 43.2k (0.79) | 20.7k (6.17) → 42.5k (3.09) |
| VZ | 2 | 11.7k (0.14) → 12.3k (0.14) | 46.8k (0.95) → 69.7k (0.58) | 36.0k (6.55) → 117.6k (1.75) |
| VZ | 4 | 9.6k (0.18) → 9.1k (0.66) | 52.0k (0.68) → 65.3k (0.49) | 42.6k (11.97) → 116.3k (1.27) |
| QEMU | 1 | 10.8k (0.18) → 11.1k (0.17) | 15.9k (2.06) → 36.5k (0.86) | 14.7k (5.49) → 36.6k (2.16) |
| QEMU | 2 | 9.4k (0.20) → 10.4k (0.18) | 17.6k (3.01) → 55.2k (0.64) | 17.3k (6.30) → 65.4k (1.01) |
| QEMU | 4 | 8.0k (0.20) → 7.8k (0.28) | 17.9k (3.49) → 53.0k (0.58) | 16.7k (8.95) → 63.3k (0.88) |

`/big` MB/s, C=1 / C=8: VZ 361/360 → 403/583 (N=1), 350/394 → 426/849
(N=2), 349/429 → 407/917 (N=4); QEMU 75/65 → 257/261, 69/66 → 234/252,
73/54 → 217/231.

Ratios patina ÷ native (same N, median req/s):

| | C=1 | C=16 | C=64 |
|---|---|---|---|
| VZ, N=1 / 2 / 4 | 0.42 / 0.39 / 0.30 | 0.55 / 0.56 / 0.50 | 0.53 / 0.85 / 0.78 |
| QEMU, N=1 / 2 / 4 | 0.33 / 0.33 / 0.25 | 0.46 / 0.44 / 0.40 | 0.46 / 0.48 / 0.43 |

### Observations (after)

1. **No collapse at C=64.** On every target and vCPU count C=64 now gives
   at least as much as C=16 (VZ 42.5k / 117.6k / 116.3k vs 43.2k /
   69.7k / 65.3k), where it was 15–20 % *lower* before. The p99 at VZ
   N=4 C=64 went from 12.0 to 1.27 ms; at N=2 C=64 patina is 85 % of
   native loopback. Profiled cause (D89): every poll of the one smoltcp
   socket set walked every socket four times under one lock, and three
   such polls per request made the lock 93 % busy at C=64.

2. **VZ scales 1 → 2 vCPUs (2.8× at C=64) but not 2 → 4.** 117.6k and
   116.3k req/s. At that point the 10-core host is ~92 % busy, 52 % of it
   in the host kernel (VZ's NAT / vmnet and wrk's four threads), and the
   guest's CPUs 1–3 are ~40 % idle; sharding the smoltcp interface per
   CPU (tried in D89) cut the guest's remaining lock contention in half
   and did not move throughput. This benchmark is host-bound at
   ~120k req/s on this Mac; showing 2 → 4 scaling needs the load
   generator on another machine. Native loopback at N=4 also gains only
   8 % over N=2 at C=64 with the same wrk.

3. **QEMU/HVF was not slirp-bound after all.** C=16/C=64 went from a flat
   ~16–18k req/s to 36–65k and `/big` from 54–75 to 217–261 MB/s with
   the guest-side changes alone; the earlier observation 5 ("the
   bottleneck is host-side") was wrong. slirp's read errors at C=64
   (about 40 per 10 s run) persist and are host-side.

4. **`/big`.** VZ moved 350–360 MB/s on one connection before and
   400–430 now (the doorbell per 1.45 KB frame is gone: 52–113 doorbells
   per 4 MiB instead of ~3180), and 583 / 849 / 917 MB/s at C=8 (was
   360–429): eight connections now use the vCPUs. One connection is
   still bound by per-frame work in the guest (no TSO: 2886 segments per
   response, each checksummed and queued) and by the device thread
   (~2.4–3.2 µs per remaining doorbell).

5. **C=1.** Throughput is within the earlier run-to-run band everywhere
   (VZ 14.2k / 12.3k / 9.1k vs 13.3k / 11.7k / 9.6k; QEMU 11.1k / 10.4k /
   7.8k vs 10.8k / 9.4k / 8.0k) and the p50 is unchanged. C=1 still gets
   slower with more vCPUs (D79's cross-CPU wake-ups: at N=4 the profile
   counts 3.8 NIC interrupts, 3.75 net-task rounds and 5.4 Reschedule IPIs
   per request, each IPI waking a vCPU parked in WFI). The tables above
   show a p99 of 0.66 ms at VZ N=4 C=1 (0.66 and 0.77 ms in two runs,
   0.19 in the third; 0.18 before) at an unchanged p50: a transmit kick
   suppressed by `EVENT_IDX` while the device was idle, which VZ honours
   late (with `patina.net.event_idx=0` the p99 was 0.19–0.20 ms in all
   three runs, but C=16/C=64 lost 10–25 %). Fixed after this run: a
   transmit batch that starts with an idle ring is always kicked (D89,
   kernel bf38609). Five runs of VZ N=4 on that kernel (host idle):
   C=1 9.7–9.9k req/s, p50 0.10 ms, p99 0.18–0.19 ms; C=16 65–68k,
   p99 0.43–0.60 ms; C=64 122–124k, p99 1.08–1.17 ms; `/big` 413 MB/s
   (C=1) and ~970 MB/s (C=8). The other cells of the tables are the
   c9bc3cf run, which the kick rule does not affect.

7. **A ~1 s stall on `/big` at C=8, about one run in ten.** In that run
   the p99 is 0.6–0.8 s and the max 1.0–1.1 s (smoltcp's minimum
   retransmission timeout is 1 s: a segment or ACK lost between the guest
   and VZ's NAT). Seen on every kernel measured here, before and after
   D89 (D85 recorded the same stall on single transfers); the medians are
   unaffected. Not chased.

6. **Host CPU, again.** Everything above was measured with wrk, the VZ NAT
   and the VM on the same ten cores; the VZ C=64 numbers at N ≥ 2 are a
   property of this host as much as of the guest.

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
