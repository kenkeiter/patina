// pproftest exercises timers and CPU profiling (design §6, §15):
//
//   - runtime/pprof CPU profile: 2 goroutines burn CPU for ~1 s; the gzipped
//     profile.proto is decoded by hand and must hold > 50 samples with a
//     period of 10 ms (100 Hz). Go arms a per-thread
//     timer_create(CLOCK_THREAD_CPUTIME_ID, SIGEV_THREAD_ID, SIGPROF) per M
//     plus setitimer(ITIMER_PROF), and drops SIGPROFs whose si_code does
//     not match (validSIGPROF);
//   - setitimer(ITIMER_REAL) / getitimer: one SIGALRM via signal.Notify;
//   - raw timer_create(CLOCK_MONOTONIC, SIGEV_THREAD_ID -> SIGUSR1) on a
//     locked thread, periodic 5 ms for 100 ms: >= 5 deliveries,
//     timer_gettime, timer_getoverrun, and timer_delete stops deliveries;
//   - CLOCK_THREAD_CPUTIME_ID of a spinning and of a sleeping thread
//     (spinning >= 100 ms, sleeping < 50 ms over 200 ms; -cputime=false
//     only prints the values).
//
// Prints "PASS pproftest" and exits 0.
package main

import (
	"bytes"
	"compress/gzip"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"runtime"
	"runtime/pprof"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unsafe"
)

var checkCPUTime = flag.Bool("cputime", true, "assert per-thread CPU time accounting")

func fail(format string, a ...any) {
	fmt.Printf("FAIL pproftest: "+format+"\n", a...)
	os.Exit(1)
}

// ---------------------------------------------------------------------------
// Minimal protobuf reader for profile.proto.

func varint(b []byte) (uint64, int, error) {
	var v uint64
	for i := 0; i < len(b) && i < 10; i++ {
		v |= uint64(b[i]&0x7f) << (7 * i)
		if b[i] < 0x80 {
			return v, i + 1, nil
		}
	}
	return 0, 0, errors.New("bad varint")
}

type field struct {
	num  int
	wire int
	v    uint64 // varint / fixed
	data []byte // length-delimited
}

func fields(b []byte) ([]field, error) {
	var out []field
	for len(b) > 0 {
		key, n, err := varint(b)
		if err != nil {
			return nil, err
		}
		b = b[n:]
		f := field{num: int(key >> 3), wire: int(key & 7)}
		switch f.wire {
		case 0:
			f.v, n, err = varint(b)
			if err != nil {
				return nil, err
			}
			b = b[n:]
		case 1:
			if len(b) < 8 {
				return nil, errors.New("short fixed64")
			}
			b = b[8:]
		case 2:
			l, n, err := varint(b)
			if err != nil || uint64(len(b)-n) < l {
				return nil, errors.New("bad length")
			}
			f.data = b[n : n+int(l)]
			b = b[n+int(l):]
		case 5:
			if len(b) < 4 {
				return nil, errors.New("short fixed32")
			}
			b = b[4:]
		default:
			return nil, fmt.Errorf("wire type %d", f.wire)
		}
		out = append(out, f)
	}
	return out, nil
}

// sampleValues returns field 2 (value) of a Sample, packed or not.
func sampleValues(b []byte) ([]int64, error) {
	fs, err := fields(b)
	if err != nil {
		return nil, err
	}
	var vals []int64
	for _, f := range fs {
		if f.num != 2 {
			continue
		}
		if f.wire == 0 {
			vals = append(vals, int64(f.v))
			continue
		}
		for d := f.data; len(d) > 0; {
			v, n, err := varint(d)
			if err != nil {
				return nil, err
			}
			vals = append(vals, int64(v))
			d = d[n:]
		}
	}
	return vals, nil
}

// decodeProfile returns the number of samples (sum of each sample's first
// value), the number of Sample messages and the period.
func decodeProfile(gz []byte) (samples, records int64, period int64, err error) {
	zr, err := gzip.NewReader(bytes.NewReader(gz))
	if err != nil {
		return 0, 0, 0, err
	}
	raw, err := io.ReadAll(zr)
	if err != nil {
		return 0, 0, 0, err
	}
	fs, err := fields(raw)
	if err != nil {
		return 0, 0, 0, err
	}
	for _, f := range fs {
		switch f.num {
		case 2: // sample
			vals, err := sampleValues(f.data)
			if err != nil {
				return 0, 0, 0, err
			}
			records++
			if len(vals) > 0 {
				samples += vals[0]
			}
		case 12: // period
			period = int64(f.v)
		}
	}
	return samples, records, period, nil
}

// ---------------------------------------------------------------------------

var sink atomic.Uint64

func burn(d time.Duration) {
	end := time.Now().Add(d)
	x := uint64(1)
	for time.Now().Before(end) {
		for i := 0; i < 10000; i++ {
			x = x*6364136223846793005 + 1442695040888963407
		}
	}
	sink.Add(x)
}

func profileTest() {
	var buf bytes.Buffer
	if err := pprof.StartCPUProfile(&buf); err != nil {
		fail("StartCPUProfile: %v", err)
	}
	var wg sync.WaitGroup
	for range 2 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			burn(time.Second)
		}()
	}
	wg.Wait()
	pprof.StopCPUProfile()
	samples, records, period, err := decodeProfile(buf.Bytes())
	if err != nil {
		fail("decode profile (%d bytes): %v", buf.Len(), err)
	}
	fmt.Printf("pprof: %d bytes, %d samples in %d records, period %d ns, GOMAXPROCS=%d\n",
		buf.Len(), samples, records, period, runtime.GOMAXPROCS(0))
	if period != 10_000_000 {
		fail("profile period %d ns, want 10000000", period)
	}
	if samples <= 50 {
		fail("only %d profile samples for ~2 CPU-seconds at 100 Hz", samples)
	}
}

// ---------------------------------------------------------------------------
// Raw timer syscalls.

type timeval struct{ sec, usec int64 }
type itimerval struct{ interval, value timeval }
type timespec struct{ sec, nsec int64 }
type itimerspec struct{ interval, value timespec }
type sigevent struct {
	value  uint64
	signo  int32
	notify int32
	tid    int32
	_      [44]byte
}

const (
	itimerReal         = 0
	clockMonotonic     = 1
	clockThreadCPUTime = 3
	sigevThreadID      = 4
)

func setitimer(which int, v *itimerval, old *itimerval) syscall.Errno {
	_, _, e := syscall.Syscall(syscall.SYS_SETITIMER, uintptr(which),
		uintptr(unsafe.Pointer(v)), uintptr(unsafe.Pointer(old)))
	return e
}

func getitimer(which int, v *itimerval) syscall.Errno {
	_, _, e := syscall.Syscall(syscall.SYS_GETITIMER, uintptr(which), uintptr(unsafe.Pointer(v)), 0)
	return e
}

func itimerTest() {
	ch := make(chan os.Signal, 4)
	signal.Notify(ch, syscall.SIGALRM)
	defer signal.Stop(ch)
	v := itimerval{value: timeval{usec: 20_000}}
	if e := setitimer(itimerReal, &v, nil); e != 0 {
		fail("setitimer: %v", e)
	}
	var cur itimerval
	if e := getitimer(itimerReal, &cur); e != 0 {
		fail("getitimer: %v", e)
	}
	left := time.Duration(cur.value.sec)*time.Second + time.Duration(cur.value.usec)*time.Microsecond
	if left <= 0 || left > 20*time.Millisecond {
		fail("getitimer: %v left, want (0, 20ms]", left)
	}
	start := time.Now()
	select {
	case <-ch:
	case <-time.After(500 * time.Millisecond):
		fail("no SIGALRM within 500 ms of setitimer(ITIMER_REAL, 20ms)")
	}
	fmt.Printf("itimer: SIGALRM after %v (armed for 20ms, %v left at getitimer)\n",
		time.Since(start).Round(time.Millisecond), left)
	if e := getitimer(itimerReal, &cur); e != 0 || cur.value != (timeval{}) {
		fail("getitimer after expiry: %+v %v", cur, e)
	}
}

func posixTimerTest() {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	ch := make(chan os.Signal, 64)
	signal.Notify(ch, syscall.SIGUSR1)
	defer signal.Stop(ch)

	ev := sigevent{signo: int32(syscall.SIGUSR1), notify: sigevThreadID, tid: int32(syscall.Gettid())}
	var id int32 = -1
	_, _, e := syscall.Syscall(syscall.SYS_TIMER_CREATE, clockMonotonic,
		uintptr(unsafe.Pointer(&ev)), uintptr(unsafe.Pointer(&id)))
	if e != 0 {
		fail("timer_create: %v", e)
	}
	// A bad target thread is EINVAL.
	bad := ev
	bad.tid = 1 << 30
	var id2 int32
	if _, _, e := syscall.Syscall(syscall.SYS_TIMER_CREATE, clockMonotonic,
		uintptr(unsafe.Pointer(&bad)), uintptr(unsafe.Pointer(&id2))); e != syscall.EINVAL {
		fail("timer_create(bad tid) = %v, want EINVAL", e)
	}
	spec := itimerspec{interval: timespec{nsec: 5_000_000}, value: timespec{nsec: 5_000_000}}
	if _, _, e := syscall.Syscall6(syscall.SYS_TIMER_SETTIME, uintptr(id), 0,
		uintptr(unsafe.Pointer(&spec)), 0, 0, 0); e != 0 {
		fail("timer_settime: %v", e)
	}
	n := 0
	end := time.After(100 * time.Millisecond)
loop:
	for {
		select {
		case <-ch:
			n++
		case <-end:
			break loop
		}
	}
	var cur itimerspec
	if _, _, e := syscall.Syscall(syscall.SYS_TIMER_GETTIME, uintptr(id), uintptr(unsafe.Pointer(&cur)), 0); e != 0 {
		fail("timer_gettime: %v", e)
	}
	ov, _, e := syscall.Syscall(syscall.SYS_TIMER_GETOVERRUN, uintptr(id), 0, 0)
	if e != 0 || int(ov) < 0 {
		fail("timer_getoverrun = %d, %v", int(ov), e)
	}
	if _, _, e := syscall.Syscall(syscall.SYS_TIMER_DELETE, uintptr(id), 0, 0); e != 0 {
		fail("timer_delete: %v", e)
	}
	fmt.Printf("timer: id %d, %d SIGUSR1 in 100 ms (5 ms period), gettime value %dns interval %dns, overrun %d\n",
		id, n, cur.value.nsec, cur.interval.nsec, int(ov))
	if n < 5 {
		fail("only %d timer signals in 100 ms", n)
	}
	if cur.interval != spec.interval || cur.value.sec != 0 || cur.value.nsec <= 0 || cur.value.nsec > 5_000_000 {
		fail("timer_gettime = %+v", cur)
	}
	// After timer_delete: drain anything in flight, then expect silence.
	time.Sleep(20 * time.Millisecond)
	for len(ch) > 0 {
		<-ch
	}
	select {
	case <-ch:
		fail("SIGUSR1 after timer_delete")
	case <-time.After(60 * time.Millisecond):
	}
	if _, _, e := syscall.Syscall(syscall.SYS_TIMER_DELETE, uintptr(id), 0, 0); e != syscall.EINVAL {
		fail("second timer_delete = %v, want EINVAL", e)
	}
}

func threadCPU() time.Duration {
	var ts timespec
	_, _, e := syscall.Syscall(syscall.SYS_CLOCK_GETTIME, clockThreadCPUTime, uintptr(unsafe.Pointer(&ts)), 0)
	if e != 0 {
		fail("clock_gettime(CLOCK_THREAD_CPUTIME_ID): %v", e)
	}
	return time.Duration(ts.sec)*time.Second + time.Duration(ts.nsec)
}

func cpuTimeTest() {
	var spin, sleep time.Duration
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		runtime.LockOSThread()
		t0 := threadCPU()
		burn(200 * time.Millisecond)
		spin = threadCPU() - t0
	}()
	go func() {
		defer wg.Done()
		runtime.LockOSThread()
		t0 := threadCPU()
		time.Sleep(200 * time.Millisecond)
		sleep = threadCPU() - t0
	}()
	wg.Wait()
	fmt.Printf("cputime: spinning thread +%v, sleeping thread +%v over 200ms\n", spin, sleep)
	if *checkCPUTime {
		if spin < 100*time.Millisecond {
			fail("spinning thread's CPU clock advanced only %v", spin)
		}
		if sleep >= 50*time.Millisecond {
			fail("sleeping thread's CPU clock advanced %v", sleep)
		}
	}
}

func main() {
	flag.Parse()
	itimerTest()
	posixTimerTest()
	profileTest()
	cpuTimeTest()
	fmt.Println("PASS pproftest")
}
