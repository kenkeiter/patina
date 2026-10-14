package main

import (
	"bytes"
	"os"
	"syscall"
	"time"
	"unsafe"
)

const (
	fionread       = 0x541b
	fionbio        = 0x5421
	fionclex       = 0x5450
	fioclex        = 0x5451
	sysEpollPwait2 = 441
)

func ioctlInt(fd int, req uintptr, v *int32) error {
	_, _, e := syscall.RawSyscall(syscall.SYS_IOCTL, uintptr(fd), req, uintptr(unsafe.Pointer(v)))
	return errnoOf(e)
}

func checkFds() {
	var p [2]int
	if err := syscall.Pipe2(p[:], syscall.O_CLOEXEC); err != nil {
		fail("pipe2: %v", err)
	}
	r, w := p[0], p[1]
	defer syscall.Close(r)
	defer syscall.Close(w)

	// writev / readv across iovecs.
	bufs := [][]byte{[]byte("hello, "), []byte("vectored "), []byte("world")}
	iov := make([]syscall.Iovec, len(bufs))
	for i, b := range bufs {
		iov[i].Base = &b[0]
		iov[i].SetLen(len(b))
	}
	n, _, e := syscall.Syscall(syscall.SYS_WRITEV, uintptr(w), uintptr(unsafe.Pointer(&iov[0])), uintptr(len(iov)))
	if e != 0 || n != 21 {
		fail("writev = %d, %v", n, e)
	}
	// FIONREAD sees the 21 bytes.
	var avail int32
	if err := ioctlInt(r, fionread, &avail); err != nil || avail != 21 {
		fail("FIONREAD = %d, %v; want 21", avail, err)
	}
	out := [][]byte{make([]byte, 5), make([]byte, 16)}
	oiov := make([]syscall.Iovec, 2)
	for i, b := range out {
		oiov[i].Base = &b[0]
		oiov[i].SetLen(len(b))
	}
	n, _, e = syscall.Syscall(syscall.SYS_READV, uintptr(r), uintptr(unsafe.Pointer(&oiov[0])), 2)
	if e != 0 || n != 21 || string(out[0])+string(out[1]) != "hello, vectored world" {
		fail("readv = %d, %v: %q %q", n, e, out[0], out[1])
	}
	// FIONBIO: a read on the empty pipe now fails with EAGAIN.
	one := int32(1)
	if err := ioctlInt(r, fionbio, &one); err != nil {
		fail("FIONBIO: %v", err)
	}
	if _, err := syscall.Read(r, make([]byte, 1)); err != syscall.EAGAIN {
		fail("read after FIONBIO = %v, want EAGAIN", err)
	}
	// FIONCLEX / FIOCLEX toggle FD_CLOEXEC.
	getfd := func() uintptr {
		v, _, _ := syscall.RawSyscall(syscall.SYS_FCNTL, uintptr(r), syscall.F_GETFD, 0)
		return v
	}
	syscall.RawSyscall(syscall.SYS_IOCTL, uintptr(r), fionclex, 0)
	cleared := getfd()
	syscall.RawSyscall(syscall.SYS_IOCTL, uintptr(r), fioclex, 0)
	if cleared&syscall.FD_CLOEXEC != 0 || getfd()&syscall.FD_CLOEXEC == 0 {
		fail("FIONCLEX/FIOCLEX: F_GETFD %#x then %#x", cleared, getfd())
	}
	// pread/pwrite/lseek on a pipe → ESPIPE.
	if _, err := syscall.Pread(r, make([]byte, 1), 0); err != syscall.ESPIPE {
		fail("pread64 on a pipe = %v, want ESPIPE", err)
	}
	if _, err := syscall.Pwrite(w, []byte("x"), 0); err != syscall.ESPIPE {
		fail("pwrite64 on a pipe = %v, want ESPIPE", err)
	}
	if _, err := syscall.Seek(r, 0, 0); err != syscall.ESPIPE {
		fail("lseek on a pipe = %v, want ESPIPE", err)
	}
	ok("pipe: writev/readv 3+2 iovecs, FIONREAD 21, FIONBIO → EAGAIN, FIONCLEX/FIOCLEX, pread64/pwrite64/lseek ESPIPE")

	// pread64/pwrite64 on a regular file.
	f, err := os.CreateTemp("", "robust")
	if err != nil {
		fail("CreateTemp: %v", err)
	}
	defer os.Remove(f.Name())
	defer f.Close()
	fd := int(f.Fd())
	if _, err := syscall.Pwrite(fd, []byte("0123456789"), 0); err != nil {
		fail("pwrite64: %v", err)
	}
	if _, err := syscall.Pwrite(fd, []byte("AB"), 4); err != nil {
		fail("pwrite64 at 4: %v", err)
	}
	got := make([]byte, 4)
	if _, err := syscall.Pread(fd, got, 3); err != nil || !bytes.Equal(got, []byte("3AB6")) {
		fail("pread64 at 3 = %q, %v", got, err)
	}
	if off, _ := syscall.Seek(fd, 0, 1); off != 0 {
		fail("pread/pwrite moved the file offset to %d", off)
	}
	ok("file: pwrite64/pread64 at offsets leave the file offset alone")

	// epoll_pwait2 with a timespec timeout.
	epfd, err := syscall.EpollCreate1(syscall.EPOLL_CLOEXEC)
	if err != nil {
		fail("epoll_create1: %v", err)
	}
	defer syscall.Close(epfd)
	ts := syscall.NsecToTimespec(int64(15 * time.Millisecond))
	var evs [1]syscall.EpollEvent
	t0 := time.Now()
	n, _, e = syscall.Syscall6(sysEpollPwait2, uintptr(epfd), uintptr(unsafe.Pointer(&evs[0])), 1,
		uintptr(unsafe.Pointer(&ts)), 0, 8)
	d := time.Since(t0)
	if e != 0 || n != 0 || d < 15*time.Millisecond {
		fail("epoll_pwait2(15ms) = %d, %v after %v", n, e, d)
	}
	ok("epoll_pwait2: 15ms timespec timeout after %v", d.Round(100*time.Microsecond))

	checkLegacyEntryPoints()
}
