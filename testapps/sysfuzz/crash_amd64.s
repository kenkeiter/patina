#include "textflag.h"

// func sigreturnAt(sp uintptr)
TEXT ·sigreturnAt(SB),NOSPLIT,$0-8
	MOVQ sp+0(FP), AX
	MOVQ AX, SP
	MOVQ $15, AX // rt_sigreturn
	SYSCALL
	RET

// exit_group(42); reached through a sigreturn frame's rip.
TEXT ·exit42(SB),NOSPLIT|NOFRAME,$0-0
	MOVQ $42, DI
	MOVQ $231, AX // exit_group
	SYSCALL
spin:
	JMP spin

// getpid, then exit_group(42).
TEXT ·getpidExit42(SB),NOSPLIT|NOFRAME,$0-0
	MOVQ $39, AX // getpid
	SYSCALL
	MOVQ $42, DI
	MOVQ $231, AX // exit_group
	SYSCALL
spin2:
	JMP spin2

// func exit42Addr() uintptr
TEXT ·exit42Addr(SB),NOSPLIT,$0-8
	MOVQ $·exit42(SB), AX
	MOVQ AX, ret+0(FP)
	RET

// func getpidExit42Addr() uintptr
TEXT ·getpidExit42Addr(SB),NOSPLIT,$0-8
	MOVQ $·getpidExit42(SB), AX
	MOVQ AX, ret+0(FP)
	RET

// func jumpTo(pc uintptr)
TEXT ·jumpTo(SB),NOSPLIT,$0-8
	MOVQ pc+0(FP), AX
	JMP AX
