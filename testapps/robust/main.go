// robust exercises Phase 5 robustness paths (scripts/test-robust.sh):
// tickless idle, sub-tick timers, fault → signal mapping, kernel stack and
// page-table reclaim, out-of-memory behaviour and smaller signal / fd
// edge cases. Every check prints one "ok <check>" line; the default mode
// ends with "PASS robust" and exit 0.
//
// Modes (-mode):
//
//	(none)    every non-fatal check, "PASS robust"
//	idle      time.Sleep(1s) precision, then sleep 3 s with nothing to do
//	          (run with patina.stats=1: the kernel prints per-CPU timer
//	          interrupt counts at exit)
//	reclaim   kernel stack and page-table frame reclaim
//	kheap     kernel heap shrink: bursts of pipes, sockets, epolls and
//	          tmpfs pages; free RAM returns to baseline once closed
//	sigbus    unaligned atomic (arm64: SIGBUS BUS_ADRALN), exit 2
//	sigill    undefined instruction (SIGILL), exit 2
//	gp        amd64: non-canonical access (#GP → SIGSEGV SI_KERNEL), exit 2
//	accerr    write to a PROT_READ mapping (SIGSEGV SEGV_ACCERR), exit 2
//	oom       allocate until the runtime runs out of memory, exit 2
//	sigkill   kill(getpid(), SIGKILL), exit 137
//	sigpipe   kill(getpid(), SIGPIPE) with the default action, exit 141
package main

import (
	"flag"
	"fmt"
	"os"
	"runtime"
	"time"
)

func fail(format string, a ...any) {
	fmt.Printf("FAIL robust: "+format+"\n", a...)
	os.Exit(1)
}

func ok(format string, a ...any) {
	fmt.Printf("ok "+format+"\n", a...)
}

func main() {
	mode := flag.String("mode", "", "sub-mode (see the package doc)")
	flag.Parse()
	fmt.Printf("robust: mode=%q GOMAXPROCS=%d NumCPU=%d GOARCH=%s\n",
		*mode, runtime.GOMAXPROCS(0), runtime.NumCPU(), runtime.GOARCH)
	switch *mode {
	case "":
		checkTimers()
		checkFltinv()
		checkSignals()
		checkFds()
		checkRusage()
	case "idle":
		checkIdle()
	case "restart":
		checkRestartSyscall()
	case "reclaim":
		checkReclaim()
	case "kheap":
		checkKheap()
	case "sigbus":
		crashSigbus()
	case "sigill":
		crashSigill()
	case "gp":
		crashGP()
	case "accerr":
		crashAccerr()
	case "oom":
		runOOM()
	case "oomfault":
		runOOMFault()
	case "sigkill":
		selfKill(9)
	case "sigpipe":
		selfKill(13)
	default:
		fail("unknown mode %q", *mode)
	}
	fmt.Println("PASS robust")
}

// checkIdle: a 1 s sleep on an otherwise idle VM returns within
// [1.0, 1.05] s, then the program idles 3 s (the script checks the
// kernel's per-CPU timer interrupt counts).
func checkIdle() {
	for i := 0; i < 3; i++ {
		t0 := time.Now()
		time.Sleep(time.Second)
		d := time.Since(t0)
		if d < time.Second || d > 1050*time.Millisecond {
			fail("time.Sleep(1s) took %v", d)
		}
		ok("time.Sleep(1s) %v", d.Round(10*time.Microsecond))
	}
	t0 := time.Now()
	time.Sleep(3 * time.Second)
	ok("idle 3s (%v)", time.Since(t0).Round(time.Millisecond))
}
