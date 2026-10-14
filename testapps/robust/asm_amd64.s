#include "textflag.h"

// func udf()
// UD2 → #UD → SIGILL ILL_ILLOPN.
TEXT ·udf(SB),NOSPLIT,$0-0
	BYTE $0x0f; BYTE $0x0b
	RET

// func noncanonical() uint64
// Load from a non-canonical address → #GP(0) → SIGSEGV SI_KERNEL.
TEXT ·noncanonical(SB),NOSPLIT,$0-8
	MOVQ $0x8000000000000000, AX
	MOVQ (AX), AX
	MOVQ AX, ret+0(FP)
	RET

// func setmxcsr(v uint32)
TEXT ·setmxcsr(SB),NOSPLIT,$0-4
	LDMXCSR v+0(FP)
	RET

// func getmxcsr() uint32
TEXT ·getmxcsr(SB),NOSPLIT,$0-4
	STMXCSR ret+0(FP)
	RET

// func divsd(a, b float64) float64
TEXT ·divsd(SB),NOSPLIT,$0-24
	MOVSD a+0(FP), X0
	DIVSD b+8(FP), X0
	MOVSD X0, ret+16(FP)
	RET
