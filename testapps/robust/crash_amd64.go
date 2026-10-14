package main

import (
	"fmt"
	"runtime"
	"strings"
)

func udf()
func noncanonical() uint64
func setmxcsr(v uint32)
func getmxcsr() uint32
func divsd(a, b float64) float64

// crashSigbus: an unaligned LOCK XADD does not fault on x86 (no #AC with
// AC clear), so the amd64 SIGBUS case is not reachable from Go code.
func crashSigbus() { fail("-mode sigbus is arm64-only") }

func crashSigill() {
	udf()
	fail("UD2 did not fault")
}

// crashGP: a load from a non-canonical address raises #GP(0) → SIGSEGV
// SI_KERNEL (code=0x80) with si_addr 0, as Linux.
func crashGP() {
	fmt.Printf("robust: load from 0x8000000000000000\n")
	noncanonical()
	fail("non-canonical load did not fault")
}

// checkFltinv: unmask SSE invalid-operation exceptions, divide 0/0: #XM →
// SIGFPE FPE_FLTINV, which Go turns into a "floating point error" panic.
func checkFltinv() {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	old := getmxcsr()
	msg := func() (msg string) {
		defer func() {
			setmxcsr(old)
			msg = fmt.Sprint(recover())
		}()
		setmxcsr(old &^ (1 << 7)) // clear IM
		zero := 0.0
		divsd(zero, zero)
		return "no fault"
	}()
	if msg == "<nil>" {
		// QEMU TCG never raises #XM for unmasked SSE exceptions.
		fmt.Println("skip SIGFPE FPE_FLTINV: the CPU raised no #XM for 0/0 with IM clear (QEMU TCG)")
		return
	}
	if !strings.Contains(msg, "floating point error") {
		fail("0/0 with IM clear: %q, want a floating point error panic", msg)
	}
	ok("SIGFPE FPE_FLTINV recovered: %s", msg)
}
