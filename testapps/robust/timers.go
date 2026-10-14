package main

import (
	"os"
	"os/signal"
	"runtime"
	"syscall"
	"time"
	"unsafe"
)

const (
	clockRealtime      = 0
	clockMonotonic     = 1
	clockProcessCPU    = 2
	clockThreadCPU     = 3
	timerAbstime       = 1
	sigevSignal        = 0
	sigevThreadID      = 4
	itimerReal         = 0
	itimerVirtual      = 1
	subTickPeriod      = 500 * time.Microsecond
	subTickRounds      = 100
	subTickMaxAvg      = 4 * time.Millisecond
	absSleepMaxLate    = 3 * time.Millisecond
	overrunBlockedTime = 20 * time.Millisecond
)

type sigevent struct {
	value  uint64
	signo  int32
	notify int32
	tid    int32
	_      [11]int32
}

type itimerspec struct {
	interval syscall.Timespec
	value    syscall.Timespec
}

type itimerval struct {
	interval syscall.Timeval
	value    syscall.Timeval
}

func errnoOf(e syscall.Errno) error {
	if e == 0 {
		return nil
	}
	return e
}

func timerCreate(clock int, ev *sigevent) (int32, error) {
	var id int32
	_, _, e := syscall.RawSyscall(syscall.SYS_TIMER_CREATE, uintptr(clock),
		uintptr(unsafe.Pointer(ev)), uintptr(unsafe.Pointer(&id)))
	return id, errnoOf(e)
}

func timerSettime(id int32, flags int, interval, value time.Duration) error {
	its := itimerspec{
		interval: syscall.NsecToTimespec(int64(interval)),
		value:    syscall.NsecToTimespec(int64(value)),
	}
	_, _, e := syscall.RawSyscall6(syscall.SYS_TIMER_SETTIME, uintptr(id), uintptr(flags),
		uintptr(unsafe.Pointer(&its)), 0, 0, 0)
	return errnoOf(e)
}

func timerGetoverrun(id int32) (int, error) {
	r, _, e := syscall.RawSyscall(syscall.SYS_TIMER_GETOVERRUN, uintptr(id), 0, 0)
	return int(r), errnoOf(e)
}

func timerDelete(id int32) {
	syscall.RawSyscall(syscall.SYS_TIMER_DELETE, uintptr(id), 0, 0)
}

func setitimer(which int, interval, value time.Duration) error {
	v := itimerval{
		interval: syscall.NsecToTimeval(int64(interval)),
		value:    syscall.NsecToTimeval(int64(value)),
	}
	_, _, e := syscall.RawSyscall(syscall.SYS_SETITIMER, uintptr(which),
		uintptr(unsafe.Pointer(&v)), 0)
	return errnoOf(e)
}

func clockGettime(clock int) time.Duration {
	var ts syscall.Timespec
	syscall.RawSyscall(syscall.SYS_CLOCK_GETTIME, uintptr(clock), uintptr(unsafe.Pointer(&ts)), 0)
	return time.Duration(ts.Nano())
}

func clockNanosleep(clock, flags int, d time.Duration) error {
	ts := syscall.NsecToTimespec(int64(d))
	_, _, e := syscall.Syscall6(syscall.SYS_CLOCK_NANOSLEEP, uintptr(clock), uintptr(flags),
		uintptr(unsafe.Pointer(&ts)), 0, 0, 0)
	return errnoOf(e)
}

// sigmask blocks (how=0) or unblocks (how=1) signals on the calling thread.
func sigmask(how int, sigs ...syscall.Signal) {
	var set uint64
	for _, s := range sigs {
		set |= 1 << (uint(s) - 1)
	}
	_, _, e := syscall.RawSyscall6(syscall.SYS_RT_SIGPROCMASK, uintptr(how),
		uintptr(unsafe.Pointer(&set)), 0, 8, 0, 0)
	if e != 0 {
		fail("rt_sigprocmask: %v", e)
	}
}

func sigpending() uint64 {
	var set uint64
	_, _, e := syscall.RawSyscall(syscall.SYS_RT_SIGPENDING, uintptr(unsafe.Pointer(&set)), 8, 0)
	if e != 0 {
		fail("rt_sigpending: %v", e)
	}
	return set
}

// subTick runs `rounds` sequential one-shot expiries of `arm` (500 µs)
// and returns the total and the shortest single wait.
func subTick(ch chan os.Signal, arm func() error) (total, minWait, maxWait time.Duration) {
	minWait = time.Hour
	for i := 0; i < subTickRounds; i++ {
		t0 := time.Now()
		if err := arm(); err != nil {
			fail("arm: %v", err)
		}
		select {
		case <-ch:
		case <-time.After(2 * time.Second):
			fail("timer signal %d/%d never arrived", i, subTickRounds)
		}
		d := time.Since(t0)
		total += d
		minWait = min(minWait, d)
		maxWait = max(maxWait, d)
	}
	return
}

func checkSubTick(what string, total, minWait, maxWait time.Duration) {
	if minWait < subTickPeriod {
		fail("%s fired early: %v < %v", what, minWait, subTickPeriod)
	}
	if avg := total / subTickRounds; avg > subTickMaxAvg {
		fail("%s not sub-tick: average %v per 500us expiry", what, avg)
	}
	ok("%s 500us x%d in %v (min %v, max %v)", what, subTickRounds,
		total.Round(time.Microsecond), minWait.Round(time.Microsecond), maxWait.Round(time.Microsecond))
}

func checkTimers() {
	// timer_settime on CLOCK_MONOTONIC, process-directed SIGUSR2.
	ch := make(chan os.Signal, 16)
	signal.Notify(ch, syscall.SIGUSR2)
	id, err := timerCreate(clockMonotonic, &sigevent{signo: int32(syscall.SIGUSR2), notify: sigevSignal})
	if err != nil {
		fail("timer_create: %v", err)
	}
	total, mn, mx := subTick(ch, func() error { return timerSettime(id, 0, 0, subTickPeriod) })
	checkSubTick("timer_settime", total, mn, mx)
	timerDelete(id)
	// Absolute expiry on a CLOCK_REALTIME timer.
	id, err = timerCreate(clockRealtime, &sigevent{signo: int32(syscall.SIGUSR2), notify: sigevSignal})
	if err != nil {
		fail("timer_create(CLOCK_REALTIME): %v", err)
	}
	total, mn, mx = subTick(ch, func() error {
		return timerSettime(id, timerAbstime, 0, clockGettime(clockRealtime)+subTickPeriod)
	})
	checkSubTick("timer_settime abs realtime", total, mn, mx)
	timerDelete(id)
	signal.Stop(ch)

	// setitimer(ITIMER_REAL) → SIGALRM.
	ach := make(chan os.Signal, 16)
	signal.Notify(ach, syscall.SIGALRM)
	total, mn, mx = subTick(ach, func() error { return setitimer(itimerReal, 0, subTickPeriod) })
	checkSubTick("setitimer ITIMER_REAL", total, mn, mx)
	signal.Stop(ach)

	// clock_nanosleep TIMER_ABSTIME on MONOTONIC and REALTIME: never
	// early, sub-tick late at most.
	for _, c := range []struct {
		name  string
		clock int
	}{{"monotonic", clockMonotonic}, {"realtime", clockRealtime}} {
		var worst, sum time.Duration
		const n = 50
		for i := 0; i < n; i++ {
			target := clockGettime(c.clock) + subTickPeriod
			if err := clockNanosleep(c.clock, timerAbstime, target); err != nil {
				fail("clock_nanosleep abs %s: %v", c.name, err)
			}
			late := clockGettime(c.clock) - target
			if late < 0 {
				fail("clock_nanosleep abs %s returned %v early", c.name, -late)
			}
			worst = max(worst, late)
			sum += late
		}
		// The average must be sub-tick; a single wakeup may be late under
		// host load (TCG).
		if sum/n > absSleepMaxLate {
			fail("clock_nanosleep abs %s: %v late on average (worst %v)", c.name, sum/n, worst)
		}
		ok("abs nanosleep %s: %d x 500us, never early, %v late on average, worst %v", c.name, n,
			(sum / n).Round(time.Microsecond), worst.Round(time.Microsecond))
	}
	// A past absolute deadline returns at once.
	if err := clockNanosleep(clockRealtime, timerAbstime, clockGettime(clockRealtime)-time.Second); err != nil {
		fail("clock_nanosleep past deadline: %v", err)
	}
	// CPU clocks: thread clock EINVAL, process clock ENOTSUP (Linux
	// supports the latter; documented deviation).
	if err := clockNanosleep(clockThreadCPU, 0, time.Millisecond); err != syscall.EINVAL {
		fail("clock_nanosleep(CLOCK_THREAD_CPUTIME_ID) = %v, want EINVAL", err)
	}
	ok("clock_nanosleep: past abs deadline returns at once, thread CPU clock EINVAL")

	checkOverrun()
	checkItimerVirtual()
	checkAlarm()
}

// checkOverrun: a 2 ms periodic timer aimed (SIGEV_THREAD_ID) at a
// thread that blocks the signal: one signal stays pending (rt_sigpending
// shows it), the other expiries accumulate as overrun, carried by the
// next signal queued after the pending one was taken (patina reports the
// overrun of the last *queued* signal; Linux that of the last delivered
// one). An expiry inside the short unblocked window queues a signal with
// the overrun that is delivered at once, and the next one carries ~0, so
// the check retries.
func checkOverrun() {
	ch := make(chan os.Signal, 64)
	signal.Notify(ch, syscall.SIGUSR2)
	defer signal.Stop(ch)
	const period = 2 * time.Millisecond
	best := 0
	for attempt := 0; attempt < 3 && best < 10; attempt++ {
		done := make(chan struct{})
		go func() {
			defer close(done)
			runtime.LockOSThread()
			defer runtime.UnlockOSThread()
			sigmask(0, syscall.SIGUSR2)
			id, err := timerCreate(clockMonotonic, &sigevent{
				signo: int32(syscall.SIGUSR2), notify: sigevThreadID, tid: int32(syscall.Gettid()),
			})
			if err != nil {
				fail("timer_create SIGEV_THREAD_ID: %v", err)
			}
			if err := timerSettime(id, 0, period, period); err != nil {
				fail("timer_settime periodic: %v", err)
			}
			ts := syscall.NsecToTimespec(int64(2 * overrunBlockedTime))
			syscall.Nanosleep(&ts, nil)
			if p := sigpending(); p&(1<<(syscall.SIGUSR2-1)) == 0 {
				fail("rt_sigpending = %#x: SIGUSR2 not pending", p)
			}
			// Take the pending one (Go's handler) and block again: the
			// next expiry queues a signal carrying the overrun.
			sigmask(1, syscall.SIGUSR2)
			sigmask(0, syscall.SIGUSR2)
			ts = syscall.NsecToTimespec(int64(3 * period))
			syscall.Nanosleep(&ts, nil)
			over, err := timerGetoverrun(id)
			if err != nil {
				fail("timer_getoverrun: %v", err)
			}
			timerDelete(id)
			best = max(best, over)
			sigmask(1, syscall.SIGUSR2)
		}()
		<-done
	}
	if best < 10 {
		fail("timer_getoverrun = %d after %v blocked at %v", best, 2*overrunBlockedTime, period)
	}
	ok("rt_sigpending shows a blocked timer signal; timer_getoverrun %d after %v blocked at %v",
		best, 2*overrunBlockedTime, period)
	select {
	case <-ch:
	case <-time.After(2 * time.Second):
		fail("blocked SIGUSR2 never delivered after unblocking")
	}
}

// checkItimerVirtual: ITIMER_VIRTUAL counts CPU time, so a spinning
// thread gets SIGVTALRM after ~20 ms of CPU.
func checkItimerVirtual() {
	ch := make(chan os.Signal, 4)
	signal.Notify(ch, syscall.SIGVTALRM)
	defer signal.Stop(ch)
	c0 := clockGettime(clockProcessCPU)
	if err := setitimer(itimerVirtual, 0, 20*time.Millisecond); err != nil {
		fail("setitimer(ITIMER_VIRTUAL): %v", err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		select {
		case <-ch:
			used := clockGettime(clockProcessCPU) - c0
			if used < 20*time.Millisecond {
				fail("SIGVTALRM after only %v of CPU", used)
			}
			ok("ITIMER_VIRTUAL: SIGVTALRM after %v of process CPU time", used.Round(time.Millisecond))
			return
		default:
		}
		if time.Now().After(deadline) {
			fail("no SIGVTALRM after 5s of spinning")
		}
		spin(time.Millisecond)
	}
}

var spinSink uint64

func spin(d time.Duration) {
	end := time.Now().Add(d)
	for time.Now().Before(end) {
		for i := 0; i < 1000; i++ {
			spinSink += uint64(i)
		}
	}
}
