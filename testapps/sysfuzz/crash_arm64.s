#include "textflag.h"

// func sigreturnAt(sp uintptr)
TEXT ·sigreturnAt(SB),NOSPLIT,$0-8
	MOVD sp+0(FP), R0
	MOVD R0, RSP
	MOVD $139, R8 // rt_sigreturn
	SVC
	RET

// exit_group(42); reached through a sigreturn frame's pc.
TEXT ·exit42(SB),NOSPLIT|NOFRAME,$0-0
	MOVD $42, R0
	MOVD $94, R8 // exit_group
	SVC
spin:
	B spin

// getpid, then exit_group(42).
TEXT ·getpidExit42(SB),NOSPLIT|NOFRAME,$0-0
	MOVD $172, R8 // getpid
	SVC
	MOVD $42, R0
	MOVD $94, R8 // exit_group
	SVC
spin2:
	B spin2

// func exit42Addr() uintptr
TEXT ·exit42Addr(SB),NOSPLIT,$0-8
	MOVD $·exit42(SB), R0
	MOVD R0, ret+0(FP)
	RET

// func getpidExit42Addr() uintptr
TEXT ·getpidExit42Addr(SB),NOSPLIT,$0-8
	MOVD $·getpidExit42(SB), R0
	MOVD R0, ret+0(FP)
	RET

// func jumpTo(pc uintptr)
TEXT ·jumpTo(SB),NOSPLIT,$0-8
	MOVD pc+0(FP), R0
	JMP (R0)
