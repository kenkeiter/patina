#include "textflag.h"

// func udf()
// UDF #0: undefined instruction (EC 0x00) → SIGILL ILL_ILLOPC.
TEXT ·udf(SB),NOSPLIT,$0-0
	WORD $0x00000000
	RET

// func ldaxr(p *uint64) uint64
// Exclusive load: needs natural alignment even with FEAT_LSE2, so an
// unaligned p takes an alignment fault (DFSC 0x21) → SIGBUS BUS_ADRALN.
TEXT ·ldaxr(SB),NOSPLIT,$0-16
	MOVD p+0(FP), R0
	LDAXR (R0), R1
	MOVD R1, ret+8(FP)
	RET
