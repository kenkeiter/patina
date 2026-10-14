// sigstress exercises signal delivery (design §15):
//
//   - async preemption: GOMAXPROCS goroutines spin in loops without
//     function calls; the main goroutine can only make progress if sysmon's
//     SIGURG preempts them (on GOMAXPROCS=1 it is otherwise starved);
//   - SIGSEGV: nil-pointer dereferences recovered as runtime panics;
//   - SIGFPE: integer division by zero recovered (x86 #DE; arm64 Go checks
//     explicitly and panics without a signal);
//   - signal.Notify(SIGUSR1) with kill(getpid()) from several goroutines and
//     tgkill to a locked thread, counting every receipt;
//   - syscall interruption: nanosleep → EINTR + rem, a blocking pipe read
//     restarted under SA_RESTART, timer sleeps under a signal storm.
//
// Modes:
//
//	sigstress               -> all of the above, prints "PASS sigstress", exit 0
//	sigstress -crash        -> unrecovered nil dereference (Go prints the
//	                           "[signal SIGSEGV ... code=0x1 addr=0x0 ...]" line, exit 2)
//	sigstress -breakpoint   -> runtime.Breakpoint() (SIGTRAP, Go's "SIGTRAP: trace trap" crash)
//
// Waits are busy loops on time.Now() with runtime.Gosched() (only the
// timer-sleep check uses time.Sleep).
package main

import (
	"flag"
	"fmt"
	"os"
	"os/signal"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

func fail(format string, a ...any) {
	fmt.Printf("FAIL sigstress: "+format+"\n", a...)
	os.Exit(1)
}

// waitFor busy-waits until cond() or the timeout.
func waitFor(d time.Duration, cond func() bool) bool {
	end := time.Now().Add(d)
	for !cond() {
		if time.Now().After(end) {
			return false
		}
		runtime.Gosched()
	}
	return true
}

var sink atomic.Uint64

// preempt runs spinners for d and counts how often main got the CPU.
func preempt(d time.Duration) {
	n := runtime.GOMAXPROCS(0)
	var stop atomic.Bool
	var wg sync.WaitGroup
	counts := make([]uint64, n)
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			var c uint64
			for !stop.Load() {
				c++
			}
			counts[i] = c
		}(i)
	}
	// With every P busy spinning, this loop only runs when a spinner is
	// asynchronously preempted.
	start := time.Now()
	turns := 0
	for time.Since(start) < d {
		turns++
		runtime.Gosched()
	}
	stop.Store(true)
	wg.Wait()
	var total uint64
	for _, c := range counts {
		total += c
	}
	sink.Add(total)
	fmt.Printf("sigstress: preemption: %d spinners, main ran %d turns in %v, spins %d\n", n, turns, d, total)
	if turns < 10 {
		fail("main starved: only %d turns", turns)
	}
	if total == 0 {
		fail("spinners never ran")
	}
}

//go:noinline
func deref(p *int) int { return *p }

func nilDeref() (msg string) {
	defer func() { msg = fmt.Sprint(recover()) }()
	var p *int
	return fmt.Sprint(deref(p))
}

//go:noinline
func div(a, b int) int { return a / b }

func divZero(b int) (msg string) {
	defer func() { msg = fmt.Sprint(recover()) }()
	return fmt.Sprint(div(7, b))
}

func faults() {
	for i := 0; i < 200; i++ {
		m := nilDeref()
		if !strings.Contains(m, "invalid memory address or nil pointer dereference") {
			fail("nil deref #%d: %q", i, m)
		}
		if i == 0 {
			fmt.Printf("sigstress: nil deref recovered: %s\n", m)
		}
	}
	for i := 0; i < 200; i++ {
		m := divZero(i - i)
		if !strings.Contains(m, "integer divide by zero") {
			fail("divide #%d: %q", i, m)
		}
		if i == 0 {
			fmt.Printf("sigstress: divide by zero recovered: %s\n", m)
		}
	}
}

func notify() {
	ch := make(chan os.Signal, 16)
	signal.Notify(ch, syscall.SIGUSR1)
	var got atomic.Int64
	go func() {
		for range ch {
			got.Add(1)
		}
	}()
	pid := os.Getpid()

	// kill(getpid(), SIGUSR1) from several goroutines. Standard signals
	// coalesce (in the kernel and in Go's sigsend), so each send waits for
	// its receipt; the lock serialises the handshakes.
	const senders, per = 4, 250
	var mu sync.Mutex
	var wg sync.WaitGroup
	var errs atomic.Int64
	for s := 0; s < senders; s++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < per; i++ {
				mu.Lock()
				before := got.Load()
				if err := syscall.Kill(pid, syscall.SIGUSR1); err != nil {
					errs.Add(1)
				}
				ok := waitFor(10*time.Second, func() bool { return got.Load() > before })
				mu.Unlock()
				if !ok {
					errs.Add(1)
					return
				}
			}
		}()
	}
	wg.Wait()
	if errs.Load() != 0 || got.Load() != senders*per {
		fail("kill: received %d of %d (errors %d)", got.Load(), senders*per, errs.Load())
	}
	fmt.Printf("sigstress: kill: %d/%d SIGUSR1 received\n", got.Load(), senders*per)

	// tgkill to a specific thread.
	const tg = 100
	done := make(chan error)
	go func() {
		runtime.LockOSThread()
		defer runtime.UnlockOSThread()
		tid := syscall.Gettid()
		for i := 0; i < tg; i++ {
			before := got.Load()
			if err := syscall.Tgkill(pid, tid, syscall.SIGUSR1); err != nil {
				done <- err
				return
			}
			if !waitFor(10*time.Second, func() bool { return got.Load() > before }) {
				done <- fmt.Errorf("tgkill #%d not received", i)
				return
			}
		}
		done <- nil
	}()
	if err := <-done; err != nil {
		fail("tgkill: %v", err)
	}
	fmt.Printf("sigstress: tgkill: %d/%d SIGUSR1 received\n", tg, tg)
	signal.Stop(ch)
}

// interrupts checks syscall interruption semantics with SIGUSR1 handled
// by Go (SA_RESTART): nanosleep returns EINTR with the remaining time
// regardless of SA_RESTART, a blocking pipe read is transparently
// restarted, and timer sleeps (epoll_pwait under the netpoller) survive a
// signal storm.
func interrupts() {
	ch := make(chan os.Signal, 64)
	signal.Notify(ch, syscall.SIGUSR1)
	defer signal.Stop(ch)
	go func() {
		for range ch {
		}
	}()
	pid := os.Getpid()

	// Run f on a locked thread; tgkill that thread every 50ms until f returns.
	onThread := func(f func() error) error {
		tidc := make(chan int)
		res := make(chan error, 1)
		go func() {
			runtime.LockOSThread()
			defer runtime.UnlockOSThread()
			tidc <- syscall.Gettid()
			res <- f()
		}()
		tid := <-tidc
		for {
			select {
			case err := <-res:
				return err
			default:
			}
			waitFor(50*time.Millisecond, func() bool { return false })
			syscall.Tgkill(pid, tid, syscall.SIGUSR1)
		}
	}

	// nanosleep: EINTR + rem.
	err := onThread(func() error {
		req := syscall.Timespec{Sec: 5}
		var rem syscall.Timespec
		start := time.Now()
		err := syscall.Nanosleep(&req, &rem)
		el := time.Since(start)
		if err != syscall.EINTR {
			return fmt.Errorf("nanosleep: got %v after %v, want EINTR", err, el)
		}
		left := time.Duration(rem.Nano())
		if left <= 0 || left > 5*time.Second || el+left < 4900*time.Millisecond {
			return fmt.Errorf("nanosleep: rem %v after %v", left, el)
		}
		fmt.Printf("sigstress: nanosleep interrupted after %v, rem %v\n", el.Round(time.Millisecond), left.Round(time.Millisecond))
		return nil
	})
	if err != nil {
		fail("%v", err)
	}

	// Blocking pipe read: restarted (SA_RESTART), returns the data.
	var p [2]int
	if err := syscall.Pipe(p[:]); err != nil {
		fail("pipe: %v", err)
	}
	go func() {
		waitFor(400*time.Millisecond, func() bool { return false })
		syscall.Write(p[1], []byte("ok"))
	}()
	err = onThread(func() error {
		buf := make([]byte, 8)
		n, err := syscall.Read(p[0], buf)
		if err != nil || string(buf[:n]) != "ok" {
			return fmt.Errorf("pipe read: %q, %v (want restart, not EINTR)", buf[:n], err)
		}
		return nil
	})
	if err != nil {
		fail("%v", err)
	}
	syscall.Close(p[0])
	syscall.Close(p[1])
	fmt.Println("sigstress: blocking pipe read restarted across signals")

	// Timer sleeps under a SIGUSR1 storm.
	stop := make(chan struct{})
	go func() {
		for {
			select {
			case <-stop:
				return
			default:
				syscall.Kill(pid, syscall.SIGUSR1)
				runtime.Gosched()
			}
		}
	}()
	start := time.Now()
	for i := 0; i < 20; i++ {
		time.Sleep(10 * time.Millisecond)
	}
	el := time.Since(start)
	close(stop)
	if el < 200*time.Millisecond {
		fail("20 x Sleep(10ms) took %v", el)
	}
	fmt.Printf("sigstress: 20 x time.Sleep(10ms) under SIGUSR1 storm: %v\n", el.Round(time.Millisecond))
}

func main() {
	crash := flag.Bool("crash", false, "unrecovered nil dereference")
	brk := flag.Bool("breakpoint", false, "runtime.Breakpoint()")
	procs := flag.Int("procs", 0, "GOMAXPROCS (0 = default)")
	dur := flag.Duration("preempt", 2*time.Second, "preemption test duration")
	flag.Parse()
	if *procs > 0 {
		runtime.GOMAXPROCS(*procs)
	}
	fmt.Printf("sigstress: GOMAXPROCS=%d\n", runtime.GOMAXPROCS(0))

	switch {
	case *crash:
		fmt.Println("sigstress: unrecovered nil dereference (expect Go crash, exit 2)")
		var p *int
		fmt.Println(deref(p))
		fail("survived a nil dereference")
	case *brk:
		fmt.Println("sigstress: runtime.Breakpoint (expect SIGTRAP crash)")
		runtime.Breakpoint()
		fail("survived runtime.Breakpoint")
	}

	preempt(*dur)
	faults()
	notify()
	interrupts()
	// Faults and preemption again with signal traffic behind us.
	preempt(*dur / 2)
	fmt.Println("PASS sigstress")
}
