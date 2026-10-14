package main

import (
	"fmt"
	"unsafe"
)

func udf()
func ldaxr(p *uint64) uint64

// crashSigbus: an exclusive load from an unaligned address takes an
// alignment fault → SIGBUS BUS_ADRALN with si_addr = the address. Go's
// crash line: [signal SIGBUS: bus error code=0x1 addr=<p> pc=...].
func crashSigbus() {
	buf := make([]uint64, 4)
	// +15: crosses a 16-byte granule, so FEAT_LSE2's relaxations cannot
	// apply.
	p := unsafe.Add(unsafe.Pointer(&buf[0]), 15)
	fmt.Printf("robust: unaligned addr %#x\n", uintptr(p))
	ldaxr((*uint64)(p))
	fail("unaligned LDAXR did not fault")
}

func crashSigill() {
	udf()
	fail("UDF did not fault")
}

// crashGP: x86 #GP only.
func crashGP() { fail("-mode gp is amd64-only") }

// checkFltinv: arm64 FP exceptions do not trap (FPCR trap enables are
// RAZ/WI on most cores); nothing to check.
func checkFltinv() {}
