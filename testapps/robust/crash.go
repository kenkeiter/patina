package main

import (
	"fmt"
	"syscall"
	"unsafe"
)

// crashAccerr: a store to a PROT_READ anonymous mapping → SIGSEGV
// SEGV_ACCERR (code=0x2) with si_addr = the address.
func crashAccerr() {
	b, err := syscall.Mmap(-1, 0, 4096, syscall.PROT_READ, syscall.MAP_PRIVATE|syscall.MAP_ANON)
	if err != nil {
		fail("mmap: %v", err)
	}
	_ = b[0] // the page is mapped (read fill)
	fmt.Printf("robust: read-only page %p\n", &b[0])
	b[8] = 1
	fail("store to a PROT_READ page did not fault")
}

// selfKill: kill(getpid(), sig) with the default action (SIGKILL cannot
// be caught; Go does not install a SIGPIPE handler that survives for a
// signal sent with kill to fd-less contexts: SIGPIPE's default action
// applies once Go re-raises it).
func selfKill(sig int) {
	if sig == int(syscall.SIGPIPE) {
		// Go catches SIGPIPE; for a signal not caused by a write to fd 1/2
		// it ignores it. Reset to SIG_DFL so the default action applies.
		resetDefault(syscall.SIGPIPE)
	}
	syscall.Kill(syscall.Getpid(), syscall.Signal(sig))
	// SIGKILL/SIGPIPE are process-directed; give delivery a moment.
	for i := 0; i < 100; i++ {
		syscall.Nanosleep(&syscall.Timespec{Nsec: 10_000_000}, nil)
	}
	fail("still alive after kill(getpid(), %d)", sig)
}

// resetDefault sets sig's disposition to SIG_DFL behind Go's back (the
// kernel sigaction layout is handler, flags, restorer, mask on both
// arches; all zero = SIG_DFL).
func resetDefault(sig syscall.Signal) {
	var act [4]uint64
	_, _, e := syscall.RawSyscall6(syscall.SYS_RT_SIGACTION, uintptr(sig),
		uintptr(unsafe.Pointer(&act[0])), 0, 8, 0, 0)
	if e != 0 {
		fail("rt_sigaction(%d, SIG_DFL): %v", sig, e)
	}
}
