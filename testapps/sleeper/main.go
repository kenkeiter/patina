// sleeper measures timer behaviour from inside the guest: one long
// time.Sleep, many sub-tick sleeps, a ticker, and a cross-thread
// goroutine ping-pong, each timed with the monotonic clock. It also prints
// CLOCK_MONOTONIC at entry (the kernel's uptime: boot-to-main latency) and
// the total, so a host can compare guest-measured time with its own wall
// time. Prints "PASS sleeper".
//
//	sleeper [-sleep 1s] [-short 200] [-tick 200] [-pingpong 20000]
package main

import (
	"flag"
	"fmt"
	"runtime"
	"syscall"
	"time"
	"unsafe"
)

func monoNow() time.Duration {
	var ts syscall.Timespec
	syscall.RawSyscall(syscall.SYS_CLOCK_GETTIME, 1, uintptr(unsafe.Pointer(&ts)), 0)
	return time.Duration(ts.Nano())
}

func ms(d time.Duration) string { return fmt.Sprintf("%.3f ms", float64(d)/1e6) }

func main() {
	entry := monoNow()
	start := time.Now()
	sleep := flag.Duration("sleep", time.Second, "one long time.Sleep")
	short := flag.Int("short", 200, "number of time.Sleep(500µs)")
	tick := flag.Int("tick", 200, "ticks of a 1 ms ticker")
	pp := flag.Int("pingpong", 20000, "cross-thread channel round trips")
	flag.Parse()
	fmt.Printf("sleeper: GOMAXPROCS=%d uptime at main %s\n", runtime.GOMAXPROCS(0), ms(entry))

	t := time.Now()
	time.Sleep(*sleep)
	d := time.Since(t)
	fmt.Printf("sleeper: sleep %s took %s (overshoot %s)\n", *sleep, ms(d), ms(d-*sleep))

	var worst time.Duration
	t = time.Now()
	for i := 0; i < *short; i++ {
		s := time.Now()
		time.Sleep(500 * time.Microsecond)
		if e := time.Since(s); e > worst {
			worst = e
		}
	}
	d = time.Since(t)
	if *short > 0 {
		fmt.Printf("sleeper: %d x sleep 500us took %s (avg %s, worst %s)\n", *short, ms(d), ms(d/time.Duration(*short)), ms(worst))
	}

	t = time.Now()
	tk := time.NewTicker(time.Millisecond)
	for i := 0; i < *tick; i++ {
		<-tk.C
	}
	tk.Stop()
	d = time.Since(t)
	if *tick > 0 {
		fmt.Printf("sleeper: %d ticks of 1ms took %s (avg %s)\n", *tick, ms(d), ms(d/time.Duration(*tick)))
	}

	a, b := make(chan int), make(chan int)
	go func() {
		runtime.LockOSThread()
		for v := range a {
			b <- v + 1
		}
		close(b)
	}()
	runtime.LockOSThread()
	t = time.Now()
	for i := 0; i < *pp; i++ {
		a <- i
		<-b
	}
	close(a)
	d = time.Since(t)
	runtime.UnlockOSThread()
	if *pp > 0 {
		fmt.Printf("sleeper: %d ping-pongs across threads took %s (%.1f us/round trip)\n", *pp, ms(d), float64(d)/float64(*pp)/1e3)
	}

	fmt.Printf("sleeper: total in main %s, uptime at end %s\n", ms(time.Since(start)), ms(monoNow()))
	fmt.Println("PASS sleeper")
}
