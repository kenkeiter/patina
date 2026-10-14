// tlbstress hunts stale-TLB use-after-free on SMP: memory that one thread
// unmaps (munmap, madvise(MADV_DONTNEED)) while another thread may still
// hold a translation for it must never be written through that stale
// translation once its frame belongs to someone else.
//
// For -dur (default 3s), concurrently:
//   - 4 mapper goroutines: mmap 1 MiB, fill every page with a
//     goroutine-specific pattern, verify, MADV_DONTNEED the first half,
//     check it reads back as zeros (and the second half is intact), munmap;
//   - a GC goroutine: keeps live slices with checksums, replaces them, calls
//     runtime.GC() every 100 ms, verifies every live slice;
//   - a racing writer (locked OS thread) that keeps storing a marker into
//     every page of a shared region, while a reclaimer goroutine (another
//     OS thread) repeatedly MADV_DONTNEEDs that region and immediately
//     maps, fills and verifies fresh pages: a fresh page showing the
//     writer's marker means the writer wrote through a stale translation
//     into a frame that had been freed and reused;
//   - a canary goroutine: mmap pages, write a per-page canary, munmap, mmap
//     again and require all-zero pages (never another page's pattern).
//
// Prints "PASS tlbstress".
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
	"unsafe"
)

const pageSize = 4096

const (
	tagMapper = 0x11 // high byte of mapper patterns
	tagWriter = 0xba // high byte of the racing writer's marker
	tagReclm  = 0x60 // high byte of the reclaimer's fresh-page pattern
	tagCanary = 0xca // high byte of canaries
)

var failed atomic.Bool

func fail(format string, a ...any) {
	if failed.CompareAndSwap(false, true) {
		fmt.Printf("FAIL tlbstress: "+format+"\n", a...)
		os.Exit(1)
	}
	select {} // another goroutine is already exiting
}

func words(b []byte) []uint64 {
	return unsafe.Slice((*uint64)(unsafe.Pointer(&b[0])), len(b)/8)
}

func mmap(n int) []byte {
	b, err := syscall.Mmap(-1, 0, n, syscall.PROT_READ|syscall.PROT_WRITE,
		syscall.MAP_ANON|syscall.MAP_PRIVATE)
	if err != nil {
		fail("mmap(%d): %v", n, err)
	}
	return b
}

func munmap(b []byte) {
	if err := syscall.Munmap(b); err != nil {
		fail("munmap: %v", err)
	}
}

func dontneed(b []byte) {
	if err := syscall.Madvise(b, syscall.MADV_DONTNEED); err != nil {
		fail("madvise(DONTNEED): %v", err)
	}
}

func pat(tag, id, iter, idx uint64) uint64 {
	return tag<<56 | (id&0xff)<<48 | (iter&0xffffff)<<24 | idx&0xffffff
}

// describe classifies a corrupt word by its tag byte.
func describe(v uint64) string {
	switch v >> 56 {
	case tagWriter:
		return "the racing writer's marker (stale-TLB write after free)"
	case tagMapper:
		return "a mapper's pattern"
	case tagReclm:
		return "a reclaimer pattern"
	case tagCanary:
		return "a canary (data from a freed page)"
	}
	return "unknown data"
}

func mapper(id int, stop *atomic.Bool, iters *atomic.Uint64) {
	const size = 1 << 20
	for it := uint64(0); !stop.Load(); it++ {
		b := mmap(size)
		w := words(b)
		for i := range w {
			w[i] = pat(tagMapper, uint64(id), it, uint64(i))
		}
		for i, v := range w {
			if want := pat(tagMapper, uint64(id), it, uint64(i)); v != want {
				fail("mapper %d iter %d word %d: %#x want %#x (%s)", id, it, i, v, want, describe(v))
			}
		}
		dontneed(b[:size/2])
		for i, v := range w {
			want := uint64(0)
			if i >= len(w)/2 {
				want = pat(tagMapper, uint64(id), it, uint64(i))
			}
			if v != want {
				fail("mapper %d iter %d after DONTNEED word %d: %#x want %#x (%s)", id, it, i, v, want, describe(v))
			}
		}
		munmap(b)
		iters.Add(1)
	}
}

func checksum(b []byte) uint64 {
	h := uint64(1469598103934665603)
	for _, c := range b {
		h = (h ^ uint64(c)) * 1099511628211
	}
	return h
}

func gcWorker(stop *atomic.Bool, iters *atomic.Uint64) {
	type live struct {
		b   []byte
		sum uint64
	}
	var set [64]live
	seed := uint64(0x9e3779b97f4a7c15)
	rnd := func() uint64 {
		seed ^= seed << 13
		seed ^= seed >> 7
		seed ^= seed << 17
		return seed
	}
	last := time.Now()
	for !stop.Load() {
		i := rnd() % uint64(len(set))
		n := 1024 + int(rnd()%(64<<10))
		b := make([]byte, n)
		for j := range b {
			b[j] = byte(rnd())
		}
		set[i] = live{b, checksum(b)}
		for k := range set {
			if set[k].b != nil && checksum(set[k].b) != set[k].sum {
				fail("gc: live slice %d (%d bytes) changed under GC", k, len(set[k].b))
			}
		}
		if time.Since(last) >= 100*time.Millisecond {
			runtime.GC()
			last = time.Now()
		}
		iters.Add(1)
	}
}

// racer: the writer keeps storing its marker into every page of region r
// while the reclaimer frees r's frames (DONTNEED) and reuses them.
func racer(stop *atomic.Bool, iters *atomic.Uint64, done *sync.WaitGroup) {
	const pages = 64
	r := mmap(pages * pageSize)
	rw := words(r)
	var writerStop atomic.Bool
	var writerDone sync.WaitGroup
	writerDone.Add(1)
	go func() {
		defer writerDone.Done()
		runtime.LockOSThread()
		for it := uint64(0); !writerStop.Load(); it++ {
			for p := 0; p < pages; p++ {
				// Word 0 is where a free frame's free-list link lives.
				rw[p*pageSize/8] = pat(tagWriter, 0, it, uint64(p))
				rw[p*pageSize/8+1] = pat(tagWriter, 1, it, uint64(p))
			}
		}
	}()
	runtime.LockOSThread()
	for it := uint64(0); !stop.Load(); it++ {
		dontneed(r)
		fresh := mmap(pages * pageSize)
		fw := words(fresh)
		for i := range fw {
			fw[i] = pat(tagReclm, 0, it, uint64(i))
		}
		// Give a stale writer time to hit the reused frames.
		for spin := 0; spin < 3; spin++ {
			for i, v := range fw {
				if want := pat(tagReclm, 0, it, uint64(i)); v != want {
					fail("reclaimer iter %d fresh page %d word %d: %#x want %#x (%s)",
						it, i*8/pageSize, i%(pageSize/8), v, want, describe(v))
				}
			}
		}
		munmap(fresh)
		iters.Add(1)
	}
	runtime.UnlockOSThread()
	writerStop.Store(true)
	writerDone.Wait()
	munmap(r)
	done.Done()
}

func canary(stop *atomic.Bool, iters *atomic.Uint64) {
	const pages = 256
	for it := uint64(0); !stop.Load(); it++ {
		b := mmap(pages * pageSize)
		w := words(b)
		for p := 0; p < pages; p++ {
			for j := 0; j < pageSize/8; j += 64 {
				w[p*pageSize/8+j] = pat(tagCanary, 0, it, uint64(p))
			}
		}
		munmap(b)
		b = mmap(pages * pageSize)
		for i, v := range words(b) {
			if v != 0 {
				fail("canary iter %d: fresh page %d word %d = %#x (%s)",
					it, i*8/pageSize, i%(pageSize/8), v, describe(v))
			}
		}
		munmap(b)
		iters.Add(1)
	}
}

func main() {
	dur := flag.Duration("dur", 3*time.Second, "stress duration")
	flag.Parse()
	fmt.Printf("tlbstress: GOMAXPROCS=%d NumCPU=%d dur=%v\n",
		runtime.GOMAXPROCS(0), runtime.NumCPU(), *dur)

	var stop atomic.Bool
	var wg sync.WaitGroup
	var mapIt, gcIt, raceIt, canIt atomic.Uint64
	for id := 0; id < 4; id++ {
		wg.Add(1)
		go func() { defer wg.Done(); mapper(id, &stop, &mapIt) }()
	}
	wg.Add(1)
	go func() { defer wg.Done(); gcWorker(&stop, &gcIt) }()
	wg.Add(1)
	go racer(&stop, &raceIt, &wg)
	wg.Add(1)
	go func() { defer wg.Done(); canary(&stop, &canIt) }()

	time.Sleep(*dur)
	stop.Store(true)
	wg.Wait()
	fmt.Printf("tlbstress: iterations: mapper %d, gc %d, reclaimer %d, canary %d\n",
		mapIt.Load(), gcIt.Load(), raceIt.Load(), canIt.Load())
	if mapIt.Load() == 0 || gcIt.Load() == 0 || raceIt.Load() == 0 || canIt.Load() == 0 {
		fail("a worker made no progress")
	}
	fmt.Println("PASS tlbstress")
}
