package main

// x86_64 rt_sigreturn reads the ucontext at rsp (the restorer's ret popped
// pretcode; arch/x86/kernel/signal_64.c): uc_flags, uc_link, uc_stack
// (+16), uc_mcontext (+40: r8..r15, rdi, rsi, rbp, rbx, rdx, rax, rcx,
// rsp (+160), rip (+168), eflags (+176), cs/gs/fs/ss, err, trapno,
// oldmask, cr2, fpstate (+224)), uc_sigmask (+296).
const (
	x64Stack   = 16
	x64RSP     = 160
	x64RIP     = 168
	x64Eflags  = 176
	x64Fpstate = 224

	eflTF = 1 << 8
	eflAC = 1 << 18

	fpMxcsr    = 24
	fpSwBytes  = 464
	fpMagic1   = 0x46505853
	fpMagic2   = 0x46505845
	xsaveHdr   = 512
	xstateSize = 832 // legacy + header + AVX
)

func archFrame(mode string) (uintptr, bool) {
	p := frameBuf()
	put64(p+x64Stack+8, 2) // uc_stack.ss_flags = SS_DISABLE
	put64(p+x64RSP, uint64(p-4096))
	put64(p+x64RIP, uint64(exit42Addr()))
	put64(p+x64Eflags, 0x202)
	fp := (p + 16<<10) &^ 63
	switch mode {
	case "sigreturn-ok":
	case "sigreturn-krip":
		put64(p+x64RIP, kernHalf) // non-user rip: SIGSEGV
	case "sigreturn-noncanon-rip":
		put64(p+x64RIP, userEnd) // first non-canonical address: SIGSEGV
	case "sigreturn-krsp":
		// A non-canonical rsp is not checked by Linux; the program never
		// touches its stack: getpid, then exit_group(42).
		put64(p+x64RSP, 0x8000_0000_0000_0000)
		put64(p+x64RIP, uint64(getpidExit42Addr()))
	case "sigreturn-tf":
		// Single-step: the next instruction raises SIGTRAP (Linux lets TF
		// through, FIX_EFLAGS). Keep the live sigaltstack in uc_stack:
		// with SS_DISABLE the SIGTRAP lands on the arena stack, where Go's
		// handler (adjustSignalStack → needm) waits forever for an extra M
		// that a non-cgo binary never has, on Linux just the same.
		scr(SYS_SIGALTSTACK, 0, p+x64Stack)
		put64(p+x64Eflags, 0x202|eflTF)
	case "sigreturn-ac":
		put64(p+x64Eflags, 0x202|eflAC)
	case "sigreturn-iopl":
		put64(p+x64Eflags, 0x202|3<<12) // IOPL is not in FIX_EFLAGS: ignored
		put64(p+x64RIP, uint64(getpidExit42Addr()))
	case "sigreturn-mxcsr":
		put32(fp+fpMxcsr, 0xffff_ffff) // reserved MXCSR bits: bad frame
		put64(p+x64Fpstate, uint64(fp))
	case "sigreturn-xsave":
		// A well-formed fpx_sw_bytes with garbage XSAVE header bits:
		// the kernel must sanitize them (an XRSTOR #GP in the kernel
		// would be fatal).
		put32(fp+fpMxcsr, 0x1f80)
		put32(fp+fpSwBytes, fpMagic1)
		put32(fp+fpSwBytes+4, xstateSize+4)
		put64(fp+fpSwBytes+8, ^uint64(0))
		put32(fp+fpSwBytes+16, xstateSize)
		put64(fp+xsaveHdr, ^uint64(0))   // xstate_bv
		put64(fp+xsaveHdr+8, ^uint64(0)) // xcomp_bv
		put64(fp+xsaveHdr+16, ^uint64(0))
		put32(fp+xstateSize, fpMagic2)
		put64(p+x64Fpstate, uint64(fp))
	case "sigreturn-fpunaligned":
		put64(p+x64Fpstate, uint64(fp+8))
	case "sigreturn-fpkernel":
		put64(p+x64Fpstate, kernHalf)
	default:
		return 0, false
	}
	return p, true
}

// lastPageCode: mov eax, 39 (getpid); syscall — the syscall instruction
// ends exactly at the end of user space.
func lastPageCode() []byte {
	return []byte{0xb8, 0x27, 0x00, 0x00, 0x00, 0x0f, 0x05}
}
