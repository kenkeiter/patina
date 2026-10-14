// smpstress exercises the SMP scheduler without relying on signals:
// GOMAXPROCS = vCPUs, cross-thread channel ping-pong next to atomic
// spinners, a contended sync.Mutex (futex wake-ups across CPUs), many
// short sleeps (sub-tick deadlines), GC under load, and two locked OS
// threads that must make progress in the same time windows (real
// parallelism). Prints "PASS smpstress".
//
//	smpstress -cpus N [-dur 2s]
package main

import (
	"flag"
	"fmt"
	"os"
	"runtime"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

func fail(format string, a ...any) {
	fmt.Printf("FAIL smpstress: "+format+"\n", a...)
	os.Exit(1)
}

func main() {
	cpus := flag.Int("cpus", 0, "expected vCPU count (GOMAXPROCS and NumCPU must match)")
	dur := flag.Duration("dur", 2*time.Second, "ping-pong phase duration")
	flag.Parse()

	procs, ncpu := runtime.GOMAXPROCS(0), runtime.NumCPU()
	fmt.Printf("smpstress: GOMAXPROCS=%d NumCPU=%d\n", procs, ncpu)
	if *cpus <= 0 {
		fail("-cpus N is required")
	}
	if procs != *cpus || ncpu != *cpus {
		fail("GOMAXPROCS=%d NumCPU=%d, want %d", procs, ncpu, *cpus)
	}

	pingPong(*cpus, *dur)
	mutexPhase(*cpus)
	sleepPhase()
	gcPhase(*cpus)
	if *cpus >= 2 {
		parallelPhase()
	} else {
		fmt.Println("smpstress: parallel check skipped (1 cpu)")
	}
	fmt.Println("PASS smpstress")
}

// pingPong: pairs of goroutines bounce a counter over unbuffered channels
// while spinners hammer shared atomics, for d.
func pingPong(n int, d time.Duration) {
	var stop atomic.Bool
	var spins [8]atomic.Uint64
	var wg sync.WaitGroup
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			for !stop.Load() {
				spins[i%len(spins)].Add(1)
				if spins[i%len(spins)].Load()%4096 == 0 {
					runtime.Gosched()
				}
			}
		}(i)
	}
	pairs := 2 * n
	var bounces atomic.Uint64
	for p := 0; p < pairs; p++ {
		a, b := make(chan uint64), make(chan uint64)
		wg.Add(2)
		go func() {
			defer wg.Done()
			v := uint64(0)
			for !stop.Load() {
				a <- v
				v = <-b
				if v%2 != 0 {
					fail("ping-pong parity broken: %d", v)
				}
			}
			close(a)
		}()
		go func() {
			defer wg.Done()
			for v := range a {
				bounces.Add(1)
				b <- v + 2
			}
		}()
	}
	time.Sleep(d)
	stop.Store(true)
	wg.Wait()
	var total uint64
	for i := range spins {
		total += spins[i].Load()
	}
	if bounces.Load() == 0 || total == 0 {
		fail("ping-pong made no progress (bounces %d, spins %d)", bounces.Load(), total)
	}
	fmt.Printf("smpstress: ping-pong %d bounces, %d spins in %v\n", bounces.Load(), total, d)
}

// mutexPhase: 4n goroutines increment one counter under a sync.Mutex.
func mutexPhase(n int) {
	const per = 20000
	var mu sync.Mutex
	counter := 0
	var wg sync.WaitGroup
	g := 4 * n
	start := time.Now()
	for i := 0; i < g; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			for j := 0; j < per; j++ {
				mu.Lock()
				counter++
				if j%1000 == i%1000 {
					time.Sleep(10 * time.Microsecond) // sleep holding it: waiters park
				}
				mu.Unlock()
			}
		}(i)
	}
	wg.Wait()
	if counter != g*per {
		fail("mutex counter %d, want %d", counter, g*per)
	}
	fmt.Printf("smpstress: mutex %d goroutines x %d ok in %v\n", g, per, time.Since(start).Round(time.Millisecond))
}

// sleepPhase: 10000 short sleeps from 8 goroutines, then 200 sequential
// 500us sleeps that must finish well under one 10 ms tick each.
func sleepPhase() {
	start := time.Now()
	var wg sync.WaitGroup
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < 1250; j++ {
				t := time.Now()
				time.Sleep(100 * time.Microsecond)
				if time.Since(t) < 100*time.Microsecond {
					fail("time.Sleep(100us) returned early")
				}
			}
		}()
	}
	wg.Wait()
	par := time.Since(start)
	if par > 20*time.Second {
		fail("10000 x Sleep(100us) from 8 goroutines took %v", par)
	}

	start = time.Now()
	for i := 0; i < 200; i++ {
		time.Sleep(500 * time.Microsecond)
	}
	seq := time.Since(start)
	fmt.Printf("smpstress: sleeps: 8x1250 x Sleep(100us) %v; 200 x Sleep(500us) %v\n",
		par.Round(time.Millisecond), seq.Round(time.Millisecond))
	if seq < 100*time.Millisecond {
		fail("200 x Sleep(500us) took only %v", seq)
	}
	if seq > 300*time.Millisecond {
		fail("200 x Sleep(500us) took %v (> 300ms: deadlines not sub-tick?)", seq)
	}
}

var sink atomic.Pointer[[]byte]

// gcPhase: allocating goroutines on every P while runtime.GC runs.
func gcPhase(n int) {
	var before runtime.MemStats
	runtime.ReadMemStats(&before)
	var stop atomic.Bool
	var wg sync.WaitGroup
	for i := 0; i < 2*n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			var keep [][]byte
			for k := 0; !stop.Load(); k++ {
				b := make([]byte, 512+(k*i)%8192)
				b[0] = byte(k)
				sink.Store(&b)
				if k%64 == 0 {
					keep = append(keep, b)
					if len(keep) > 256 {
						keep = keep[:0]
					}
				}
			}
		}(i)
	}
	for i := 0; i < 5; i++ {
		runtime.GC()
		time.Sleep(20 * time.Millisecond)
	}
	stop.Store(true)
	wg.Wait()
	var after runtime.MemStats
	runtime.ReadMemStats(&after)
	if after.NumGC < before.NumGC+5 {
		fail("NumGC %d -> %d, want >= +5", before.NumGC, after.NumGC)
	}
	fmt.Printf("smpstress: gc under load ok, NumGC %d -> %d\n", before.NumGC, after.NumGC)
}

// parallelPhase: two goroutines locked to their own OS threads spin for the
// same 500 ms window, marking each 5 ms bucket in which they ran. On one
// CPU they would alternate (20 ms slices) and rarely share a bucket; on
// two or more CPUs both run in nearly every bucket.
func parallelPhase() {
	const window = 500 * time.Millisecond
	const bucket = 5 * time.Millisecond
	const nb = int(window / bucket)
	var ready sync.WaitGroup
	var done sync.WaitGroup
	var go_ atomic.Bool
	var marks [2][nb]bool
	var tids [2]int
	var iters [2]uint64
	var start time.Time
	var startMu sync.RWMutex
	startMu.Lock()
	for w := 0; w < 2; w++ {
		ready.Add(1)
		done.Add(1)
		go func(w int) {
			defer done.Done()
			runtime.LockOSThread()
			defer runtime.UnlockOSThread()
			tids[w] = syscall.Gettid()
			ready.Done()
			for !go_.Load() {
			}
			startMu.RLock()
			s := start
			startMu.RUnlock()
			for {
				el := time.Since(s)
				if el >= window {
					break
				}
				marks[w][int(el/bucket)] = true
				iters[w]++
			}
		}(w)
	}
	ready.Wait()
	start = time.Now()
	startMu.Unlock()
	go_.Store(true)
	done.Wait()
	if tids[0] == tids[1] {
		fail("locked goroutines share tid %d", tids[0])
	}
	both, any0, any1 := 0, 0, 0
	for b := 0; b < nb; b++ {
		if marks[0][b] {
			any0++
		}
		if marks[1][b] {
			any1++
		}
		if marks[0][b] && marks[1][b] {
			both++
		}
	}
	fmt.Printf("smpstress: parallel tids %d,%d: %d/%d buckets each %d/%d, both %d/%d (iters %d, %d)\n",
		tids[0], tids[1], any0, nb, any1, nb, both, nb, iters[0], iters[1])
	if both < nb/2 {
		fail("locked threads ran together in only %d of %d 5ms buckets (no parallelism?)", both, nb)
	}
}
