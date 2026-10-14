package main

import "syscall"

// epoll_event is naturally aligned on arm64.
const epollEventSize = 16

// casesArch: arm64 has no legacy entry points; check the sigaltstack
// minimum, which is larger here (MINSIGSTKSZ 5120 and AT_MINSIGSTKSZ).
func casesArch(t *tcase) {
	c := t.c
	// kernel/signal.c do_sigaltstack: "if (ss_size < min_ss_size) return -ENOMEM".
	r, e := scr(SYS_SIGALTSTACK, c.u64s(uint64(rwBase), 0, 4096), 0)
	t.want("sigaltstack 4096 below arm64 MINSIGSTKSZ", r, e, syscall.ENOMEM)
}
