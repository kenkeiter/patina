// sysfuzz hammers the kernel's syscall argument and user-memory validation
// (scripts/test-sysfuzz.sh). The guest program runs in user mode and must
// never be able to crash, hang or corrupt the kernel; this program tries.
//
// Modes (-mode):
//
//	cases     a deterministic list of nasty argument combinations, each
//	          checked against the errno Linux returns (cases.go; every
//	          expectation cites the Linux source that produces it). Prints
//	          "ok <case>" or "FAIL <case>: ..." and "PASS sysfuzz cases".
//	fuzz      -iters random syscalls with random and edge-case arguments
//	          (NULL, kernel-half and unmapped addresses, huge lengths,
//	          negative fds, random flags) from -procs goroutines on locked
//	          OS threads, seeded by -seed (fuzz.go). The only pass criterion
//	          is that the kernel stays alive and the program exits 0
//	          ("PASS sysfuzz fuzz").
//	all       cases, then fuzz.
//	exhaust   reserved (not implemented): kernel memory pressure.
//	sigreturn-* hand-crafted bad rt_sigreturn frames (crash.go): the
//	          program dies by a signal; the kernel must survive.
//
// The fuzzer never hands the kernel a pointer into Go-managed memory
// except its own dedicated arenas, and it excludes the syscalls whose only
// possible effect is to kill or wedge the program itself (see fuzz.go's
// exclusion list).
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

var (
	modeFlag     = flag.String("mode", "all", "cases|fuzz|all|exhaust|sigreturn-*")
	seedFlag     = flag.Int64("seed", 1, "fuzz seed")
	itersFlag    = flag.Int("iters", 20000, "total fuzz syscalls")
	procsFlag    = flag.Int("procs", 0, "fuzz goroutines (0: GOMAXPROCS)")
	vFlag        = flag.Bool("v", false, "log every fuzz call")
	skipFlag     = flag.String("skip", "", "skip a named case group (cases mode): mincore-huge")
	deadlineFlag = flag.Duration("deadline", 120*time.Second, "fuzz mode: fail (exit 3) if the workers are stuck past this")
)

func main() {
	flag.Parse()
	fmt.Printf("sysfuzz: mode=%s seed=%d iters=%d GOMAXPROCS=%d GOARCH=%s\n",
		*modeFlag, *seedFlag, *itersFlag, runtime.GOMAXPROCS(0), runtime.GOARCH)
	// Timers and itimers armed by the fuzzer signal the process; these
	// must not terminate it. SIGPIPE from writes to widowed pipes and
	// sockets is likewise expected.
	signal.Ignore(syscall.SIGALRM, syscall.SIGVTALRM, syscall.SIGPROF, syscall.SIGPIPE,
		syscall.SIGWINCH, syscall.SIGCHLD, syscall.SIGIO, syscall.SIGXFSZ, syscall.SIGXCPU,
		syscall.SIGUSR1, syscall.SIGUSR2, syscall.SIGHUP)
	// Make sure Go's netpoller (epoll fd, eventfd) exists before the
	// fuzzer records which fds it owns.
	time.Sleep(time.Millisecond)
	setupArena()

	switch *modeFlag {
	case "cases":
		runCases()
	case "fuzz":
		runFuzz()
	case "all":
		runCases()
		runFuzz()
	case "exhaust":
		fmt.Println("sysfuzz: exhaust mode is not implemented (finding 6 is covered by the kernel-side check)")
		os.Exit(2)
	default:
		if !runCrash(*modeFlag) {
			fmt.Printf("FAIL sysfuzz: unknown mode %q\n", *modeFlag)
			os.Exit(2)
		}
	}
	// The kernel is still answering: a last sanity round trip.
	if pid, _, _ := syscall.RawSyscall(syscall.SYS_GETPID, 0, 0, 0); pid == 0 {
		fmt.Println("FAIL sysfuzz: getpid returned 0")
		os.Exit(1)
	}
	fmt.Println("PASS sysfuzz")
}

// sc issues a raw syscall with up to six arguments.
func sc(nr uintptr, a ...uintptr) (uintptr, syscall.Errno) {
	var x [6]uintptr
	copy(x[:], a)
	r, _, e := syscall.Syscall6(nr, x[0], x[1], x[2], x[3], x[4], x[5])
	return r, e
}

// scr is sc without the scheduler hooks (for calls that never block).
func scr(nr uintptr, a ...uintptr) (uintptr, syscall.Errno) {
	var x [6]uintptr
	copy(x[:], a)
	r, _, e := syscall.RawSyscall6(nr, x[0], x[1], x[2], x[3], x[4], x[5])
	return r, e
}

// u turns a (possibly negative) int into a raw syscall argument.
func u(v int64) uintptr { return uintptr(v) }
