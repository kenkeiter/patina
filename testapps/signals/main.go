// signals tests SIGTERM delivery.
//
//	signals                 -> signal.Notify(SIGTERM), wait; on SIGTERM print PASS, exit 0
//	signals -mode=nohandler -> no Notify; SIGTERM must kill the process with status 143
//	signals -mode=ignore    -> signal.Ignore(SIGTERM), wait (only a forced stop ends it)
//	signals -mode=usr1      -> print "signals: SIGUSR1 at <unix secs>" per SIGUSR1; exit 0 on SIGTERM
//	signals -enosys        -> first make syscall 9999 (expect ENOSYS; exercises the kernel log event)
//	signals -self           -> send SIGTERM to itself (kill(getpid())) instead of waiting for the host
//
// Waiting is a busy-wait on time.Now() with runtime.Gosched() rather than
// time.Sleep/time.After, so the program does not need Go's netpoller
// (epoll/eventfd), which Go initialises on first timer use.
package main

import (
	"flag"
	"fmt"
	"os"
	"os/signal"
	"runtime"
	"syscall"
	"time"
)

// spin waits for d without timers.
func spin(d time.Duration) {
	end := time.Now().Add(d)
	for time.Now().Before(end) {
		runtime.Gosched()
	}
}

func main() {
	mode := flag.String("mode", "handler", "handler | nohandler | ignore | usr1")
	self := flag.Bool("self", false, "raise SIGTERM ourselves after a short delay")
	timeout := flag.Duration("timeout", 5*time.Minute, "give up waiting after this long")
	enosys := flag.Bool("enosys", false, "first make an unknown syscall (9999), which must fail with ENOSYS")
	flag.Parse()

	if *enosys {
		_, _, e := syscall.Syscall(9999, 0, 0, 0)
		fmt.Printf("signals: syscall 9999: %v\n", e)
	}

	raise := func() {
		if *self {
			spin(100 * time.Millisecond)
			fmt.Println("signals: sending SIGTERM to self")
			if err := syscall.Kill(os.Getpid(), syscall.SIGTERM); err != nil {
				fmt.Printf("FAIL signals: kill: %v\n", err)
				os.Exit(1)
			}
		}
	}

	switch *mode {
	case "handler":
		ch := make(chan os.Signal, 1)
		signal.Notify(ch, syscall.SIGTERM)
		fmt.Println("signals: ready, waiting for SIGTERM (handler installed)")
		go raise()
		deadline := time.Now().Add(*timeout)
		for time.Now().Before(deadline) {
			select {
			case s := <-ch:
				fmt.Printf("signals: got %v\n", s)
				fmt.Println("PASS signals (handler)")
				os.Exit(0)
			default:
				runtime.Gosched()
			}
		}
		fmt.Println("FAIL signals: no SIGTERM received")
		os.Exit(1)
	case "nohandler":
		// Go's runtime handler resets SIG_DFL and re-raises: the process
		// must die with 128+15. Reaching the timeout means it did not.
		fmt.Println("signals: ready, waiting for SIGTERM (no handler; expect exit 143)")
		go raise()
		spin(*timeout)
		fmt.Println("FAIL signals: still alive, SIGTERM did not terminate the process")
		os.Exit(1)
	case "ignore":
		signal.Ignore(syscall.SIGTERM)
		fmt.Println("signals: ready, ignoring SIGTERM")
		go raise()
		spin(*timeout)
		fmt.Println("signals: timeout, still alive")
		os.Exit(1)
	case "usr1":
		ch := make(chan os.Signal, 4)
		signal.Notify(ch, syscall.SIGUSR1, syscall.SIGTERM)
		fmt.Println("signals: ready, waiting for SIGUSR1 / SIGTERM")
		deadline := time.Now().Add(*timeout)
		for time.Now().Before(deadline) {
			select {
			case s := <-ch:
				if s == syscall.SIGTERM {
					fmt.Println("signals: got SIGTERM, exiting")
					os.Exit(0)
				}
				fmt.Printf("signals: SIGUSR1 at %d\n", time.Now().Unix())
			default:
				runtime.Gosched()
			}
		}
		os.Exit(1)
	default:
		fmt.Printf("FAIL signals: unknown mode %q\n", *mode)
		os.Exit(2)
	}
}
