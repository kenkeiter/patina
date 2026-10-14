// sockets exercises the Phase 4 TCP/IP stack over loopback: TCP over
// 127.0.0.1, [::1] and a dual-stack listener, many concurrent connections,
// large transfers, UDP, error paths (ECONNREFUSED, EOF, EPIPE/ECONNRESET,
// deadlines), half-close, SO_REUSEADDR, raw socket syscalls and an HTTP
// round trip. Prints "ok <case>" per check, then "PASS sockets" (exit 0),
// or "FAIL sockets: ..." (exit 1).
package main

import (
	"bytes"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"math/rand"
	"net"
	"net/http"
	"os"
	"sync"
	"syscall"
	"time"
	"unsafe"
)

func fail(format string, a ...any) {
	fmt.Printf("FAIL sockets: "+format+"\n", a...)
	os.Exit(1)
}

func ok(name string) { fmt.Println("ok", name) }

func must(err error, what string) {
	if err != nil {
		fail("%s: %v", what, err)
	}
}

// echoServer accepts connections on ln and echoes each one until EOF.
func echoServer(ln net.Listener) {
	for {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		go func() {
			defer c.Close()
			io.Copy(c, c)
		}()
	}
}

func echoOnce(network, addr, msg string) {
	c, err := net.DialTimeout(network, addr, 5*time.Second)
	must(err, "dial "+addr)
	defer c.Close()
	c.SetDeadline(time.Now().Add(10 * time.Second))
	_, err = io.WriteString(c, msg)
	must(err, "write "+addr)
	buf := make([]byte, len(msg))
	_, err = io.ReadFull(c, buf)
	must(err, "read "+addr)
	if string(buf) != msg {
		fail("echo %s: got %q want %q", addr, buf, msg)
	}
}

func tcpEcho() {
	ln, err := net.Listen("tcp4", "127.0.0.1:0")
	must(err, "listen tcp4")
	go echoServer(ln)
	echoOnce("tcp4", ln.Addr().String(), "hello over 127.0.0.1")
	ln.Close()
	ok("tcp echo 127.0.0.1")

	ln6, err := net.Listen("tcp6", "[::1]:0")
	must(err, "listen tcp6")
	go echoServer(ln6)
	echoOnce("tcp6", ln6.Addr().String(), "hello over [::1]")
	ln6.Close()
	ok("tcp echo [::1]")

	// Dual stack: ":0" is AF_INET6 [::] with V6ONLY=0.
	ds, err := net.Listen("tcp", ":0")
	must(err, "listen :0")
	port := ds.Addr().(*net.TCPAddr).Port
	got := make(chan net.Addr, 1)
	go func() {
		c, err := ds.Accept()
		if err != nil {
			got <- nil
			return
		}
		got <- c.RemoteAddr()
		go func() {
			io.Copy(c, c)
			c.Close()
		}()
		echoServer(ds)
	}()
	echoOnce("tcp", fmt.Sprintf("127.0.0.1:%d", port), "dual stack")
	ra := <-got
	if ra == nil {
		fail("dual-stack accept failed")
	}
	if ip := ra.(*net.TCPAddr).IP; !ip.Equal(net.IPv4(127, 0, 0, 1)) || ip.To4() == nil {
		fail("dual-stack RemoteAddr %v, want 127.0.0.1", ra)
	}
	echoOnce("tcp", fmt.Sprintf("[::1]:%d", port), "dual stack v6")
	ds.Close()
	ok("tcp dual-stack listener reached over 127.0.0.1 (peer " + ra.String() + ")")
}

func concurrent() {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(err, "listen")
	defer ln.Close()
	go echoServer(ln)
	const n = 50
	var wg sync.WaitGroup
	errc := make(chan error, n)
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			c, err := net.DialTimeout("tcp", ln.Addr().String(), 10*time.Second)
			if err != nil {
				errc <- fmt.Errorf("dial %d: %w", i, err)
				return
			}
			defer c.Close()
			c.SetDeadline(time.Now().Add(20 * time.Second))
			msg := bytes.Repeat([]byte{byte(i)}, 1000+i*37)
			if _, err := c.Write(msg); err != nil {
				errc <- fmt.Errorf("write %d: %w", i, err)
				return
			}
			buf := make([]byte, len(msg))
			if _, err := io.ReadFull(c, buf); err != nil {
				errc <- fmt.Errorf("read %d: %w", i, err)
				return
			}
			if !bytes.Equal(buf, msg) {
				errc <- fmt.Errorf("conn %d: data mismatch", i)
			}
		}(i)
	}
	wg.Wait()
	close(errc)
	for err := range errc {
		fail("concurrent: %v", err)
	}
	ok("50 concurrent connections through one listener")
}

func bulk() {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(err, "listen")
	defer ln.Close()
	go echoServer(ln)
	c, err := net.Dial("tcp", ln.Addr().String())
	must(err, "dial")
	defer c.Close()
	c.SetDeadline(time.Now().Add(120 * time.Second))
	const size = 8 << 20
	data := make([]byte, size)
	rand.New(rand.NewSource(1)).Read(data)
	want := sha256.Sum256(data)
	werr := make(chan error, 1)
	go func() {
		_, err := c.Write(data)
		werr <- err
	}()
	h := sha256.New()
	n, err := io.CopyN(h, c, size)
	if err != nil {
		fail("bulk read after %d bytes: %v", n, err)
	}
	must(<-werr, "bulk write")
	if !bytes.Equal(h.Sum(nil), want[:]) {
		fail("bulk checksum mismatch")
	}
	ok("8 MiB each direction, sha256 match")
}

func udp() {
	srv, err := net.ListenPacket("udp", "127.0.0.1:0")
	must(err, "listen udp")
	defer srv.Close()
	go func() {
		buf := make([]byte, 2048)
		for {
			n, from, err := srv.ReadFrom(buf)
			if err != nil {
				return
			}
			srv.WriteTo(buf[:n], from)
		}
	}()
	cl, err := net.ListenPacket("udp", "127.0.0.1:0")
	must(err, "listen udp client")
	defer cl.Close()
	cl.SetDeadline(time.Now().Add(5 * time.Second))
	_, err = cl.WriteTo([]byte("ping"), srv.LocalAddr())
	must(err, "udp WriteTo")
	buf := make([]byte, 64)
	n, from, err := cl.ReadFrom(buf)
	must(err, "udp ReadFrom")
	if string(buf[:n]) != "ping" || from.String() != srv.LocalAddr().String() {
		fail("udp echo: got %q from %v", buf[:n], from)
	}
	ok("udp ReadFrom/WriteTo")

	cc, err := net.DialUDP("udp", nil, srv.LocalAddr().(*net.UDPAddr))
	must(err, "DialUDP")
	defer cc.Close()
	cc.SetDeadline(time.Now().Add(5 * time.Second))
	_, err = cc.Write([]byte("connected"))
	must(err, "connected udp write")
	n, err = cc.Read(buf)
	must(err, "connected udp read")
	if string(buf[:n]) != "connected" {
		fail("connected udp: got %q", buf[:n])
	}
	// Over [::1] too.
	s6, err := net.ListenPacket("udp6", "[::1]:0")
	must(err, "listen udp6")
	defer s6.Close()
	c6, err := net.DialUDP("udp6", nil, s6.LocalAddr().(*net.UDPAddr))
	must(err, "DialUDP6")
	defer c6.Close()
	_, err = c6.Write([]byte("v6"))
	must(err, "udp6 write")
	s6.SetDeadline(time.Now().Add(5 * time.Second))
	n, from, err = s6.ReadFrom(buf)
	must(err, "udp6 read")
	if string(buf[:n]) != "v6" || from.String() != c6.LocalAddr().String() {
		fail("udp6: got %q from %v (client %v)", buf[:n], from, c6.LocalAddr())
	}
	ok("udp connected socket, udp6")
}

func errorsCases() {
	// A port nobody listens on: bind, note the port, close.
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(err, "listen")
	addr := ln.Addr().String()
	ln.Close()
	_, err = net.DialTimeout("tcp", addr, 5*time.Second)
	if !errors.Is(err, syscall.ECONNREFUSED) {
		fail("dial closed port: %v, want ECONNREFUSED", err)
	}
	ok("connect to closed port: ECONNREFUSED")

	ln, err = net.Listen("tcp", "127.0.0.1:0")
	must(err, "listen")
	defer ln.Close()
	acc := make(chan net.Conn, 4)
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			acc <- c
		}
	}()

	// Read after the peer closed: EOF.
	c, err := net.Dial("tcp", ln.Addr().String())
	must(err, "dial")
	s := <-acc
	io.WriteString(s, "bye")
	s.Close()
	c.SetReadDeadline(time.Now().Add(5 * time.Second))
	b, err := io.ReadAll(c)
	if err != nil || string(b) != "bye" {
		fail("read after peer close: %q, %v", b, err)
	}
	n, err := c.Read(make([]byte, 1))
	if n != 0 || err != io.EOF {
		fail("read at EOF: %d, %v", n, err)
	}
	ok("read after peer close: io.EOF")

	// Write after the peer closed: EPIPE or ECONNRESET, no SIGPIPE death.
	var werr error
	for i := 0; i < 200 && werr == nil; i++ {
		_, werr = c.Write([]byte("data after close"))
		time.Sleep(5 * time.Millisecond)
	}
	if !errors.Is(werr, syscall.EPIPE) && !errors.Is(werr, syscall.ECONNRESET) {
		fail("write after peer close: %v", werr)
	}
	c.Close()
	ok("write after peer close: " + werr.(*net.OpError).Err.Error())

	// Read deadline.
	c, err = net.Dial("tcp", ln.Addr().String())
	must(err, "dial")
	s = <-acc
	c.SetReadDeadline(time.Now().Add(200 * time.Millisecond))
	_, err = c.Read(make([]byte, 1))
	var ne net.Error
	if !errors.As(err, &ne) || !ne.Timeout() {
		fail("read deadline: %v", err)
	}
	ok("SetReadDeadline: timeout")

	// Half close: CloseWrite → peer sees EOF and can still answer.
	go func(s net.Conn) {
		b, _ := io.ReadAll(s)
		s.Write(append([]byte("got:"), b...))
		s.Close()
	}(s)
	c.SetReadDeadline(time.Now().Add(5 * time.Second))
	io.WriteString(c, "half")
	must(c.(*net.TCPConn).CloseWrite(), "CloseWrite")
	b, err = io.ReadAll(c)
	if err != nil || string(b) != "got:half" {
		fail("half close: %q, %v", b, err)
	}
	c.Close()
	ok("CloseWrite: peer sees EOF, still writes back")

	// SO_REUSEADDR (Go sets it on listeners): rebind right after close,
	// with a connection just closed on that port.
	l1, err := net.Listen("tcp", "127.0.0.1:0")
	must(err, "listen")
	port := l1.Addr().(*net.TCPAddr).Port
	go func() {
		if c, err := l1.Accept(); err == nil {
			c.Close()
		}
	}()
	if c, err := net.Dial("tcp", l1.Addr().String()); err == nil {
		io.ReadAll(c)
		c.Close()
	}
	l1.Close()
	l2, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", port))
	must(err, "rebind after close")
	l2.Close()
	// Without the listener closed it must fail.
	l3, err := net.Listen("tcp", "127.0.0.1:0")
	must(err, "listen")
	if _, err := net.Listen("tcp", l3.Addr().String()); !errors.Is(err, syscall.EADDRINUSE) {
		fail("second listen on a live port: %v, want EADDRINUSE", err)
	}
	l3.Close()
	ok("SO_REUSEADDR rebind after close; EADDRINUSE while listening")
}

func sockaddrPort(sa syscall.Sockaddr) int {
	switch a := sa.(type) {
	case *syscall.SockaddrInet4:
		return a.Port
	case *syscall.SockaddrInet6:
		return a.Port
	}
	return -1
}

// FIONREAD (SIOCINQ), the same on amd64 and arm64.
const fionreadReq = 0x541B

func fionread(fd int) int {
	var n int32
	_, _, e := syscall.Syscall(syscall.SYS_IOCTL, uintptr(fd), fionreadReq, uintptr(unsafe.Pointer(&n)))
	if e != 0 {
		fail("FIONREAD: %v", e)
	}
	return int(n)
}

func rawSyscalls() {
	lfd, err := syscall.Socket(syscall.AF_INET, syscall.SOCK_STREAM, 0)
	must(err, "socket")
	defer syscall.Close(lfd)
	must(syscall.Bind(lfd, &syscall.SockaddrInet4{Addr: [4]byte{127, 0, 0, 1}}), "bind")
	must(syscall.Listen(lfd, 8), "listen")
	lsa, err := syscall.Getsockname(lfd)
	must(err, "getsockname")
	port := sockaddrPort(lsa)
	if v, err := syscall.GetsockoptInt(lfd, syscall.SOL_SOCKET, syscall.SO_ACCEPTCONN); err != nil || v != 1 {
		fail("SO_ACCEPTCONN: %d, %v", v, err)
	}

	// Nonblocking connect: EINPROGRESS, then epoll says writable.
	cfd, err := syscall.Socket(syscall.AF_INET, syscall.SOCK_STREAM|syscall.SOCK_NONBLOCK|syscall.SOCK_CLOEXEC, 0)
	must(err, "socket")
	defer syscall.Close(cfd)
	err = syscall.Connect(cfd, &syscall.SockaddrInet4{Addr: [4]byte{127, 0, 0, 1}, Port: port})
	if err != syscall.EINPROGRESS {
		fail("nonblocking connect: %v, want EINPROGRESS", err)
	}
	ep, err := syscall.EpollCreate1(syscall.EPOLL_CLOEXEC)
	must(err, "epoll_create1")
	defer syscall.Close(ep)
	must(syscall.EpollCtl(ep, syscall.EPOLL_CTL_ADD, cfd, &syscall.EpollEvent{Events: syscall.EPOLLOUT, Fd: int32(cfd)}), "epoll_ctl")
	evs := make([]syscall.EpollEvent, 4)
	n, err := syscall.EpollWait(ep, evs, 5000)
	if err != nil || n != 1 || evs[0].Events&syscall.EPOLLOUT == 0 {
		fail("epoll after connect: n=%d ev=%#x err=%v", n, evs[0].Events, err)
	}
	if v, err := syscall.GetsockoptInt(cfd, syscall.SOL_SOCKET, syscall.SO_ERROR); err != nil || v != 0 {
		fail("SO_ERROR: %d, %v", v, err)
	}
	ok("nonblocking connect: EINPROGRESS, EPOLLOUT, SO_ERROR=0")

	// accept4 with SOCK_NONBLOCK|SOCK_CLOEXEC.
	afd, asa, err := syscall.Accept4(lfd, syscall.SOCK_NONBLOCK|syscall.SOCK_CLOEXEC)
	must(err, "accept4")
	defer syscall.Close(afd)
	fl, _, _ := syscall.Syscall(syscall.SYS_FCNTL, uintptr(afd), syscall.F_GETFL, 0)
	fd, _, _ := syscall.Syscall(syscall.SYS_FCNTL, uintptr(afd), syscall.F_GETFD, 0)
	if fl&syscall.O_NONBLOCK == 0 || fd&syscall.FD_CLOEXEC == 0 {
		fail("accept4 flags: fl=%#x fd=%#x", fl, fd)
	}
	csa, _ := syscall.Getsockname(cfd)
	if sockaddrPort(asa) != sockaddrPort(csa) {
		fail("accept4 peer port %d, client port %d", sockaddrPort(asa), sockaddrPort(csa))
	}
	ok("accept4 SOCK_NONBLOCK|SOCK_CLOEXEC, peer address")

	// TCP_NODELAY set/get.
	must(syscall.SetsockoptInt(cfd, syscall.IPPROTO_TCP, syscall.TCP_NODELAY, 1), "set TCP_NODELAY")
	if v, err := syscall.GetsockoptInt(cfd, syscall.IPPROTO_TCP, syscall.TCP_NODELAY); err != nil || v != 1 {
		fail("TCP_NODELAY: %d, %v", v, err)
	}
	if v, err := syscall.GetsockoptInt(cfd, syscall.SOL_SOCKET, syscall.SO_TYPE); err != nil || v != syscall.SOCK_STREAM {
		fail("SO_TYPE: %d, %v", v, err)
	}
	ok("TCP_NODELAY set/get, SO_TYPE")

	// MSG_PEEK leaves data; FIONREAD counts it.
	_, err = syscall.Write(cfd, []byte("peekaboo"))
	must(err, "write")
	deadline := time.Now().Add(5 * time.Second)
	for fionread(afd) < 8 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if n := fionread(afd); n != 8 {
		fail("FIONREAD = %d, want 8", n)
	}
	buf := make([]byte, 16)
	n, _, err = syscall.Recvfrom(afd, buf, syscall.MSG_PEEK)
	if err != nil || string(buf[:n]) != "peekaboo" {
		fail("MSG_PEEK: %q, %v", buf[:n], err)
	}
	n, _, err = syscall.Recvfrom(afd, buf, 0)
	if err != nil || string(buf[:n]) != "peekaboo" {
		fail("recv after peek: %q, %v", buf[:n], err)
	}
	if _, _, err = syscall.Recvfrom(afd, buf, syscall.MSG_DONTWAIT); err != syscall.EAGAIN {
		fail("empty MSG_DONTWAIT recv: %v, want EAGAIN", err)
	}
	ok("MSG_PEEK leaves data, FIONREAD, MSG_DONTWAIT")

	// MSG_NOSIGNAL send after shutdown(SHUT_WR): EPIPE, no signal.
	must(syscall.Shutdown(cfd, syscall.SHUT_WR), "shutdown")
	if err := syscall.Sendto(cfd, []byte("x"), syscall.MSG_NOSIGNAL, nil); err != syscall.EPIPE {
		fail("send after SHUT_WR: %v, want EPIPE", err)
	}
	ok("send after SHUT_WR: EPIPE")

	// IPV6_V6ONLY=1 refuses IPv4-mapped addresses.
	s6, err := syscall.Socket(syscall.AF_INET6, syscall.SOCK_STREAM, 0)
	must(err, "socket6")
	defer syscall.Close(s6)
	must(syscall.SetsockoptInt(s6, syscall.IPPROTO_IPV6, syscall.IPV6_V6ONLY, 1), "V6ONLY")
	if v, _ := syscall.GetsockoptInt(s6, syscall.IPPROTO_IPV6, syscall.IPV6_V6ONLY); v != 1 {
		fail("V6ONLY get = %d", v)
	}
	mapped := [16]byte{10: 0xff, 11: 0xff, 12: 127, 15: 1}
	if err := syscall.Connect(s6, &syscall.SockaddrInet6{Addr: mapped, Port: port}); err == nil {
		fail("V6ONLY connect to ::ffff:127.0.0.1 succeeded")
	}
	if err := syscall.Bind(s6, &syscall.SockaddrInet6{Addr: mapped}); err == nil {
		fail("V6ONLY bind to ::ffff:127.0.0.1 succeeded")
	}
	ok("IPV6_V6ONLY=1 refuses ::ffff:127.0.0.1")

	// Unsupported families and non-sockets.
	if _, err := syscall.Socket(syscall.AF_PACKET, syscall.SOCK_RAW, 0); err != syscall.EAFNOSUPPORT {
		fail("AF_PACKET: %v", err)
	}
	if _, err := syscall.Getsockname(0); err != syscall.ENOTSOCK {
		fail("getsockname(stdin): %v", err)
	}
	ok("EAFNOSUPPORT, ENOTSOCK")
}

func httpRoundTrip() {
	body := bytes.Repeat([]byte("0123456789abcdef"), 1<<16) // 1 MiB
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(err, "listen")
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Write(body)
	})}
	go srv.Serve(ln)
	defer srv.Close()
	cl := &http.Client{Timeout: 30 * time.Second}
	resp, err := cl.Get("http://" + ln.Addr().String() + "/big")
	must(err, "http get")
	got, err := io.ReadAll(resp.Body)
	resp.Body.Close()
	if err != nil || resp.StatusCode != 200 || !bytes.Equal(got, body) {
		fail("http: status %d, %d bytes, %v", resp.StatusCode, len(got), err)
	}
	ok("http.Server + http.Get, 1 MiB body")
}

func main() {
	tcpEcho()
	concurrent()
	bulk()
	udp()
	errorsCases()
	rawSyscalls()
	httpRoundTrip()
	fmt.Println("PASS sockets")
}
