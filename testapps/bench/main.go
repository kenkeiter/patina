// bench times the kernel paths a Go program leans on (design §14, Phase 5
// "per-syscall benchmarks", decision D79). Each benchmark warms up, runs a
// fixed count (or, for throughput, a fixed duration) and prints one line
//
//	bench <name>: <value> <unit>
//
// then "PASS bench" and exit 0. Flags:
//
//	-n F      scale every iteration count and duration by F (default 1)
//	-only L   run only the benchmarks named in the comma-separated list L
//	-list     print the benchmark names and exit
//
// Latency rows are ns or µs per operation (lower is better); throughput rows
// are MB/s (10^6 bytes, higher is better). Ping-pong benchmarks raise
// GOMAXPROCS to at least 2 so that both OS-locked threads hold a P and the
// number measures the kernel's block/wake path, not Go's sysmon P handoff.
//
// std only: no dependencies outside the Go distribution.
package main

import (
	"crypto/rand"
	"flag"
	"fmt"
	"io"
	"net"
	"os"
	"os/signal"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unsafe"
)

var scale = flag.Float64("n", 1, "scale iteration counts and durations")

type bench struct {
	name string
	run  func() (float64, string)
}

var benches = []bench{
	{"getpid-raw", benchGetpidRaw},
	{"getpid-syscall", benchGetpidSyscall},
	{"clock_gettime-syscall", benchClockSyscall},
	{"time.Now", benchTimeNow},
	{"getrandom32-syscall", benchGetrandomSyscall},
	{"crypto-rand-read32", benchCryptoRand},
	{"futex-pingpong", benchFutexPingPong},
	{"chan-pingpong", benchChanPingPong},
	{"mutex-handoff", benchMutexHandoff},
	{"pipe-pingpong", benchPipePingPong},
	{"pipe-throughput", benchPipeThroughput},
	{"netpoll-pipe-pingpong", benchNetpollPipe},
	{"tcp-rtt", benchTCPRTT},
	{"tcp-throughput", benchTCPThroughput},
	{"unix-rtt", benchUnixRTT},
	{"unix-throughput", benchUnixThroughput},
	{"thread-create-exit", benchThreadCreateExit},
	{"mmap-munmap-1M", benchMmap},
	{"signal-rtt", benchSignal},
}

func count(base int) int {
	n := int(float64(base) * *scale)
	if n < 1 {
		n = 1
	}
	return n
}

func dur(base time.Duration) time.Duration {
	return time.Duration(float64(base) * *scale)
}

func fail(f string, a ...any) {
	fmt.Printf("FAIL bench: "+f+"\n", a...)
	os.Exit(1)
}

// timeOps runs f(warm) then f(n) and returns the time per op of the second.
func timeOps(n int, f func(n int)) time.Duration {
	warm := n / 10
	if warm < 1 {
		warm = 1
	}
	f(warm)
	t0 := time.Now()
	f(n)
	return time.Since(t0) / time.Duration(n)
}

func ns(d time.Duration) (float64, string) { return float64(d.Nanoseconds()), "ns" }
func us(d time.Duration) (float64, string) { return float64(d.Nanoseconds()) / 1e3, "us" }
func mbps(bytes int64, d time.Duration) (float64, string) {
	return float64(bytes) / d.Seconds() / 1e6, "MB/s"
}

// withProcs runs f with GOMAXPROCS at least p.
func withProcs(p int, f func()) {
	old := runtime.GOMAXPROCS(0)
	if old < p {
		runtime.GOMAXPROCS(p)
		defer runtime.GOMAXPROCS(old)
	}
	f()
}

// --- syscall entry/exit ---

func benchGetpidRaw() (float64, string) {
	return ns(timeOps(count(1_000_000), func(n int) {
		for i := 0; i < n; i++ {
			syscall.RawSyscall(syscall.SYS_GETPID, 0, 0, 0)
		}
	}))
}

func benchGetpidSyscall() (float64, string) {
	return ns(timeOps(count(1_000_000), func(n int) {
		for i := 0; i < n; i++ {
			syscall.Syscall(syscall.SYS_GETPID, 0, 0, 0)
		}
	}))
}

// --- clocks and randomness (syscall vs vDSO) ---

func benchClockSyscall() (float64, string) {
	var ts syscall.Timespec
	return ns(timeOps(count(1_000_000), func(n int) {
		for i := 0; i < n; i++ {
			_, _, e := syscall.RawSyscall(syscall.SYS_CLOCK_GETTIME, 1, uintptr(unsafe.Pointer(&ts)), 0)
			if e != 0 {
				fail("clock_gettime: %v", e)
			}
		}
	}))
}

var sink int64

func benchTimeNow() (float64, string) {
	return ns(timeOps(count(1_000_000), func(n int) {
		for i := 0; i < n; i++ {
			sink += time.Now().UnixNano()
		}
	}))
}

func sysGetrandom() uintptr {
	if runtime.GOARCH == "amd64" {
		return 318
	}
	return 278
}

func benchGetrandomSyscall() (float64, string) {
	var b [32]byte
	nr := sysGetrandom()
	return ns(timeOps(count(200_000), func(n int) {
		for i := 0; i < n; i++ {
			r, _, e := syscall.RawSyscall(nr, uintptr(unsafe.Pointer(&b[0])), 32, 0)
			if e != 0 || r != 32 {
				fail("getrandom: %d %v", r, e)
			}
		}
	}))
}

func benchCryptoRand() (float64, string) {
	var b [32]byte
	return ns(timeOps(count(200_000), func(n int) {
		for i := 0; i < n; i++ {
			if _, err := rand.Read(b[:]); err != nil {
				fail("crypto/rand: %v", err)
			}
		}
	}))
}

// --- futex ---

const (
	futexWait = 0
	futexWake = 1
	futexPriv = 128
)

func futex(addr *uint32, op, val uint32) {
	_, _, e := syscall.Syscall6(syscall.SYS_FUTEX, uintptr(unsafe.Pointer(addr)), uintptr(op|futexPriv), uintptr(val), 0, 0, 0)
	if e != 0 && e != syscall.EAGAIN && e != syscall.EINTR {
		fail("futex op %d: %v", op, e)
	}
}

// waitFor blocks until *w == want (the word only ever toggles 0/1).
func waitFor(w *uint32, want uint32) {
	for {
		v := atomic.LoadUint32(w)
		if v == want {
			return
		}
		futex(w, futexWait, v)
	}
}

func set(w *uint32, v uint32) {
	atomic.StoreUint32(w, v)
	futex(w, futexWake, 1)
}

// Two OS-locked threads hand a token back and forth through one word: the
// pinger sets it to 1 and waits for 0, the ponger waits for 1 and sets 0.
func benchFutexPingPong() (v float64, u string) {
	withProcs(2, func() {
		v, u = us(timeOps(count(20_000), func(n int) {
			var w uint32
			done := make(chan struct{})
			go func() {
				runtime.LockOSThread()
				defer runtime.UnlockOSThread()
				for i := 0; i < n; i++ {
					waitFor(&w, 1)
					set(&w, 0)
				}
				close(done)
			}()
			runtime.LockOSThread()
			for i := 0; i < n; i++ {
				set(&w, 1)
				waitFor(&w, 0)
			}
			runtime.UnlockOSThread()
			<-done
		}))
	})
	return
}

func benchChanPingPong() (float64, string) {
	return us(timeOps(count(100_000), func(n int) {
		ping, pong := make(chan int), make(chan int)
		go func() {
			for v := range ping {
				pong <- v
			}
		}()
		for i := 0; i < n; i++ {
			ping <- i
			<-pong
		}
		close(ping)
	}))
}

// Two goroutines strictly alternate through a sync.Mutex + sync.Cond turn.
func benchMutexHandoff() (float64, string) {
	return us(timeOps(count(100_000), func(n int) {
		var mu sync.Mutex
		c := sync.NewCond(&mu)
		turn := 0
		done := make(chan struct{})
		go func() {
			mu.Lock()
			for i := 0; i < n; i++ {
				for turn != 1 {
					c.Wait()
				}
				turn = 0
				c.Signal()
			}
			mu.Unlock()
			close(done)
		}()
		mu.Lock()
		for i := 0; i < n; i++ {
			turn = 1
			c.Signal()
			for turn != 0 {
				c.Wait()
			}
		}
		mu.Unlock()
		<-done
	}))
}

// --- pipes ---

func pipe() (r, w int) {
	var p [2]int
	if err := syscall.Pipe2(p[:], syscall.O_CLOEXEC); err != nil {
		fail("pipe2: %v", err)
	}
	return p[0], p[1]
}

func rd(fd int, b []byte) int {
	n, err := syscall.Read(fd, b)
	if err != nil {
		fail("read: %v", err)
	}
	return n
}

func wr(fd int, b []byte) {
	for len(b) > 0 {
		n, err := syscall.Write(fd, b)
		if err != nil {
			fail("write: %v", err)
		}
		b = b[n:]
	}
}

// Blocking pipes, two OS-locked threads, 1 byte each way.
func benchPipePingPong() (v float64, u string) {
	withProcs(2, func() {
		v, u = us(timeOps(count(20_000), func(n int) {
			ar, aw := pipe()
			br, bw := pipe()
			done := make(chan struct{})
			go func() {
				runtime.LockOSThread()
				defer runtime.UnlockOSThread()
				b := make([]byte, 1)
				for i := 0; i < n; i++ {
					rd(ar, b)
					wr(bw, b)
				}
				close(done)
			}()
			runtime.LockOSThread()
			b := []byte{'x'}
			for i := 0; i < n; i++ {
				wr(aw, b)
				rd(br, b)
			}
			runtime.UnlockOSThread()
			<-done
			for _, fd := range []int{ar, aw, br, bw} {
				syscall.Close(fd)
			}
		}))
	})
	return
}

func benchPipeThroughput() (v float64, u string) {
	withProcs(2, func() {
		r, w := pipe()
		var total int64
		done := make(chan struct{})
		go func() {
			runtime.LockOSThread()
			defer runtime.UnlockOSThread()
			b := make([]byte, 64<<10)
			for {
				n, err := syscall.Read(r, b)
				if err != nil {
					fail("pipe read: %v", err)
				}
				if n == 0 {
					break
				}
				atomic.AddInt64(&total, int64(n))
			}
			close(done)
		}()
		buf := make([]byte, 64<<10)
		// Warm up, then time a fixed duration of 64 KiB writes.
		for i := 0; i < 16; i++ {
			wr(w, buf)
		}
		start := atomic.LoadInt64(&total)
		t0 := time.Now()
		d := dur(time.Second)
		for time.Since(t0) < d {
			wr(w, buf)
		}
		got := atomic.LoadInt64(&total) - start
		el := time.Since(t0)
		syscall.Close(w)
		<-done
		syscall.Close(r)
		v, u = mbps(got, el)
	})
	return
}

// os.Pipe goes through Go's netpoller (non-blocking fds + epoll): a goroutine
// parked in Read is woken by the other goroutine's Write.
func benchNetpollPipe() (float64, string) {
	return us(timeOps(count(20_000), func(n int) {
		ar, aw, err := os.Pipe()
		if err != nil {
			fail("os.Pipe: %v", err)
		}
		br, bw, err := os.Pipe()
		if err != nil {
			fail("os.Pipe: %v", err)
		}
		done := make(chan struct{})
		go func() {
			b := make([]byte, 1)
			for i := 0; i < n; i++ {
				if _, err := io.ReadFull(ar, b); err != nil {
					fail("netpoll read: %v", err)
				}
				if _, err := bw.Write(b); err != nil {
					fail("netpoll write: %v", err)
				}
			}
			close(done)
		}()
		b := []byte{'x'}
		for i := 0; i < n; i++ {
			if _, err := aw.Write(b); err != nil {
				fail("netpoll write: %v", err)
			}
			if _, err := io.ReadFull(br, b); err != nil {
				fail("netpoll read: %v", err)
			}
		}
		<-done
		ar.Close()
		aw.Close()
		br.Close()
		bw.Close()
	}))
}

// --- sockets ---

// connPair returns a connected (client, server) pair on network/addr.
func connPair(network, addr string) (net.Conn, net.Conn, func()) {
	ln, err := net.Listen(network, addr)
	if err != nil {
		fail("listen %s %s: %v", network, addr, err)
	}
	ch := make(chan net.Conn, 1)
	go func() {
		c, err := ln.Accept()
		if err != nil {
			fail("accept: %v", err)
		}
		ch <- c
	}()
	c, err := net.Dial(network, ln.Addr().String())
	if err != nil {
		fail("dial %s: %v", network, err)
	}
	s := <-ch
	return c, s, func() { c.Close(); s.Close(); ln.Close() }
}

func rtt(network, addr string, base int) (float64, string) {
	c, s, cleanup := connPair(network, addr)
	defer cleanup()
	if tc, ok := c.(*net.TCPConn); ok {
		tc.SetNoDelay(true)
		s.(*net.TCPConn).SetNoDelay(true)
	}
	go func() {
		b := make([]byte, 1)
		for {
			if _, err := io.ReadFull(s, b); err != nil {
				return
			}
			if _, err := s.Write(b); err != nil {
				return
			}
		}
	}()
	b := []byte{'x'}
	return us(timeOps(count(base), func(n int) {
		for i := 0; i < n; i++ {
			if _, err := c.Write(b); err != nil {
				fail("%s write: %v", network, err)
			}
			if _, err := io.ReadFull(c, b); err != nil {
				fail("%s read: %v", network, err)
			}
		}
	}))
}

func throughput(network, addr string, d time.Duration) (float64, string) {
	c, s, cleanup := connPair(network, addr)
	defer cleanup()
	var total int64
	done := make(chan struct{})
	go func() {
		b := make([]byte, 64<<10)
		for {
			n, err := s.Read(b)
			atomic.AddInt64(&total, int64(n))
			if err != nil {
				break
			}
		}
		close(done)
	}()
	buf := make([]byte, 64<<10)
	for i := 0; i < 16; i++ {
		if _, err := c.Write(buf); err != nil {
			fail("%s write: %v", network, err)
		}
	}
	start := atomic.LoadInt64(&total)
	t0 := time.Now()
	for time.Since(t0) < d {
		if _, err := c.Write(buf); err != nil {
			fail("%s write: %v", network, err)
		}
	}
	got := atomic.LoadInt64(&total) - start
	el := time.Since(t0)
	c.Close()
	<-done
	return mbps(got, el)
}

func benchTCPRTT() (float64, string)        { return rtt("tcp", "127.0.0.1:0", 10_000) }
func benchTCPThroughput() (float64, string) { return throughput("tcp", "127.0.0.1:0", dur(2*time.Second)) }
func benchUnixRTT() (float64, string)       { return rtt("unix", "/tmp/bench-rtt.sock", 10_000) }
func benchUnixThroughput() (float64, string) {
	return throughput("unix", "/tmp/bench-tput.sock", dur(2*time.Second))
}

// --- threads and memory ---

// A goroutine that locks its thread and returns makes the runtime exit the
// M: one clone (for the replacement M) + one thread exit per iteration.
func benchThreadCreateExit() (float64, string) {
	return us(timeOps(count(2_000), func(n int) {
		done := make(chan struct{})
		for i := 0; i < n; i++ {
			go func() {
				runtime.LockOSThread()
				done <- struct{}{}
			}()
			<-done
		}
	}))
}

func benchMmap() (float64, string) {
	return us(timeOps(count(20_000), func(n int) {
		for i := 0; i < n; i++ {
			b, err := syscall.Mmap(-1, 0, 1<<20, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_PRIVATE|syscall.MAP_ANON)
			if err != nil {
				fail("mmap: %v", err)
			}
			b[4096] = 1
			if err := syscall.Munmap(b); err != nil {
				fail("munmap: %v", err)
			}
		}
	}))
}

// --- signals ---

func benchSignal() (float64, string) {
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGUSR1)
	defer signal.Stop(ch)
	pid := syscall.Getpid()
	return us(timeOps(count(10_000), func(n int) {
		for i := 0; i < n; i++ {
			if err := syscall.Kill(pid, syscall.SIGUSR1); err != nil {
				fail("kill: %v", err)
			}
			select {
			case <-ch:
			case <-time.After(5 * time.Second):
				fail("SIGUSR1 %d not delivered in 5 s", i)
			}
		}
	}))
}

func main() {
	only := flag.String("only", "", "comma-separated benchmark names to run")
	list := flag.Bool("list", false, "list benchmark names")
	flag.Parse()
	if *list {
		for _, b := range benches {
			fmt.Println(b.name)
		}
		return
	}
	want := map[string]bool{}
	for _, s := range strings.Split(*only, ",") {
		if s = strings.TrimSpace(s); s != "" {
			want[s] = true
		}
	}
	for name := range want {
		found := false
		for _, b := range benches {
			found = found || b.name == name
		}
		if !found {
			fail("unknown benchmark %q (try -list)", name)
		}
	}
	fmt.Printf("bench: %s/%s %s, GOMAXPROCS=%d, NumCPU=%d, -n %g\n",
		runtime.GOOS, runtime.GOARCH, runtime.Version(), runtime.GOMAXPROCS(0), runtime.NumCPU(), *scale)
	for _, b := range benches {
		if len(want) > 0 && !want[b.name] {
			continue
		}
		v, unit := b.run()
		prec := 1
		if unit == "us" && v < 10 {
			prec = 2
		}
		fmt.Printf("bench %s: %.*f %s\n", b.name, prec, v, unit)
	}
	fmt.Println("PASS bench")
}
