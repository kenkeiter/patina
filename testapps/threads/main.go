// threads exercises clone/futex/scheduling/GC: many goroutines allocating
// under GC pressure, channel fan-in, timers and preemption of a busy loop.
package main

import (
	"flag"
	"fmt"
	"os"
	"runtime"
	"sync"
	"sync/atomic"
	"time"
)

func fail(format string, a ...any) {
	fmt.Printf("FAIL threads: "+format+"\n", a...)
	os.Exit(1)
}

func main() {
	workers := flag.Int("g", 1000, "goroutines")
	rounds := flag.Int("rounds", 200, "allocation rounds per goroutine")
	cpus := flag.Int("cpus", 0, "if > 0: expected vCPU count (GOMAXPROCS and NumCPU must match)")
	flag.Parse()

	fmt.Printf("threads: GOMAXPROCS=%d NumCPU=%d\n", runtime.GOMAXPROCS(0), runtime.NumCPU())
	if *cpus > 0 && (runtime.GOMAXPROCS(0) != *cpus || runtime.NumCPU() != *cpus) {
		fail("GOMAXPROCS=%d NumCPU=%d, want %d", runtime.GOMAXPROCS(0), runtime.NumCPU(), *cpus)
	}

	// Fan-out / fan-in with allocation pressure.
	var wg sync.WaitGroup
	results := make(chan uint64, *workers)
	for i := 0; i < *workers; i++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			var sum uint64
			var keep [][]byte
			for r := 0; r < *rounds; r++ {
				b := make([]byte, 1024+(id*r)%4096)
				b[0], b[len(b)-1] = byte(id), byte(r)
				sum += uint64(b[0]) + uint64(b[len(b)-1])
				if r%16 == 0 {
					keep = append(keep, b) // some survive a while
				}
				if r%50 == 0 {
					runtime.Gosched()
				}
			}
			sum += uint64(len(keep))
			results <- sum
		}(i)
	}
	wg.Wait()
	close(results)
	var total uint64
	for s := range results {
		total += s
	}
	var want uint64
	for id := 0; id < *workers; id++ {
		for r := 0; r < *rounds; r++ {
			want += uint64(byte(id)) + uint64(byte(r))
		}
		want += uint64((*rounds + 15) / 16)
	}
	if total != want {
		fail("checksum %d, want %d", total, want)
	}

	runtime.GC()
	var ms runtime.MemStats
	runtime.ReadMemStats(&ms)
	if ms.NumGC == 0 {
		fail("no GC cycles ran")
	}
	fmt.Printf("threads: %d goroutines ok, NumGC=%d HeapAlloc=%dKiB\n", *workers, ms.NumGC, ms.HeapAlloc/1024)

	// Sleep / timer accuracy (monotonic clock must advance).
	start := time.Now()
	time.Sleep(50 * time.Millisecond)
	if d := time.Since(start); d < 50*time.Millisecond || d > 5*time.Second {
		fail("time.Sleep(50ms) took %v", d)
	}
	tick := time.NewTicker(10 * time.Millisecond)
	for i := 0; i < 5; i++ {
		<-tick.C
	}
	tick.Stop()

	// Async preemption: a tight loop must not starve a timer goroutine
	// (needs SIGURG delivery when GOMAXPROCS is small).
	var stop atomic.Bool
	go func() {
		for !stop.Load() {
		}
	}()
	runtime.GOMAXPROCS(1)
	done := make(chan struct{})
	go func() { time.Sleep(20 * time.Millisecond); close(done) }()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		fail("tight loop starved other goroutines (async preemption broken?)")
	}
	stop.Store(true)

	fmt.Println("PASS threads")
}
