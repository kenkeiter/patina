package main

// arm64 rt_sigframe (arch/arm64/kernel/signal.c): siginfo (128 bytes),
// then ucontext: uc_flags, uc_link, uc_stack (+16), uc_sigmask (+40),
// uc_mcontext (+176): fault_address, regs[31], sp, pc, pstate, then
// __reserved (+288) holding the fpsimd_context record and a terminator.
const (
	a64Uc       = 128
	a64Stack    = a64Uc + 16
	a64Mctx     = a64Uc + 176
	a64Regs     = a64Mctx + 8
	a64SP       = a64Mctx + 256
	a64PC       = a64Mctx + 264
	a64Pstate   = a64Mctx + 272
	a64Reserved = a64Mctx + 288

	fpsimdMagic = 0x46508001
	fpsimdSize  = 528

	psrIL = 1 << 20 // illegal execution state (RES0 in a sigframe)
	psrI  = 1 << 7
)

// archFrame builds the frame for an arm64 sigreturn-* mode.
func archFrame(mode string) (uintptr, bool) {
	p := frameBuf()
	put64(p+a64Stack+8, 2) // uc_stack.ss_flags = SS_DISABLE
	put64(p+a64SP, uint64(p-4096))
	put64(p+a64PC, uint64(exit42Addr()))
	put32(p+a64Reserved, fpsimdMagic)
	put32(p+a64Reserved+4, fpsimdSize)
	var pstate uint64
	switch mode {
	case "sigreturn-ok":
	case "sigreturn-il":
		// Linux valid_native_regs clears the RES0 bits (IL among them):
		// the frame is accepted and exit_group(42) runs.
		pstate = psrIL
	case "sigreturn-el1":
		pstate = 0x5 // EL1h: must be rejected (SIGSEGV)
	case "sigreturn-daif":
		pstate = psrI // IRQs masked at EL0: rejected
	case "sigreturn-aarch32":
		pstate = 0x10 // AArch32: rejected
	case "sigreturn-kpc":
		put64(p+a64PC, kernHalf) // accepted; the fetch faults at EL0
	case "sigreturn-nofp":
		put32(p+a64Reserved, 0) // no fpsimd record: rejected
		put32(p+a64Reserved+4, 0)
	case "sigreturn-unaligned":
		return p + 8, true // sp not 16-aligned: rejected
	default:
		return 0, false
	}
	put64(p+a64Pstate, pstate)
	return p, true
}

// lastPageCode: mov x8, #172 (getpid); svc #0 — the svc is the last
// instruction of user space.
func lastPageCode() []byte {
	return []byte{0x88, 0x15, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4}
}
