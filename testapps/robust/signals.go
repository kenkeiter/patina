package main

import (
	"errors"
	"os"
	"os/signal"
	"runtime"
	"syscall"
	"time"
	"unsafe"
)

type stackT struct {
	sp    uintptr
	flags uint32
	_     int32
	size  uintptr
}

const (
	ssDisable    = 2
	ssAutodisarm = 1 << 31
)

func sigaltstack(ss, old *stackT) error {
	_, _, e := syscall.RawSyscall(syscall.SYS_SIGALTSTACK, uintptr(unsafe.Pointer(ss)),
		uintptr(unsafe.Pointer(old)), 0)
	return errnoOf(e)
}

func tgkill(tid int, sig syscall.Signal) {
	syscall.RawSyscall(syscall.SYS_TGKILL, uintptr(syscall.Getpid()), uintptr(tid), uintptr(sig))
}

// onLockedThread runs f on a fresh goroutine locked to its OS thread,
// handing its tid to the caller first; returns once f returned.
func onLockedThread(f func()) (tid int, wait func()) {
	tidc := make(chan int)
	done := make(chan struct{})
	go func() {
		defer close(done)
		runtime.LockOSThread()
		defer runtime.UnlockOSThread()
		tidc <- syscall.Gettid()
		f()
	}()
	return <-tidc, func() { <-done }
}

func checkSignals() {
	checkSigaltstack()
	checkRestartSyscall()
	checkSigsuspend()
	checkEpollEINTR()
	checkSigpipeWriter()
	checkPause()
}

// checkSigaltstack: size below MINSIGSTKSZ → ENOMEM, bad flags → EINVAL,
// SS_AUTODISARM round trip, Go's own stack restored afterwards. (EPERM
// while running on the stack needs a handler of our own: not reachable
// from Go.)
func checkSigaltstack() {
	_, wait := onLockedThread(func() {
		var goStack stackT
		if err := sigaltstack(nil, &goStack); err != nil {
			fail("sigaltstack query: %v", err)
		}
		if goStack.flags&ssDisable != 0 || goStack.size == 0 {
			fail("Go's sigaltstack not installed: %+v", goStack)
		}
		buf := make([]byte, 64<<10)
		small := stackT{sp: uintptr(unsafe.Pointer(&buf[0])), size: 1024}
		if err := sigaltstack(&small, nil); err != syscall.ENOMEM {
			fail("sigaltstack(1 KiB) = %v, want ENOMEM", err)
		}
		bad := stackT{sp: uintptr(unsafe.Pointer(&buf[0])), size: uintptr(len(buf)), flags: 0x40}
		if err := sigaltstack(&bad, nil); err != syscall.EINVAL {
			fail("sigaltstack(flags 0x40) = %v, want EINVAL", err)
		}
		auto := stackT{sp: uintptr(unsafe.Pointer(&buf[0])), size: uintptr(len(buf)), flags: ssAutodisarm}
		if err := sigaltstack(&auto, nil); err != nil {
			fail("sigaltstack(SS_AUTODISARM): %v", err)
		}
		var got stackT
		sigaltstack(nil, &got)
		if got.flags&ssAutodisarm == 0 || got.sp != auto.sp || got.size != auto.size {
			fail("sigaltstack read back %+v after SS_AUTODISARM", got)
		}
		if err := sigaltstack(&goStack, nil); err != nil {
			fail("restoring Go's sigaltstack: %v", err)
		}
		runtime.KeepAlive(buf)
	})
	wait()
	ok("sigaltstack: ENOMEM below MINSIGSTKSZ, EINVAL for bad flags, SS_AUTODISARM read back")
}

// rawSigaction swaps sig's kernel disposition; returns the old one.
func rawSigaction(sig syscall.Signal, act *[4]uint64) [4]uint64 {
	var old [4]uint64
	_, _, e := syscall.RawSyscall6(syscall.SYS_RT_SIGACTION, uintptr(sig),
		uintptr(unsafe.Pointer(act)), uintptr(unsafe.Pointer(&old[0])), 8, 0, 0)
	if e != 0 {
		fail("rt_sigaction(%d): %v", sig, e)
	}
	return old
}

// checkRestartSyscall: a 200 ms nanosleep must sleep the full 200 ms
// across signals that run no handler.
//   - signal.Ignore(SIGUSR1) then tgkill: dropped at send (Linux
//     sig_ignored), the sleep is not even woken;
//   - tgkill while Go's handler is installed, then SIG_IGN before the
//     sleeper runs: the sleeper is woken, finds no handler to run and
//     continues through restart_syscall (on one vCPU the sender keeps the
//     CPU, so this path is the usual one; elsewhere the handler may win
//     and the sleep returns EINTR, which is resumed and reported).
func checkRestartSyscall() {
	sleep := func(d time.Duration) (time.Duration, int) {
		t0 := time.Now()
		ts := syscall.NsecToTimespec(int64(d))
		eintr := 0
		for {
			var rem syscall.Timespec
			_, _, e := syscall.Syscall(syscall.SYS_NANOSLEEP, uintptr(unsafe.Pointer(&ts)),
				uintptr(unsafe.Pointer(&rem)), 0)
			if e == syscall.EINTR {
				eintr++
				ts = rem
				continue
			}
			if e != 0 {
				fail("nanosleep: %v", e)
			}
			return time.Since(t0), eintr
		}
	}
	// 1: ignored at send.
	signal.Ignore(syscall.SIGUSR1)
	var d1 time.Duration
	var e1 int
	tid, wait := onLockedThread(func() { d1, e1 = sleep(200 * time.Millisecond) })
	time.Sleep(50 * time.Millisecond)
	for i := 0; i < 5; i++ {
		tgkill(tid, syscall.SIGUSR1)
	}
	wait()
	signal.Reset(syscall.SIGUSR1)
	if d1 < 200*time.Millisecond || e1 != 0 {
		fail("nanosleep(200ms) with ignored SIGUSR1: %v, %d EINTR", d1, e1)
	}
	// 2: woken, then no handler at delivery. Notify makes Go's handler the
	// disposition at send time (after Ignore+Reset it may still be
	// SIG_IGN).
	usr1 := make(chan os.Signal, 4)
	signal.Notify(usr1, syscall.SIGUSR1)
	defer signal.Reset(syscall.SIGUSR1)
	defer signal.Stop(usr1)
	var d2 time.Duration
	var e2 int
	tid, wait = onLockedThread(func() { d2, e2 = sleep(200 * time.Millisecond) })
	time.Sleep(50 * time.Millisecond)
	runtime.LockOSThread()
	ign := [4]uint64{1} // SIG_IGN
	goAct := [4]uint64{}
	{
		var cur [4]uint64
		_, _, e := syscall.RawSyscall6(syscall.SYS_RT_SIGACTION, uintptr(syscall.SIGUSR1), 0,
			uintptr(unsafe.Pointer(&cur[0])), 8, 0, 0)
		if e != 0 {
			fail("rt_sigaction query: %v", e)
		}
		goAct = cur
	}
	tgkill(tid, syscall.SIGUSR1)
	rawSigaction(syscall.SIGUSR1, &ign)
	runtime.UnlockOSThread()
	wait()
	rawSigaction(syscall.SIGUSR1, &goAct)
	if d2 < 200*time.Millisecond || d2 > 400*time.Millisecond {
		fail("nanosleep(200ms) woken by a signal that ran no handler: %v", d2)
	}
	ok("restart_syscall: nanosleep(200ms) took %v with an ignored SIGUSR1 (dropped at send), %v when woken with no handler to run (%d EINTR)",
		d1.Round(time.Millisecond), d2.Round(time.Millisecond), e2)
}

// checkSigsuspend: rt_sigsuspend with a mask that blocks nothing returns
// EINTR once a SIGUSR2 handler ran.
func checkSigsuspend() {
	ch := make(chan os.Signal, 4)
	signal.Notify(ch, syscall.SIGUSR2)
	defer signal.Stop(ch)
	var err error
	var took time.Duration
	tid, wait := onLockedThread(func() {
		var mask uint64
		t0 := time.Now()
		_, _, e := syscall.Syscall(syscall.SYS_RT_SIGSUSPEND, uintptr(unsafe.Pointer(&mask)), 8, 0)
		took = time.Since(t0)
		err = errnoOf(e)
	})
	time.Sleep(50 * time.Millisecond)
	tgkill(tid, syscall.SIGUSR2)
	wait()
	if err != syscall.EINTR || took < 40*time.Millisecond {
		fail("rt_sigsuspend = %v after %v, want EINTR after ~50ms", err, took)
	}
	select {
	case <-ch:
	case <-time.After(2 * time.Second):
		fail("SIGUSR2 that ended rt_sigsuspend never reached signal.Notify")
	}
	ok("rt_sigsuspend: EINTR after %v when SIGUSR2's handler ran", took.Round(time.Millisecond))
}

// checkEpollEINTR: epoll_pwait returns EINTR (never restarted, even under
// Go's SA_RESTART) when a handler runs; with a sigmask that blocks the
// signal it times out instead and the signal is delivered once the
// temporary mask is restored.
func checkEpollEINTR() {
	ch := make(chan os.Signal, 4)
	signal.Notify(ch, syscall.SIGUSR2)
	defer signal.Stop(ch)
	epfd, err := syscall.EpollCreate1(syscall.EPOLL_CLOEXEC)
	if err != nil {
		fail("epoll_create1: %v", err)
	}
	defer syscall.Close(epfd)
	pwait := func(timeoutMs int, mask *uint64) (time.Duration, error) {
		var evs [1]syscall.EpollEvent
		t0 := time.Now()
		_, _, e := syscall.Syscall6(syscall.SYS_EPOLL_PWAIT, uintptr(epfd),
			uintptr(unsafe.Pointer(&evs[0])), 1, uintptr(timeoutMs), uintptr(unsafe.Pointer(mask)), 8)
		return time.Since(t0), errnoOf(e)
	}
	var d1 time.Duration
	var e1 error
	tid, wait := onLockedThread(func() { d1, e1 = pwait(2000, nil) })
	time.Sleep(50 * time.Millisecond)
	tgkill(tid, syscall.SIGUSR2)
	wait()
	if e1 != syscall.EINTR || d1 > time.Second {
		fail("epoll_pwait interrupted: %v after %v, want EINTR", e1, d1)
	}
	<-ch
	var d2 time.Duration
	var e2 error
	blockUsr2 := uint64(1) << (syscall.SIGUSR2 - 1)
	tid, wait = onLockedThread(func() { d2, e2 = pwait(200, &blockUsr2) })
	time.Sleep(50 * time.Millisecond)
	tgkill(tid, syscall.SIGUSR2)
	wait()
	if e2 != nil || d2 < 200*time.Millisecond {
		fail("epoll_pwait with SIGUSR2 in its sigmask: %v after %v, want a 200ms timeout", e2, d2)
	}
	select {
	case <-ch:
	case <-time.After(2 * time.Second):
		fail("SIGUSR2 held by epoll_pwait's sigmask never delivered afterwards")
	}
	ok("epoll_pwait: EINTR after %v under SA_RESTART; temporary sigmask held SIGUSR2 for the %v timeout, delivered after",
		d1.Round(time.Millisecond), d2.Round(time.Millisecond))
}

// checkSigpipeWriter: a write to a pipe whose read end is closed returns
// EPIPE and raises SIGPIPE for the writer (Go delivers it to Notify).
func checkSigpipeWriter() {
	ch := make(chan os.Signal, 4)
	signal.Notify(ch, syscall.SIGPIPE)
	defer signal.Stop(ch)
	var p [2]int
	if err := syscall.Pipe2(p[:], syscall.O_CLOEXEC); err != nil {
		fail("pipe2: %v", err)
	}
	syscall.Close(p[0])
	_, err := syscall.Write(p[1], []byte("x"))
	syscall.Close(p[1])
	if !errors.Is(err, syscall.EPIPE) {
		fail("write to a widowed pipe: %v, want EPIPE", err)
	}
	select {
	case <-ch:
	case <-time.After(2 * time.Second):
		fail("no SIGPIPE for the EPIPE writer")
	}
	ok("SIGPIPE to the writer with EPIPE (fd %d)", p[1])
}

// checkRusage: getrusage/times CPU time grows while spinning.
func checkRusage() {
	var r0, r1, t0, t1 syscall.Rusage
	syscall.Getrusage(syscall.RUSAGE_SELF, &r0)
	var tms0, tms1 syscall.Tms
	ticks0, err := syscall.Times(&tms0)
	if err != nil {
		fail("times: %v", err)
	}
	_, wait := onLockedThread(func() {
		syscall.Getrusage(1, &t0) // RUSAGE_THREAD
		spin(100 * time.Millisecond)
		syscall.Getrusage(1, &t1)
	})
	wait()
	syscall.Getrusage(syscall.RUSAGE_SELF, &r1)
	ticks1, _ := syscall.Times(&tms1)
	self := time.Duration(r1.Utime.Nano()+r1.Stime.Nano()) - time.Duration(r0.Utime.Nano()+r0.Stime.Nano())
	thr := time.Duration(t1.Utime.Nano()+t1.Stime.Nano()) - time.Duration(t0.Utime.Nano()+t0.Stime.Nano())
	tms := time.Duration(tms1.Utime+tms1.Stime-tms0.Utime-tms0.Stime) * 10 * time.Millisecond
	if self < 80*time.Millisecond || thr < 80*time.Millisecond || tms < 70*time.Millisecond || ticks1 <= ticks0 {
		fail("CPU time over a 100ms spin: getrusage self %v thread %v, times %v (ticks %d -> %d)",
			self, thr, tms, ticks0, ticks1)
	}
	ok("getrusage: self +%v, thread +%v; times +%v, %d ticks elapsed over a 100ms spin",
		self.Round(time.Millisecond), thr.Round(time.Millisecond), tms, ticks1-ticks0)
}
