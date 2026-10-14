// fdtest exercises the fd table and the Phase 2 file objects: pipes,
// eventfd, epoll, dup/dup3, fcntl, fstat, ppoll, select (x86_64 select,
// arm64 pselect6), and finally Go timers
// (which need the runtime's netpoller: epoll + eventfd). Prints
// "PASS fdtest" and exits 0, or "FAIL fdtest: ..." and exits 1.
package main

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"runtime"
	"syscall"
	"time"
	"unsafe"
)

const (
	efdSemaphore = 1
	efdNonblock  = syscall.O_NONBLOCK
	efdCloexec   = syscall.O_CLOEXEC
)

func fail(format string, a ...any) {
	fmt.Printf("FAIL fdtest: "+format+"\n", a...)
	os.Exit(1)
}

func ok(name string) { fmt.Println("ok", name) }

func must(err error, what string) {
	if err != nil {
		fail("%s: %v", what, err)
	}
}

func wantErr(err error, want syscall.Errno, what string) {
	if !errors.Is(err, want) {
		fail("%s: got %v, want %v", what, err, want)
	}
}

func pipe2(flags int) [2]int {
	var p [2]int
	must(syscall.Pipe2(p[:], flags), "pipe2")
	return p
}

func closeAll(fds ...int) {
	for _, fd := range fds {
		syscall.Close(fd)
	}
}

func fcntl(fd, cmd, arg int) (int, error) {
	r, _, e := syscall.Syscall(syscall.SYS_FCNTL, uintptr(fd), uintptr(cmd), uintptr(arg))
	if e != 0 {
		return -1, e
	}
	return int(r), nil
}

func eventfd(init uint, flags int) int {
	r, _, e := syscall.Syscall(syscall.SYS_EVENTFD2, uintptr(init), uintptr(flags), 0)
	if e != 0 {
		fail("eventfd2: %v", e)
	}
	return int(r)
}

func writeU64(fd int, v uint64) error {
	_, err := syscall.Write(fd, (*[8]byte)(unsafe.Pointer(&v))[:])
	return err
}

func readU64(fd int) (uint64, error) {
	var v uint64
	_, err := syscall.Read(fd, (*[8]byte)(unsafe.Pointer(&v))[:])
	return v, err
}

func testOsPipe() {
	r, w, err := os.Pipe()
	must(err, "os.Pipe")
	// Blocking read in a goroutine (through the netpoller).
	got := make(chan string)
	go func() {
		b := make([]byte, 64)
		n, err := r.Read(b)
		if err != nil {
			got <- "error: " + err.Error()
			return
		}
		got <- string(b[:n])
	}()
	time.Sleep(10 * time.Millisecond)
	_, err = w.Write([]byte("ping"))
	must(err, "pipe write")
	if s := <-got; s != "ping" {
		fail("pipe read got %q", s)
	}
	// EOF after the write end is closed.
	must(w.Close(), "close write end")
	n, err := r.Read(make([]byte, 8))
	if n != 0 || err != io.EOF {
		fail("read after writer close: n=%d err=%v, want EOF", n, err)
	}
	r.Close()
	ok("os.Pipe round trip + EOF")

	// EPIPE after the read end is closed.
	r, w, err = os.Pipe()
	must(err, "os.Pipe")
	r.Close()
	_, err = w.Write([]byte("x"))
	wantErr(err, syscall.EPIPE, "write with no reader")
	w.Close()
	ok("EPIPE")

	// 70000 bytes: more than the 64 KiB pipe buffer, drained by a reader.
	r, w, err = os.Pipe()
	must(err, "os.Pipe")
	data := make([]byte, 70000)
	for i := range data {
		data[i] = byte(i * 7)
	}
	done := make(chan []byte)
	go func() {
		b, err := io.ReadAll(r)
		if err != nil {
			fail("ReadAll: %v", err)
		}
		done <- b
	}()
	n, err = w.Write(data)
	if n != len(data) || err != nil {
		fail("big write: n=%d err=%v", n, err)
	}
	w.Close()
	if b := <-done; !bytes.Equal(b, data) {
		fail("big transfer: got %d bytes, mismatch", len(b))
	}
	r.Close()
	ok("70000-byte transfer")
}

func testRawPipe() {
	p := pipe2(syscall.O_NONBLOCK | syscall.O_CLOEXEC)
	_, err := syscall.Read(p[0], make([]byte, 4))
	wantErr(err, syscall.EAGAIN, "non-blocking read of empty pipe")
	// Write on the read end / read on the write end: EBADF.
	_, err = syscall.Write(p[0], []byte("x"))
	wantErr(err, syscall.EBADF, "write on read end")
	// Fill it: 65536 bytes fit, then EAGAIN.
	total := 0
	chunk := make([]byte, 8192)
	for {
		n, err := syscall.Write(p[1], chunk)
		if err != nil {
			wantErr(err, syscall.EAGAIN, "write to full pipe")
			break
		}
		total += n
	}
	if total != 65536 {
		fail("pipe capacity %d, want 65536", total)
	}
	closeAll(p[0], p[1])
	ok("non-blocking pipe2 (EAGAIN, capacity 64 KiB)")

	// Blocking pipe: a raw blocking read woken by a writer goroutine.
	p = pipe2(syscall.O_CLOEXEC)
	go func() {
		time.Sleep(20 * time.Millisecond)
		syscall.Write(p[1], []byte("wake"))
	}()
	b := make([]byte, 16)
	n, err := syscall.Read(p[0], b)
	if err != nil || string(b[:n]) != "wake" {
		fail("blocking read: %q %v", b[:n], err)
	}
	closeAll(p[0], p[1])
	ok("blocking raw read")

	// fstat: S_IFIFO.
	p = pipe2(0)
	var st syscall.Stat_t
	must(syscall.Fstat(p[0], &st), "fstat pipe")
	if st.Mode&syscall.S_IFMT != syscall.S_IFIFO {
		fail("pipe st_mode %#o, want S_IFIFO", st.Mode)
	}
	ok("fstat pipe S_IFIFO")

	// fcntl F_GETFL / F_SETFL, shared through dup.
	fl, err := fcntl(p[0], syscall.F_GETFL, 0)
	must(err, "F_GETFL")
	if fl&syscall.O_NONBLOCK != 0 || fl&syscall.O_ACCMODE != syscall.O_RDONLY {
		fail("F_GETFL = %#x", fl)
	}
	d, err := syscall.Dup(p[0])
	must(err, "dup")
	_, err = fcntl(d, syscall.F_SETFL, fl|syscall.O_NONBLOCK)
	must(err, "F_SETFL")
	fl2, _ := fcntl(p[0], syscall.F_GETFL, 0)
	if fl2&syscall.O_NONBLOCK == 0 {
		fail("O_NONBLOCK set via dup not visible on original (F_GETFL %#x)", fl2)
	}
	_, err = syscall.Read(p[0], b)
	wantErr(err, syscall.EAGAIN, "read after F_SETFL O_NONBLOCK")
	ok("fcntl F_GETFL/F_SETFL shared by dup")

	// dup: data written to the pipe is readable through the dup.
	syscall.Write(p[1], []byte("dup"))
	n, err = syscall.Read(d, b)
	if err != nil || string(b[:n]) != "dup" {
		fail("read through dup: %q %v", b[:n], err)
	}
	// dup3 with O_CLOEXEC onto a chosen fd; same fd → EINVAL.
	target := 100
	must(syscall.Dup3(p[1], target, syscall.O_CLOEXEC), "dup3")
	fd, err := fcntl(target, syscall.F_GETFD, 0)
	if err != nil || fd != syscall.FD_CLOEXEC {
		fail("F_GETFD after dup3(O_CLOEXEC) = %d %v", fd, err)
	}
	_, err = fcntl(target, syscall.F_SETFD, 0)
	must(err, "F_SETFD")
	fd, _ = fcntl(target, syscall.F_GETFD, 0)
	if fd != 0 {
		fail("F_GETFD after F_SETFD 0 = %d", fd)
	}
	wantErr(syscall.Dup3(target, target, 0), syscall.EINVAL, "dup3 same fd")
	// F_DUPFD_CLOEXEC: lowest free >= arg.
	nfd, err := fcntl(p[0], syscall.F_DUPFD_CLOEXEC, 200)
	if err != nil || nfd != 200 {
		fail("F_DUPFD_CLOEXEC 200 = %d %v", nfd, err)
	}
	// Closing the write end via one fd is not enough for EOF: `target`
	// still refers to it.
	syscall.Close(p[1])
	syscall.Write(target, []byte("z"))
	n, err = syscall.Read(p[0], b)
	if err != nil || string(b[:n]) != "z" {
		fail("write through dup3'd fd: %q %v", b[:n], err)
	}
	syscall.Close(target)
	n, err = syscall.Read(p[0], b)
	if n != 0 || err != nil {
		fail("EOF after last writer fd closed: n=%d err=%v", n, err)
	}
	closeAll(p[0], d, nfd)
	wantErr(syscall.Close(p[0]), syscall.EBADF, "close of closed fd")
	ok("dup/dup3/F_DUPFD_CLOEXEC/close EBADF")

	// EMFILE at RLIMIT_NOFILE. Go raises the soft limit to the hard limit
	// at startup; put it back to 1024 for the test.
	var lim syscall.Rlimit
	must(syscall.Getrlimit(syscall.RLIMIT_NOFILE, &lim), "getrlimit")
	saved := lim
	lim.Cur = 1024
	must(syscall.Setrlimit(syscall.RLIMIT_NOFILE, &lim), "setrlimit")
	var fds []int
	for {
		fd, err := syscall.Dup(0)
		if err != nil {
			wantErr(err, syscall.EMFILE, "dup until full")
			break
		}
		fds = append(fds, fd)
		if len(fds) > 2000 {
			fail("no EMFILE after 2000 dups")
		}
	}
	if last := fds[len(fds)-1]; last != 1023 {
		fail("highest fd %d, want 1023", last)
	}
	closeAll(fds...)
	must(syscall.Setrlimit(syscall.RLIMIT_NOFILE, &saved), "setrlimit restore")
	ok(fmt.Sprintf("EMFILE after %d dups (soft limit was %d)", len(fds), saved.Cur))
}

func testEventfd() {
	fd := eventfd(0, efdNonblock|efdCloexec)
	_, err := readU64(fd)
	wantErr(err, syscall.EAGAIN, "read of zero eventfd")
	must(writeU64(fd, 3), "eventfd write 3")
	must(writeU64(fd, 4), "eventfd write 4")
	v, err := readU64(fd)
	if err != nil || v != 7 {
		fail("eventfd read = %d %v, want 7", v, err)
	}
	wantErr(writeU64(fd, ^uint64(0)), syscall.EINVAL, "eventfd write of 2^64-1")
	_, err = syscall.Write(fd, []byte{1, 2, 3, 4})
	wantErr(err, syscall.EINVAL, "eventfd 4-byte write")
	_, err = syscall.Read(fd, make([]byte, 4))
	wantErr(err, syscall.EINVAL, "eventfd 4-byte read")
	must(writeU64(fd, ^uint64(0)-1), "eventfd write max")
	wantErr(writeU64(fd, 1), syscall.EAGAIN, "eventfd overflow")
	var st syscall.Stat_t
	must(syscall.Fstat(fd, &st), "fstat eventfd")
	syscall.Close(fd)

	fd = eventfd(2, efdSemaphore|efdNonblock)
	for i := 0; i < 2; i++ {
		v, err := readU64(fd)
		if err != nil || v != 1 {
			fail("semaphore read %d = %d %v", i, v, err)
		}
	}
	_, err = readU64(fd)
	wantErr(err, syscall.EAGAIN, "semaphore read at 0")
	syscall.Close(fd)

	// Blocking read woken by a goroutine.
	fd = eventfd(0, 0)
	go func() {
		time.Sleep(10 * time.Millisecond)
		writeU64(fd, 5)
	}()
	v, err = readU64(fd)
	if err != nil || v != 5 {
		fail("blocking eventfd read = %d %v", v, err)
	}
	syscall.Close(fd)
	ok("eventfd counter/semaphore/blocking")
}

func epollWait(epfd int, ms int) []syscall.EpollEvent {
	ev := make([]syscall.EpollEvent, 8)
	n, err := syscall.EpollWait(epfd, ev, ms)
	must(err, "epoll_wait")
	return ev[:n]
}

func testEpoll() {
	epfd, err := syscall.EpollCreate1(syscall.EPOLL_CLOEXEC)
	must(err, "epoll_create1")
	add := func(fd int, events uint32) error {
		return syscall.EpollCtl(epfd, syscall.EPOLL_CTL_ADD, fd, &syscall.EpollEvent{Events: events, Fd: int32(fd)})
	}
	b := make([]byte, 16)

	// Level-triggered.
	lt := pipe2(syscall.O_NONBLOCK)
	must(add(lt[0], syscall.EPOLLIN), "ADD lt")
	wantErr(add(lt[0], syscall.EPOLLIN), syscall.EEXIST, "second ADD")
	if ev := epollWait(epfd, 0); len(ev) != 0 {
		fail("empty epoll: %d events", len(ev))
	}
	syscall.Write(lt[1], []byte("a"))
	for i := 0; i < 2; i++ {
		ev := epollWait(epfd, 0)
		if len(ev) != 1 || ev[0].Fd != int32(lt[0]) || ev[0].Events != syscall.EPOLLIN {
			fail("LT wait %d: %+v", i, ev)
		}
	}
	syscall.Read(lt[0], b)
	if ev := epollWait(epfd, 0); len(ev) != 0 {
		fail("LT after drain: %+v", ev)
	}
	must(syscall.EpollCtl(epfd, syscall.EPOLL_CTL_DEL, lt[0], nil), "DEL lt")
	wantErr(syscall.EpollCtl(epfd, syscall.EPOLL_CTL_DEL, lt[0], nil), syscall.ENOENT, "second DEL")
	wantErr(syscall.EpollCtl(epfd, syscall.EPOLL_CTL_MOD, lt[0], &syscall.EpollEvent{Events: syscall.EPOLLIN}), syscall.ENOENT, "MOD of absent fd")
	wantErr(add(epfd, syscall.EPOLLIN), syscall.EINVAL, "add epoll to itself")
	ok("epoll level-triggered, EEXIST/ENOENT/EINVAL")

	// Edge-triggered.
	et := pipe2(syscall.O_NONBLOCK)
	must(add(et[0], syscall.EPOLLIN|syscall.EPOLLET&0xffffffff), "ADD et")
	syscall.Write(et[1], []byte("a"))
	if ev := epollWait(epfd, 0); len(ev) != 1 || ev[0].Fd != int32(et[0]) {
		fail("ET first wait: %+v", ev)
	}
	if ev := epollWait(epfd, 0); len(ev) != 0 {
		fail("ET second wait (no new edge): %+v", ev)
	}
	syscall.Write(et[1], []byte("b"))
	if ev := epollWait(epfd, 0); len(ev) != 1 {
		fail("ET after new write: %+v", ev)
	}
	syscall.EpollCtl(epfd, syscall.EPOLL_CTL_DEL, et[0], nil)
	ok("epoll edge-triggered")

	// One-shot.
	os1 := pipe2(syscall.O_NONBLOCK)
	must(add(os1[0], syscall.EPOLLIN|syscall.EPOLLONESHOT), "ADD oneshot")
	syscall.Write(os1[1], []byte("a"))
	if ev := epollWait(epfd, 0); len(ev) != 1 {
		fail("ONESHOT first: %+v", ev)
	}
	if ev := epollWait(epfd, 0); len(ev) != 0 {
		fail("ONESHOT disarmed: %+v", ev)
	}
	must(syscall.EpollCtl(epfd, syscall.EPOLL_CTL_MOD, os1[0], &syscall.EpollEvent{Events: syscall.EPOLLIN | syscall.EPOLLONESHOT, Fd: int32(os1[0])}), "MOD rearm")
	if ev := epollWait(epfd, 0); len(ev) != 1 {
		fail("ONESHOT re-armed: %+v", ev)
	}
	syscall.EpollCtl(epfd, syscall.EPOLL_CTL_DEL, os1[0], nil)
	ok("epoll oneshot + MOD re-arm")

	// Timeout: 50 ms with nothing ready → 0 events, not early.
	start := time.Now()
	if ev := epollWait(epfd, 50); len(ev) != 0 {
		fail("timeout wait: %+v", ev)
	}
	if d := time.Since(start); d < 50*time.Millisecond || d > 2*time.Second {
		fail("epoll_wait(50ms) took %v", d)
	}
	ok("epoll timeout")

	// Blocking wait (-1) woken by a writer.
	bw := pipe2(syscall.O_NONBLOCK)
	must(add(bw[0], syscall.EPOLLIN), "ADD bw")
	go func() {
		time.Sleep(20 * time.Millisecond)
		syscall.Write(bw[1], []byte("w"))
	}()
	if ev := epollWait(epfd, -1); len(ev) != 1 || ev[0].Fd != int32(bw[0]) {
		fail("blocking wait: %+v", ev)
	}
	syscall.Read(bw[0], b)
	syscall.EpollCtl(epfd, syscall.EPOLL_CTL_DEL, bw[0], nil)
	ok("epoll blocking wait")

	// HUP when the writer closes; ERR on the write end when the reader
	// closes; closed fds disappear from the set.
	h := pipe2(syscall.O_NONBLOCK)
	must(add(h[0], syscall.EPOLLIN), "ADD hup")
	syscall.Close(h[1])
	if ev := epollWait(epfd, 0); len(ev) != 1 || ev[0].Events&syscall.EPOLLHUP == 0 {
		fail("EPOLLHUP: %+v", ev)
	}
	syscall.Close(h[0])
	if ev := epollWait(epfd, 0); len(ev) != 0 {
		fail("closed fd still reported: %+v", ev)
	}
	e := pipe2(syscall.O_NONBLOCK)
	must(add(e[1], syscall.EPOLLOUT), "ADD err")
	if ev := epollWait(epfd, 0); len(ev) != 1 || ev[0].Events != syscall.EPOLLOUT {
		fail("EPOLLOUT on empty pipe: %+v", ev)
	}
	syscall.Close(e[0])
	if ev := epollWait(epfd, 0); len(ev) != 1 || ev[0].Events&syscall.EPOLLERR == 0 {
		fail("EPOLLERR: %+v", ev)
	}
	closeAll(e[1], lt[0], lt[1], et[0], et[1], os1[0], os1[1], bw[0], bw[1])
	ok("epoll HUP/ERR/auto-removal")

	// eventfd in epoll, as the Go runtime uses it.
	efd := eventfd(0, efdNonblock)
	must(add(efd, syscall.EPOLLIN), "ADD eventfd")
	writeU64(efd, 1)
	if ev := epollWait(epfd, 0); len(ev) != 1 || ev[0].Events != syscall.EPOLLIN {
		fail("eventfd in epoll: %+v", ev)
	}
	readU64(efd)
	if ev := epollWait(epfd, 0); len(ev) != 0 {
		fail("drained eventfd: %+v", ev)
	}
	closeAll(efd, epfd)
	ok("epoll + eventfd")
}

// ppoll via the raw syscall (arm64 has no poll).
func testPpoll() {
	p := pipe2(syscall.O_NONBLOCK)
	type pollfd struct {
		fd      int32
		events  int16
		revents int16
	}
	pfd := []pollfd{{fd: int32(p[0]), events: 1 /* POLLIN */}, {fd: 999, events: 1}}
	var ts syscall.Timespec
	n, _, e := syscall.Syscall6(syscall.SYS_PPOLL, uintptr(unsafe.Pointer(&pfd[0])), 2, uintptr(unsafe.Pointer(&ts)), 0, 0, 0)
	if e != 0 || n != 1 || pfd[0].revents != 0 || pfd[1].revents != 0x20 /* POLLNVAL */ {
		fail("ppoll empty: n=%d e=%v %+v", n, e, pfd)
	}
	syscall.Write(p[1], []byte("x"))
	n, _, e = syscall.Syscall6(syscall.SYS_PPOLL, uintptr(unsafe.Pointer(&pfd[0])), 1, 0, 0, 0, 0)
	if e != 0 || n != 1 || pfd[0].revents != 1 {
		fail("ppoll ready: n=%d e=%v %+v", n, e, pfd)
	}
	closeAll(p[0], p[1])
	ok("ppoll")
}

func fdSet(s *syscall.FdSet, fd int)        { s.Bits[fd/64] |= 1 << (uint(fd) % 64) }
func fdIsSet(s *syscall.FdSet, fd int) bool { return s.Bits[fd/64]&(1<<(uint(fd)%64)) != 0 }

// syscall.Select: SYS_SELECT on amd64, pselect6 on arm64.
func testSelect() {
	p := pipe2(syscall.O_NONBLOCK)
	nfd := max(p[0], p[1]) + 1
	var r, w syscall.FdSet
	fdSet(&r, p[0])
	fdSet(&w, p[1])
	n, err := syscall.Select(nfd, &r, &w, nil, &syscall.Timeval{})
	if err != nil || n != 1 || fdIsSet(&r, p[0]) || !fdIsSet(&w, p[1]) {
		fail("select empty pipe: n=%d err=%v r=%x w=%x", n, err, r.Bits[0], w.Bits[0])
	}
	syscall.Write(p[1], []byte("x"))
	r, w = syscall.FdSet{}, syscall.FdSet{}
	fdSet(&r, p[0])
	fdSet(&w, p[1])
	n, err = syscall.Select(nfd, &r, &w, nil, nil)
	if err != nil || n != 2 || !fdIsSet(&r, p[0]) || !fdIsSet(&w, p[1]) {
		fail("select ready: n=%d err=%v r=%x w=%x", n, err, r.Bits[0], w.Bits[0])
	}
	// A timeout: nothing readable on q.
	q := pipe2(0)
	r = syscall.FdSet{}
	fdSet(&r, q[0])
	tv := syscall.Timeval{Usec: 50000}
	start := time.Now()
	n, err = syscall.Select(q[0]+1, &r, nil, nil, &tv)
	if d := time.Since(start); err != nil || n != 0 || r.Bits[0] != 0 || d < 50*time.Millisecond {
		fail("select timeout: n=%d err=%v r=%x after %v", n, err, r.Bits[0], d)
	}
	// Linux writes the time left back (amd64 passes tv straight through).
	if runtime.GOARCH == "amd64" && (tv.Sec != 0 || tv.Usec != 0) {
		fail("select timeout: time left %+v, want 0", tv)
	}
	// A reader blocked in select is woken by a write from another thread.
	go func() {
		time.Sleep(20 * time.Millisecond)
		syscall.Write(q[1], []byte("y"))
	}()
	r = syscall.FdSet{}
	fdSet(&r, q[0])
	tv = syscall.Timeval{Sec: 5}
	n, err = syscall.Select(q[0]+1, &r, nil, nil, &tv)
	if err != nil || n != 1 || !fdIsSet(&r, q[0]) {
		fail("select wake-up: n=%d err=%v r=%x", n, err, r.Bits[0])
	}
	if runtime.GOARCH == "amd64" && (tv.Sec < 4 || tv.Sec > 5) {
		fail("select wake-up: time left %+v", tv)
	}
	// A descriptor that is not open, and a negative nfds.
	r = syscall.FdSet{}
	fdSet(&r, 900)
	_, err = syscall.Select(901, &r, nil, nil, &syscall.Timeval{})
	wantErr(err, syscall.EBADF, "select closed fd")
	_, err = syscall.Select(-1, nil, nil, nil, &syscall.Timeval{})
	wantErr(err, syscall.EINVAL, "select nfds -1")
	closeAll(p[0], p[1], q[0], q[1])
	ok("select")
}

func testTimers() {
	start := time.Now()
	time.Sleep(30 * time.Millisecond)
	if d := time.Since(start); d < 30*time.Millisecond {
		fail("time.Sleep(30ms) took %v", d)
	}
	select {
	case <-time.After(20 * time.Millisecond):
	case <-time.After(5 * time.Second):
		fail("time.After(20ms) did not fire")
	}
	tk := time.NewTicker(10 * time.Millisecond)
	for i := 0; i < 5; i++ {
		<-tk.C
	}
	tk.Stop()
	ok("time.Sleep/After/Ticker")
}

func main() {
	testOsPipe()
	testRawPipe()
	testEventfd()
	testEpoll()
	testPpoll()
	testSelect()
	testTimers()
	fmt.Println("PASS fdtest")
}
