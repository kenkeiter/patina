package main

import "syscall"

// epoll_event is packed on x86_64.
const epollEventSize = 12

// casesArch: the x86_64-only legacy entry points and arch_prctl.
func casesArch(t *tcase) {
	c := t.c
	// fs/select.c sys_poll: nfds > RLIMIT_NOFILE → -EINVAL; bad fds → -EFAULT.
	r, e := scr(SYS_POLL, c.alloc(8), 1<<20+1, 0)
	t.want("poll nfds > NOFILE", r, e, syscall.EINVAL)
	r, e = scr(SYS_POLL, kernHalf, 1, 0)
	t.want("poll kernel fds", r, e, syscall.EFAULT)
	// sys_select: negative tv → -EINVAL; nfds < 0 → -EINVAL; a bad tv
	// pointer → -EFAULT.
	r, e = scr(SYS_SELECT, 0, 0, 0, 0, c.u64s(^uint64(0), 0))
	t.want("select negative tv", r, e, syscall.EINVAL)
	r, e = scr(SYS_SELECT, u(-1), 0, 0, 0, c.u64s(0, 0))
	t.want("select nfds -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_SELECT, 0, 0, 0, 0, kernHalf)
	t.want("select kernel tv", r, e, syscall.EFAULT)
	// epoll_wait: maxevents <= 0 → -EINVAL; epoll_create(size <= 0) → -EINVAL.
	r, e = scr(SYS_EPOLL_WAIT, 0, junk(randFor(20), 64), 0, 0)
	t.want("epoll_wait maxevents 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_EPOLL_CREATE, 0)
	t.want("epoll_create 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_EPOLL_CREATE, u(-1))
	t.want("epoll_create -1", r, e, syscall.EINVAL)
	// dup2: oldfd == newfd returns newfd if valid, -EBADF otherwise.
	r, e = scr(SYS_DUP2, 777, 777)
	t.want("dup2 bad old == new", r, e, syscall.EBADF)
	r, e = scr(SYS_DUP2, 1, 1)
	t.wantRet("dup2 1 1", r, e, 1)
	r, e = scr(SYS_DUP2, 1, u(-1))
	t.want("dup2 new -1", r, e, syscall.EBADF)
	r, e = scr(SYS_PIPE, kernHalf)
	t.want("pipe kernel fds", r, e, syscall.EFAULT)
	r, e = scr(SYS_OPEN, 0, 0, 0)
	t.want("open NULL", r, e, syscall.EFAULT)
	// arch/x86/kernel/process_64.c do_arch_prctl_64: unknown code →
	// -EINVAL; ARCH_SET_FS beyond TASK_SIZE_MAX → -EPERM; ARCH_GET_FS
	// to a bad pointer → -EFAULT.
	r, e = scr(SYS_ARCH_PRCTL, 0x9999, 0)
	t.want("arch_prctl bad code", r, e, syscall.EINVAL)
	r, e = scr(SYS_ARCH_PRCTL, 0x1002, kernHalf)
	t.want("arch_prctl ARCH_SET_FS kernel", r, e, syscall.EPERM)
	r, e = scr(SYS_ARCH_PRCTL, 0x1003, kernHalf)
	t.want("arch_prctl ARCH_GET_FS kernel ptr", r, e, syscall.EFAULT)
	r, e = scr(SYS_TIME, kernHalf)
	t.want("time kernel tloc", r, e, syscall.EFAULT)
}
