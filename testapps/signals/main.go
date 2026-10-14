// signals tests SIGTERM delivery.
//
//	signals                 -> signal.Notify(SIGTERM), wait; on SIGTERM print PASS, exit 0
//	signals -mode=nohandler -> no Notify; SIGTERM must kill the process with status 143
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
	mode := flag.String("mode", "handler", "handler | nohandler")
	self := flag.Bool("self", false, "raise SIGTERM ourselves after a short delay")
	timeout := flag.Duration("timeout", 5*time.Minute, "give up waiting after this long")
	flag.Parse()

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
	default:
		fmt.Printf("FAIL signals: unknown mode %q\n", *mode)
		os.Exit(2)
	}
}
