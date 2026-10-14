// vsockapp exercises AF_VSOCK stream sockets (decision D76) the way
// github.com/mdlayher/vsock does, with no third-party code: raw
// socket/bind/connect/accept4 on a sockaddr_vm, the fd handed to the Go
// runtime poller with os.NewFile, and the local CID from
// IOCTL_VM_SOCKETS_GET_LOCAL_CID on /dev/vsock.
//
//	vsockapp [-port 5000] [-conns 1] [-hostport 1] [-quit]
//	vsockapp -novsock
//
// Default mode (a virtio-vsock device is present): local CID; socket()
// errors for SOCK_DGRAM / SOCK_SEQPACKET / a bad protocol; port 4242
// reserved; loopback echo (VMADDR_CID_LOCAL and the guest's own CID) with
// non-blocking connect + SO_ERROR; a guest->host connect to an unlistened
// host port (blocking and non-blocking: ECONNRESET, as Linux reports the
// host's RST); then it listens on -port, prints
// "vsockapp: listening cid C port P", accepts -conns connections from the
// host and echoes each until EOF ("ok echo N bytes from cid 2 port X");
// with -quit it then waits for one more host connection to end.
//
// -novsock, or no device found (socket(AF_VSOCK) is EAFNOSUPPORT, e.g.
// QEMU on macOS): socket(AF_VSOCK) must be EAFNOSUPPORT and opening
// /dev/vsock must fail.
//
// Prints "ok <case>" per check, then "PASS vsockapp" (exit 0), or
// "FAIL vsockapp: ..." (exit 1).
package main

import (
	"bytes"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"sync"
	"syscall"
	"time"
	"unsafe"
)

const (
	afVsock          = 40
	cidAny           = 0xFFFFFFFF
	cidLocal         = 1
	cidHost          = 2
	portAny          = 0xFFFFFFFF
	ioctlGetLocalCID = 0x7b9
	ctlPort          = 4242
)

func fail(format string, a ...any) {
	fmt.Printf("FAIL vsockapp: "+format+"\n", a...)
	os.Exit(1)
}

func ok(format string, a ...any) { fmt.Printf("ok "+format+"\n", a...) }

func must(err error, what string) {
	if err != nil {
		fail("%s: %v", what, err)
	}
}

// sockaddrVM is struct sockaddr_vm.
type sockaddrVM struct {
	family    uint16
	reserved1 uint16
	port      uint32
	cid       uint32
	flags     uint8
	zero      [3]uint8
}

func socket(typ, proto int) (int, error) {
	fd, _, e := syscall.RawSyscall(syscall.SYS_SOCKET, afVsock, uintptr(typ), uintptr(proto))
	if e != 0 {
		return -1, e
	}
	return int(fd), nil
}

func sockcall(nr uintptr, fd int, cid, port uint32) error {
	sa := sockaddrVM{family: afVsock, cid: cid, port: port}
	_, _, e := syscall.Syscall(nr, uintptr(fd), uintptr(unsafe.Pointer(&sa)), unsafe.Sizeof(sa))
	if e != 0 {
		return e
	}
	return nil
}

func bind(fd int, cid, port uint32) error    { return sockcall(syscall.SYS_BIND, fd, cid, port) }
func connect(fd int, cid, port uint32) error { return sockcall(syscall.SYS_CONNECT, fd, cid, port) }

func name(nr uintptr, fd int) (cid, port uint32, err error) {
	var sa sockaddrVM
	l := uint32(unsafe.Sizeof(sa))
	_, _, e := syscall.RawSyscall(nr, uintptr(fd), uintptr(unsafe.Pointer(&sa)), uintptr(unsafe.Pointer(&l)))
	if e != 0 {
		return 0, 0, e
	}
	if sa.family != afVsock || l != 16 {
		return 0, 0, fmt.Errorf("bad sockaddr: family %d len %d", sa.family, l)
	}
	return sa.cid, sa.port, nil
}

func sockname(fd int) (uint32, uint32, error) { return name(syscall.SYS_GETSOCKNAME, fd) }
func peername(fd int) (uint32, uint32, error) { return name(syscall.SYS_GETPEERNAME, fd) }

// localCID is mdlayher/vsock's ContextID: the ioctl on /dev/vsock.
func localCID() (uint32, error) {
	f, err := os.Open("/dev/vsock")
	if err != nil {
		return 0, err
	}
	defer f.Close()
	var cid uint32
	_, _, e := syscall.Syscall(syscall.SYS_IOCTL, f.Fd(), ioctlGetLocalCID, uintptr(unsafe.Pointer(&cid)))
	if e != 0 {
		return 0, e
	}
	return cid, nil
}

// conn is a connected socket on the runtime poller.
type conn struct {
	f                 *os.File
	peerCID, peerPort uint32
}

// listener accepts on the poller (mdlayher/socket's Accept loop).
type listener struct {
	f    *os.File
	port uint32
}

func listen(cid, port uint32, backlog int) (*listener, error) {
	fd, err := socket(syscall.SOCK_STREAM|syscall.SOCK_NONBLOCK|syscall.SOCK_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	if err := bind(fd, cid, port); err != nil {
		syscall.Close(fd)
		return nil, fmt.Errorf("bind: %w", err)
	}
	if err := syscall.Listen(fd, backlog); err != nil {
		syscall.Close(fd)
		return nil, fmt.Errorf("listen: %w", err)
	}
	_, p, err := sockname(fd)
	if err != nil {
		syscall.Close(fd)
		return nil, err
	}
	return &listener{f: os.NewFile(uintptr(fd), "vsock-listener"), port: p}, nil
}

func (l *listener) accept() (*conn, error) {
	rc, err := l.f.SyscallConn()
	if err != nil {
		return nil, err
	}
	var nfd int
	var aerr error
	err = rc.Read(func(fd uintptr) bool {
		// Raw: syscall.Accept4 decodes the peer address and fails (closing
		// the new fd) for families it does not know, AF_VSOCK included.
		r, _, e := syscall.Syscall6(syscall.SYS_ACCEPT4, fd, 0, 0, syscall.SOCK_NONBLOCK|syscall.SOCK_CLOEXEC, 0, 0)
		nfd, aerr = int(r), nil
		if e != 0 {
			aerr = e
		}
		return aerr != syscall.EAGAIN
	})
	if err == nil {
		err = aerr
	}
	if err != nil {
		return nil, err
	}
	c, p, err := peername(nfd)
	if err != nil {
		syscall.Close(nfd)
		return nil, fmt.Errorf("getpeername: %w", err)
	}
	return &conn{f: os.NewFile(uintptr(nfd), "vsock-conn"), peerCID: c, peerPort: p}, nil
}

// dial connects non-blocking and waits for writability on the poller, then
// reads SO_ERROR (what Go's net package and mdlayher/vsock do).
func dial(cid, port uint32, timeout time.Duration) (*conn, error) {
	fd, err := socket(syscall.SOCK_STREAM|syscall.SOCK_NONBLOCK|syscall.SOCK_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	f := os.NewFile(uintptr(fd), "vsock-conn")
	err = connect(fd, cid, port)
	if err != syscall.EINPROGRESS {
		f.Close()
		if err == nil {
			return nil, errors.New("connect: completed at once (want EINPROGRESS)")
		}
		return nil, err
	}
	rc, err := f.SyscallConn()
	if err != nil {
		f.Close()
		return nil, err
	}
	f.SetWriteDeadline(time.Now().Add(timeout))
	var soerr int
	var gerr error
	err = rc.Write(func(fd uintptr) bool {
		soerr, gerr = syscall.GetsockoptInt(int(fd), syscall.SOL_SOCKET, syscall.SO_ERROR)
		if gerr != nil || soerr != 0 {
			return true
		}
		// Connected once getpeername works; spurious wake-ups retry.
		_, _, perr := peername(int(fd))
		return perr == nil
	})
	f.SetWriteDeadline(time.Time{})
	if err == nil {
		err = gerr
	}
	if err == nil && soerr != 0 {
		err = syscall.Errno(soerr)
	}
	if err != nil {
		f.Close()
		return nil, err
	}
	c, p, _ := peername(fd)
	return &conn{f: f, peerCID: c, peerPort: p}, nil
}

// sockname without f.Fd(), which would make the descriptor blocking.
func (c *conn) sockname() (cid, port uint32, err error) {
	rc, err := c.f.SyscallConn()
	if err != nil {
		return 0, 0, err
	}
	rc.Control(func(fd uintptr) { cid, port, err = sockname(int(fd)) })
	return
}

func (c *conn) Read(b []byte) (int, error)  { return c.f.Read(b) }
func (c *conn) Write(b []byte) (int, error) { return c.f.Write(b) }
func (c *conn) Close() error                { return c.f.Close() }
func (c *conn) CloseWrite() error {
	rc, err := c.f.SyscallConn()
	if err != nil {
		return err
	}
	var serr error
	rc.Control(func(fd uintptr) { serr = syscall.Shutdown(int(fd), syscall.SHUT_WR) })
	return serr
}

// echo copies c back to itself until EOF, then closes. Returns the count.
func echo(c *conn) (int64, error) {
	defer c.Close()
	buf := make([]byte, 32<<10)
	var n int64
	for {
		r, err := c.Read(buf)
		if r > 0 {
			if _, werr := c.Write(buf[:r]); werr != nil {
				return n, werr
			}
			n += int64(r)
		}
		if err == io.EOF {
			return n, nil
		}
		if err != nil {
			return n, err
		}
	}
}

func pattern(n int) []byte {
	b := make([]byte, n)
	x := uint32(2463534242)
	for i := range b {
		x ^= x << 13
		x ^= x >> 17
		x ^= x << 5
		b[i] = byte(x)
	}
	return b
}

// loopEcho runs an echo exchange of n bytes through a fresh listener,
// dialling cid.
func loopEcho(cid uint32, n int) {
	l, err := listen(cidAny, portAny, 4)
	must(err, "loopback listen")
	defer l.f.Close()
	done := make(chan error, 1)
	go func() {
		c, err := l.accept()
		if err != nil {
			done <- err
			return
		}
		if c.peerCID != cidLocal {
			done <- fmt.Errorf("accepted peer cid %d, want %d (loopback)", c.peerCID, cidLocal)
			return
		}
		_, err = echo(c)
		done <- err
	}()
	c, err := dial(cid, l.port, 5*time.Second)
	must(err, fmt.Sprintf("loopback dial cid %d port %d", cid, l.port))
	if c.peerCID != cid || c.peerPort != l.port {
		fail("loopback peer %d:%d, want %d:%d", c.peerCID, c.peerPort, cid, l.port)
	}
	want := pattern(n)
	go func() {
		c.Write(want)
		c.CloseWrite()
	}()
	got, err := io.ReadAll(c)
	must(err, "loopback read")
	c.Close()
	if !bytes.Equal(got, want) {
		fail("loopback echo via cid %d: got %d bytes, want %d (equal=%v)", cid, len(got), n, bytes.Equal(got, want))
	}
	must(<-done, "loopback server")
	ok("loopback-echo cid %d %d bytes", cid, n)
}

func expectErrno(err, want error, what string) {
	if !errors.Is(err, want) {
		fail("%s: got %v, want %v", what, err, want)
	}
	ok("%s -> %v", what, want)
}

func noVsock() {
	_, err := socket(syscall.SOCK_STREAM, 0)
	expectErrno(err, syscall.EAFNOSUPPORT, "socket(AF_VSOCK) without a device")
	_, err = localCID()
	if err == nil {
		fail("/dev/vsock ioctl worked without a device")
	}
	ok("/dev/vsock without a device: %v", err)
	fmt.Println("PASS vsockapp")
}

func main() {
	port := flag.Uint("port", 5000, "port to listen on for the host")
	conns := flag.Int("conns", 1, "host connections to accept and echo")
	hostPort := flag.Uint("hostport", 1, "an unlistened host port for the refused-connect check")
	novsock := flag.Bool("novsock", false, "expect no vsock device")
	quit := flag.Bool("quit", false, "after the echoes, wait for one more host connection to close before exiting")
	flag.Parse()
	if !*novsock {
		// No device (QEMU on macOS): run the no-device checks instead.
		if fd, err := socket(syscall.SOCK_STREAM, 0); err == syscall.EAFNOSUPPORT {
			fmt.Println("vsockapp: no vsock device")
			*novsock = true
		} else if err == nil {
			syscall.Close(fd)
		}
	}
	if *novsock {
		noVsock()
		return
	}

	cid, err := localCID()
	must(err, "IOCTL_VM_SOCKETS_GET_LOCAL_CID")
	if cid <= cidHost || cid == cidAny {
		fail("local cid %d is not a guest cid", cid)
	}
	ok("local-cid %d", cid)

	// socket() errors.
	_, err = socket(syscall.SOCK_DGRAM, 0)
	expectErrno(err, syscall.ENODEV, "socket(AF_VSOCK, SOCK_DGRAM)")
	_, err = socket(syscall.SOCK_SEQPACKET, 0)
	expectErrno(err, syscall.ESOCKTNOSUPPORT, "socket(AF_VSOCK, SOCK_SEQPACKET)")
	_, err = socket(syscall.SOCK_STREAM, 6)
	expectErrno(err, syscall.EPROTONOSUPPORT, "socket(AF_VSOCK, SOCK_STREAM, 6)")

	// The control channel's port is the kernel's.
	fd, err := socket(syscall.SOCK_STREAM, 0)
	must(err, "socket")
	expectErrno(bind(fd, cidAny, ctlPort), syscall.EADDRINUSE, "bind port 4242")
	// Binding another CID than ours / LOCAL / ANY.
	expectErrno(bind(fd, cidHost, 7000), syscall.EADDRNOTAVAIL, "bind cid 2")
	must(bind(fd, cid, portAny), "bind own cid, any port")
	c, p, err := sockname(fd)
	must(err, "getsockname")
	if c != cid || p <= 1023 || p == portAny {
		fail("getsockname after autobind: %d:%d", c, p)
	}
	ok("bind own cid -> ephemeral port %d", p)
	_, _, err = peername(fd)
	expectErrno(err, syscall.ENOTCONN, "getpeername unconnected")
	syscall.Close(fd)

	// Loopback, through VMADDR_CID_LOCAL and the guest's own CID.
	loopEcho(cidLocal, 256<<10)
	loopEcho(cid, 64<<10)

	// Guest -> host to a port nobody listens on: the host answers RST.
	fd, err = socket(syscall.SOCK_STREAM, 0)
	must(err, "socket")
	t0 := time.Now()
	err = connect(fd, cidHost, uint32(*hostPort))
	syscall.Close(fd)
	expectErrno(err, syscall.ECONNRESET, fmt.Sprintf("blocking connect host port %d (%s)", *hostPort, time.Since(t0).Round(time.Millisecond)))
	t0 = time.Now()
	_, err = dial(cidHost, uint32(*hostPort), 5*time.Second)
	expectErrno(err, syscall.ECONNRESET, fmt.Sprintf("non-blocking connect host port %d, SO_ERROR (%s)", *hostPort, time.Since(t0).Round(time.Millisecond)))

	// Serve the host.
	l, err := listen(cidAny, uint32(*port), 16)
	must(err, "listen")
	fmt.Printf("vsockapp: listening cid %d port %d\n", cid, l.port)
	var wg sync.WaitGroup
	errs := make(chan error, *conns)
	for i := 0; i < *conns; i++ {
		c, err := l.accept()
		must(err, "accept")
		if c.peerCID != cidHost {
			fail("host connection from cid %d, want %d", c.peerCID, cidHost)
		}
		lc, lp, err := c.sockname()
		must(err, "getsockname accepted")
		if lc != cid || lp != l.port {
			fail("accepted socket local %d:%d, want %d:%d", lc, lp, cid, l.port)
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			n, err := echo(c)
			if err != nil {
				errs <- fmt.Errorf("echo: %v after %d bytes", err, n)
				return
			}
			ok("echo %d bytes from cid %d port %d", n, c.peerCID, c.peerPort)
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		fail("%v", err)
	}
	if *quit {
		// Stay up until the host has read every echo: exiting powers the
		// VM off, and bytes still queued (in the guest or the VMM) would
		// be lost with it.
		c, err := l.accept()
		must(err, "accept quit connection")
		io.Copy(io.Discard, c)
		c.Close()
		ok("quit connection")
	}
	l.f.Close()
	fmt.Println("PASS vsockapp")
}
