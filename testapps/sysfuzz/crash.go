package main

// Crash modes: hand-crafted rt_sigreturn frames and a syscall from the
// last user page. Each one ends the program: by exit_group(42) when the
// kernel accepts (and sanitizes) the frame, otherwise by a signal. The
// script checks that the kernel survived (no panic / fatal dump, a normal
// exit sentinel) and, where Linux's outcome is certain, the exit code.

import (
	"fmt"
	"math/rand"
	"os"
	"syscall"
)

// Implemented in crash_<arch>.s.
func sigreturnAt(sp uintptr)
func exit42Addr() uintptr
func getpidExit42Addr() uintptr
func jumpTo(pc uintptr)

// frameBuf returns a zeroed, 64-aligned scratch frame inside the junk
// area, with room below it for the handler's stack use.
func frameBuf() uintptr {
	p := (rwBase + 512<<10) &^ 63
	clear(mem(p, 64<<10))
	return p
}

func runCrash(mode string) bool {
	if mode == "lastpage" {
		crashLastPage()
		return true
	}
	if mode == "sigreturn-garbage" {
		r := rand.New(rand.NewSource(*seedFlag))
		p := frameBuf()
		r.Read(mem(p, 8192))
		fmt.Println("sysfuzz: rt_sigreturn on random bytes")
		sigreturnAt(p)
		return true
	}
	sp, ok := archFrame(mode)
	if !ok {
		return false
	}
	fmt.Printf("sysfuzz: %s: rt_sigreturn at %#x\n", mode, sp)
	sigreturnAt(sp)
	fmt.Println("FAIL sysfuzz: rt_sigreturn returned")
	os.Exit(1)
	return true
}

// crashLastPage maps the highest user page and runs a syscall that ends
// exactly at the end of user space, so the return address is the first
// non-user address (x86_64: non-canonical; the kernel must not sysret
// there). Linux refuses the mapping on x86_64 (TASK_SIZE excludes the
// last page); either outcome is fine as long as the kernel survives.
func crashLastPage() {
	const last = userEnd - pageSize
	r, e := scr(SYS_MMAP, last, pageSize, rw, anon|mapFixed, allOnes, 0)
	if e != 0 {
		fmt.Printf("sysfuzz: lastpage: mmap refused (%v)\n", e)
		os.Exit(43)
	}
	code := lastPageCode()
	copy(mem(r+pageSize-uintptr(len(code)), len(code)), code)
	if _, e := scr(SYS_MPROTECT, r, pageSize, syscall.PROT_READ|syscall.PROT_EXEC); e != 0 {
		fmt.Printf("FAIL sysfuzz: lastpage mprotect: %v\n", e)
		os.Exit(1)
	}
	fmt.Println("sysfuzz: lastpage: jumping to the syscall at the end of user space")
	jumpTo(r + pageSize - uintptr(len(code)))
}
