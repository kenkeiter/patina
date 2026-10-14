package main

import (
	"fmt"
	"os"
	"syscall"
)

// kheapMem: the numbers checkKheap tracks, in 4 KiB frames.
type kheapMem struct {
	memSample
	committed int // Committed_AS
}

func sampleKheap() kheapMem {
	s := settled()
	return kheapMem{memSample: s, committed: procKB("/proc/meminfo", "Committed_AS:")}
}

func (m kheapMem) String() string {
	return fmt.Sprintf("kernel %d MiB, MemFree %d MiB, Committed_AS %d MiB",
		m.kernel/256, m.free/256, m.committed/256)
}

const (
	kheapPipes   = 50000
	kheapRings   = 50 // every 50th pipe gets a byte: 1000 64 KiB rings
	kheapUnix    = 8500
	kheapUDP     = 2000
	kheapTCP     = 1000
	kheapEpolls  = 10000
	kheapTmpMiB  = 200
	kheapSlackMB = 4 // free RAM must come back to within this of baseline
)

func raiseNofile() {
	var r syscall.Rlimit
	if err := syscall.Getrlimit(syscall.RLIMIT_NOFILE, &r); err != nil {
		fail("getrlimit: %v", err)
	}
	r.Cur = r.Max
	if err := syscall.Setrlimit(syscall.RLIMIT_NOFILE, &r); err != nil {
		fail("setrlimit: %v", err)
	}
}

func closeAll(fds []int) {
	for _, fd := range fds {
		syscall.Close(fd)
	}
}

// kheapPhase runs open (which returns the fds it holds), samples the
// peak, closes everything and samples again.
func kheapPhase(name string, open func() []int) (peak, after kheapMem) {
	fds := open()
	peak = sampleKheap()
	closeAll(fds)
	after = sampleKheap()
	fmt.Printf("robust: kheap %s: %d fds; peak %v; after close %v\n", name, len(fds), peak, after)
	return
}

func kheapPipesOpen() []int {
	fds := make([]int, 0, 2*kheapPipes)
	one := []byte{1}
	for i := 0; i < kheapPipes; i++ {
		var p [2]int
		if err := syscall.Pipe2(p[:], syscall.O_CLOEXEC); err != nil {
			fail("kheap: pipe2 #%d: %v", i, err)
		}
		if i%kheapRings == 0 {
			if _, err := syscall.Write(p[1], one); err != nil {
				fail("kheap: pipe write #%d: %v", i, err)
			}
		}
		fds = append(fds, p[0], p[1])
	}
	return fds
}

func kheapSocketsOpen() []int {
	fds := make([]int, 0, 2*kheapUnix+kheapUDP+kheapTCP)
	one := []byte{1}
	for i := 0; i < kheapUnix; i++ {
		p, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
		if err != nil {
			fail("kheap: socketpair #%d: %v", i, err)
		}
		if _, err := syscall.Write(p[0], one); err != nil {
			fail("kheap: socketpair write #%d: %v", i, err)
		}
		fds = append(fds, p[0], p[1])
	}
	for i := 0; i < kheapUDP; i++ {
		fd, err := syscall.Socket(syscall.AF_INET, syscall.SOCK_DGRAM|syscall.SOCK_CLOEXEC, 0)
		if err != nil {
			fail("kheap: udp socket #%d: %v", i, err)
		}
		fds = append(fds, fd)
	}
	for i := 0; i < kheapTCP; i++ {
		fd, err := syscall.Socket(syscall.AF_INET, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
		if err != nil {
			fail("kheap: tcp socket #%d: %v", i, err)
		}
		fds = append(fds, fd)
	}
	return fds
}

func kheapEpollsOpen() []int {
	fds := make([]int, 0, kheapEpolls+1)
	efd, _, e := syscall.RawSyscall(syscall.SYS_EVENTFD2, 0, syscall.O_CLOEXEC, 0)
	if e != 0 {
		fail("kheap: eventfd2: %v", e)
	}
	fds = append(fds, int(efd))
	for i := 0; i < kheapEpolls; i++ {
		ep, err := syscall.EpollCreate1(syscall.EPOLL_CLOEXEC)
		if err != nil {
			fail("kheap: epoll_create1 #%d: %v", i, err)
		}
		ev := syscall.EpollEvent{Events: syscall.EPOLLIN, Fd: int32(efd)}
		if err := syscall.EpollCtl(ep, syscall.EPOLL_CTL_ADD, int(efd), &ev); err != nil {
			fail("kheap: epoll_ctl #%d: %v", i, err)
		}
		fds = append(fds, ep)
	}
	return fds
}

// kheapTmpfs writes kheapTmpMiB MiB to /tmp in 4 MiB files, samples the
// peak, then deletes them.
func kheapTmpfs() (peak, after kheapMem) {
	buf := make([]byte, 4<<20)
	for i := range buf {
		buf[i] = byte(i)
	}
	n := kheapTmpMiB / 4
	for i := 0; i < n; i++ {
		if err := os.WriteFile(fmt.Sprintf("/tmp/kheap-%d", i), buf, 0o600); err != nil {
			fail("kheap: write /tmp/kheap-%d: %v", i, err)
		}
	}
	buf = nil
	peak = sampleKheap()
	for i := 0; i < n; i++ {
		if err := os.Remove(fmt.Sprintf("/tmp/kheap-%d", i)); err != nil {
			fail("kheap: remove /tmp/kheap-%d: %v", i, err)
		}
	}
	after = sampleKheap()
	fmt.Printf("robust: kheap tmpfs: %d MiB; peak %v; after delete %v\n", kheapTmpMiB, peak, after)
	return
}

// checkKheap: bursts of kernel objects (pipes, sockets, epolls, tmpfs
// pages) grow the kernel heap by hundreds of MiB; once they are closed,
// the heap must give the frames back (D87): free RAM and the frames the
// kernel holds return to within kheapSlackMB of the baseline.
func checkKheap() {
	raiseNofile()
	// The fd table grows to the peak fd count and keeps its size (as on
	// Linux): grow it before the baseline.
	kheapPhase("warm-up", func() []int {
		fds := make([]int, 0, 2*kheapPipes)
		for i := 0; i < kheapPipes; i++ {
			fd, _, e := syscall.RawSyscall(syscall.SYS_EVENTFD2, 0, syscall.O_CLOEXEC, 0)
			if e != 0 {
				fail("kheap: eventfd2 #%d: %v", i, e)
			}
			fds = append(fds, int(fd))
		}
		for i := 0; i < kheapPipes; i++ {
			fd, err := syscall.Dup(fds[i])
			if err != nil {
				fail("kheap: dup #%d: %v", i, err)
			}
			fds = append(fds, fd)
		}
		return fds
	})
	base := sampleKheap()
	fmt.Printf("robust: kheap baseline: %v\n", base)
	maxPeak := 0
	check := func(name string, peak, after kheapMem) {
		grew := peak.kernel - base.kernel
		if grew > maxPeak {
			maxPeak = grew
		}
		dk := after.kernel - base.kernel
		df := base.free - after.free
		if dk > kheapSlackMB*256 || df > kheapSlackMB*256 {
			fail("kheap %s: kernel held %+d frames (%+d MiB) over baseline after close, MemFree %d -> %d MiB (peak grew %d MiB)",
				name, dk, dk/256, base.free/256, after.free/256, grew/256)
		}
	}
	p, a := kheapPhase("pipes", kheapPipesOpen)
	check("pipes", p, a)
	p, a = kheapPhase("sockets", kheapSocketsOpen)
	check("sockets", p, a)
	p, a = kheapPhase("epolls", kheapEpollsOpen)
	check("epolls", p, a)
	p, a = kheapTmpfs()
	check("tmpfs", p, a)
	end := sampleKheap()
	if maxPeak < 64*256 {
		fail("kheap: the kernel grew only %d MiB at the peak; the test exercised nothing", maxPeak/256)
	}
	ok("kheap shrink: kernel-held %d -> %d MiB (peak +%d MiB), MemFree %d -> %d MiB, Committed_AS %d -> %d MiB",
		base.kernel/256, end.kernel/256, maxPeak/256, base.free/256, end.free/256,
		base.committed/256, end.committed/256)
}
