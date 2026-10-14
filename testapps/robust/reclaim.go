package main

import (
	"bufio"
	"os"
	"runtime"
	"runtime/debug"
	"strconv"
	"strings"
	"syscall"
)

const (
	reclaimRounds  = 5
	reclaimThreads = 2000
	maxFrameDelta  = 64
)

// procKB reads the "key: N kB" field of a /proc file, in 4 KiB frames.
func procKB(path, key string) int {
	f, err := os.Open(path)
	if err != nil {
		fail("open %s: %v", path, err)
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		if len(fields) >= 2 && fields[0] == key {
			kb, err := strconv.Atoi(fields[1])
			if err != nil {
				fail("%s %s %q: %v", path, key, fields[1], err)
			}
			return kb / 4
		}
	}
	fail("no %s in %s", key, path)
	return 0
}

type memSample struct {
	free   int // MemFree
	kernel int // frames in use that are not user pages: MemTotal - MemFree - VmRSS
}

func sampleMem() memSample {
	total := procKB("/proc/meminfo", "MemTotal:")
	free := procKB("/proc/meminfo", "MemFree:")
	rss := procKB("/proc/self/status", "VmRSS:")
	return memSample{free: free, kernel: total - free - rss}
}

// settled samples after Go returned what it can to the OS.
func settled() memSample {
	debug.FreeOSMemory()
	return sampleMem()
}

// threadRound starts n goroutines that lock their OS thread and block,
// then releases them: a goroutine that exits while locked makes Go exit
// its thread (M), so this is n clone + n thread exits.
func threadRound(n int) {
	start := make(chan struct{})
	release := make(chan struct{})
	done := make(chan struct{}, n)
	for i := 0; i < n; i++ {
		go func() {
			runtime.LockOSThread()
			start <- struct{}{}
			<-release
			done <- struct{}{}
			// Returns locked: the M exits.
		}()
	}
	for i := 0; i < n; i++ {
		<-start
	}
	close(release)
	for i := 0; i < n; i++ {
		<-done
	}
}

func checkReclaim() {
	// Kernel stacks: a warm-up round grows Go's heap and the kernel's
	// pools to their peak, then 5 rounds of 2000 thread exits must leave
	// the frames the kernel holds flat. (MemFree alone also moves with
	// what Go's allocator keeps resident, which is not the kernel's.)
	threadRound(reclaimThreads)
	before := settled()
	for r := 0; r < reclaimRounds; r++ {
		threadRound(reclaimThreads)
	}
	after := settled()
	if d := after.kernel - before.kernel; d < -maxFrameDelta || d > maxFrameDelta {
		fail("kstack reclaim: kernel-held frames %d -> %d (delta %d) over %d x %d thread exits (free %d -> %d)",
			before.kernel, after.kernel, d, reclaimRounds, reclaimThreads, before.free, after.free)
	}
	ok("kstack reclaim: kernel-held frames %d -> %d (delta %d), free frames %d -> %d, over %d x %d thread exits",
		before.kernel, after.kernel, after.kernel-before.kernel, before.free, after.free,
		reclaimRounds, reclaimThreads)

	// User page tables: 1 GiB lazy mapping, one byte touched every 2 MiB
	// (a page-table page each), munmap: the tables must be freed too.
	const size = 1 << 30
	const stride = 2 << 20
	pt0 := sampleMem()
	// MAP_NORESERVE: 1 GiB is more than the VM's RAM, so it must not be
	// charged against the commit limit (D78).
	b, err := syscall.Mmap(-1, 0, size, syscall.PROT_READ|syscall.PROT_WRITE,
		syscall.MAP_PRIVATE|syscall.MAP_ANON|syscall.MAP_NORESERVE)
	if err != nil {
		fail("mmap 1 GiB: %v", err)
	}
	for off := 0; off < size; off += stride {
		b[off] = 1
	}
	mid := sampleMem()
	if err := syscall.Munmap(b); err != nil {
		fail("munmap: %v", err)
	}
	pt1 := sampleMem()
	tables := mid.kernel - pt0.kernel
	if d := pt1.kernel - pt0.kernel; d < -8 || d > 8 || tables < size/stride {
		fail("page-table reclaim: %d table frames while touched; kernel-held frames %d -> %d (delta %d) after munmap",
			tables, pt0.kernel, pt1.kernel, d)
	}
	ok("page-table reclaim: 1 GiB touched every 2 MiB held %d page-table frames; kernel-held frames %d -> %d (delta %d), free frames %d -> %d after munmap",
		tables, pt0.kernel, pt1.kernel, pt1.kernel-pt0.kernel, pt0.free, pt1.free)
}
