package main

import (
	"os"
	"os/signal"
	"syscall"
	"time"
	"unsafe"
)

// checkLegacyEntryPoints: the x86_64-only syscalls Go never uses itself:
// pipe, dup2, eventfd, epoll_create, epoll_wait, poll.
func checkLegacyEntryPoints() {
	var p [2]int32
	if _, _, e := syscall.RawSyscall(syscall.SYS_PIPE, uintptr(unsafe.Pointer(&p[0])), 0, 0); e != 0 {
		fail("pipe: %v", e)
	}
	r, w := int(p[0]), int(p[1])
	defer syscall.Close(r)
	defer syscall.Close(w)
	// dup2 onto a fixed high fd, and dup2(fd, fd) returns fd.
	const target = 200
	if v, _, e := syscall.RawSyscall(syscall.SYS_DUP2, uintptr(w), target, 0); e != 0 || v != target {
		fail("dup2(%d, %d) = %d, %v", w, target, v, e)
	}
	defer syscall.Close(target)
	if v, _, e := syscall.RawSyscall(syscall.SYS_DUP2, target, target, 0); e != 0 || v != target {
		fail("dup2(fd, fd) = %d, %v", v, e)
	}
	syscall.Write(target, []byte("z"))
	// poll sees the byte.
	pfd := struct {
		fd      int32
		events  int16
		revents int16
	}{int32(r), 1, 0} // POLLIN
	if v, _, e := syscall.Syscall(syscall.SYS_POLL, uintptr(unsafe.Pointer(&pfd)), 1, 100); e != 0 || v != 1 || pfd.revents&1 == 0 {
		fail("poll = %d, %v, revents %#x", v, e, pfd.revents)
	}
	// eventfd (no flags) + epoll_create + epoll_wait.
	efd, _, e := syscall.RawSyscall(syscall.SYS_EVENTFD, 3, 0, 0)
	if e != 0 {
		fail("eventfd: %v", e)
	}
	defer syscall.Close(int(efd))
	epfd, _, e := syscall.RawSyscall(syscall.SYS_EPOLL_CREATE, 1, 0, 0)
	if e != 0 {
		fail("epoll_create: %v", e)
	}
	defer syscall.Close(int(epfd))
	if v, _, e := syscall.RawSyscall(syscall.SYS_EPOLL_CREATE, 0, 0, 0); e != syscall.EINVAL {
		fail("epoll_create(0) = %d, %v; want EINVAL", v, e)
	}
	ev := syscall.EpollEvent{Events: syscall.EPOLLIN, Fd: int32(efd)}
	if err := syscall.EpollCtl(int(epfd), syscall.EPOLL_CTL_ADD, int(efd), &ev); err != nil {
		fail("epoll_ctl: %v", err)
	}
	var evs [2]syscall.EpollEvent
	n, _, e := syscall.Syscall6(syscall.SYS_EPOLL_WAIT, epfd, uintptr(unsafe.Pointer(&evs[0])), 2, 100, 0, 0)
	if e != 0 || n != 1 || evs[0].Fd != int32(efd) {
		fail("epoll_wait = %d, %v, %+v", n, e, evs[0])
	}
	var cnt [8]byte
	if _, err := syscall.Read(int(efd), cnt[:]); err != nil || cnt[0] != 3 {
		fail("eventfd read = %v, %v", cnt, err)
	}
	ok("x86_64 entry points: pipe, dup2 (incl. fd onto itself), poll, eventfd, epoll_create (+EINVAL for 0), epoll_wait")
}

// checkPause: x86 pause() returns EINTR once a handler ran.
func checkPause() {
	ch := make(chan os.Signal, 4)
	signal.Notify(ch, syscall.SIGUSR2)
	defer signal.Stop(ch)
	var e syscall.Errno
	tid, wait := onLockedThread(func() {
		_, _, e = syscall.Syscall(syscall.SYS_PAUSE, 0, 0, 0)
	})
	time.Sleep(50 * time.Millisecond)
	tgkill(tid, syscall.SIGUSR2)
	wait()
	if e != syscall.EINTR {
		fail("pause = %v, want EINTR", e)
	}
	<-ch
	ok("pause: EINTR after SIGUSR2's handler ran")
}
