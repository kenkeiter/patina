// unixsock exercises AF_UNIX sockets: stream listeners on paths and
// abstract names, error paths (ENOENT, ECONNREFUSED, ENXIO, EPIPE),
// unlink while listening, many concurrent connections, bulk transfers,
// half-close, deadlines, socketpair for every type, SCM_RIGHTS and
// SCM_CREDENTIALS, SO_PEERCRED, unixgram with sender addresses,
// MSG_TRUNC/MSG_PEEK/MSG_CTRUNC, backlog limits and HTTP over a unix
// listener. Prints "ok <case>" per check, then "PASS unixsock" (exit 0),
// or "FAIL unixsock: ..." (exit 1).
package main

import (
	"bytes"
	"context"
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
	fmt.Printf("FAIL unixsock: "+format+"\n", a...)
	os.Exit(1)
}

func ok(name string) { fmt.Println("ok", name) }

func must(err error, what string) {
	if err != nil {
		fail("%s: %v", what, err)
	}
}

func wantErrno(err error, want syscall.Errno, what string) {
	if !errors.Is(err, want) {
		fail("%s: got %v, want %v", what, err, want)
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

func echoOnce(c net.Conn, msg string) {
	c.SetDeadline(time.Now().Add(10 * time.Second))
	_, err := io.WriteString(c, msg)
	must(err, "write")
	buf := make([]byte, len(msg))
	_, err = io.ReadFull(c, buf)
	must(err, "read")
	if string(buf) != msg {
		fail("echo: got %q want %q", buf, msg)
	}
}

func dialEcho(addr, msg string) {
	c, err := net.DialTimeout("unix", addr, 5*time.Second)
	must(err, "dial "+addr)
	defer c.Close()
	echoOnce(c, msg)
}

func streamPath() {
	path := "/tmp/echo.sock"
	ln, err := net.Listen("unix", path)
	must(err, "listen "+path)
	defer ln.Close()
	go echoServer(ln)
	c, err := net.Dial("unix", path)
	must(err, "dial")
	echoOnce(c, "hello over a unix socket")
	if got := c.RemoteAddr().String(); got != path {
		fail("RemoteAddr %q want %q", got, path)
	}
	// Unnamed: addrlen 2, which Go's anyToSockaddr shows as "@" (as on
	// Linux).
	if got := c.LocalAddr().String(); got != "@" && got != "" {
		fail("LocalAddr of unbound client %q", got)
	}
	c.Close()
	if got := ln.Addr().String(); got != path {
		fail("listener Addr %q", got)
	}
	ok("stream echo on " + path)

	fi, err := os.Stat(path)
	must(err, "stat")
	if fi.Mode()&os.ModeSocket == 0 || fi.Mode().Perm() != 0o755 {
		fail("stat mode %v want socket 0755 (0777 &^ umask 022)", fi.Mode())
	}
	ok("stat shows ModeSocket")

	_, err = os.Open(path)
	wantErrno(err, syscall.ENXIO, "open socket")
	ok("open of a socket node is ENXIO")

	_, err = net.Listen("unix", path)
	wantErrno(err, syscall.EADDRINUSE, "second bind")
	ok("bind to an existing path is EADDRINUSE")
}

func dialErrors() {
	_, err := net.Dial("unix", "/tmp/no-such.sock")
	wantErrno(err, syscall.ENOENT, "dial missing")
	ok("dial missing path is ENOENT")

	path := "/tmp/closed.sock"
	ln, err := net.ListenUnix("unix", &net.UnixAddr{Name: path, Net: "unix"})
	must(err, "listen")
	ln.SetUnlinkOnClose(false)
	ln.Close()
	if _, err := os.Stat(path); err != nil {
		fail("node gone after close without unlink: %v", err)
	}
	_, err = net.Dial("unix", path)
	wantErrno(err, syscall.ECONNREFUSED, "dial closed listener")
	ok("dial a closed listener's path is ECONNREFUSED")

	must(os.WriteFile("/tmp/plain", []byte("x"), 0o644), "write file")
	_, err = net.Dial("unix", "/tmp/plain")
	wantErrno(err, syscall.ECONNREFUSED, "dial regular file")
	ok("dial a regular file is ECONNREFUSED")

	// A path-bound datagram socket is not a stream listener.
	g, err := net.ListenUnixgram("unixgram", &net.UnixAddr{Name: "/tmp/dg-only.sock", Net: "unixgram"})
	must(err, "listen unixgram")
	_, err = net.Dial("unix", "/tmp/dg-only.sock")
	wantErrno(err, syscall.EPROTOTYPE, "dial dgram socket")
	g.Close()
	ok("stream dial of a datagram socket is EPROTOTYPE")
}

func unlinkWhileListening() {
	path := "/tmp/unlink.sock"
	ln, err := net.Listen("unix", path)
	must(err, "listen")
	go echoServer(ln)
	c, err := net.Dial("unix", path)
	must(err, "dial")
	must(os.Remove(path), "unlink")
	_, err = net.Dial("unix", path)
	wantErrno(err, syscall.ENOENT, "dial unlinked")
	echoOnce(c, "still connected")
	c.Close()
	// The name is free again.
	ln2, err := net.Listen("unix", path)
	must(err, "relisten")
	ln2.Close()
	ln.Close()
	ok("unlink while listening: new dials ENOENT, old conn works")
}

func abstractName() {
	ln, err := net.Listen("unix", "@patina-test")
	must(err, "listen abstract")
	if got := ln.Addr().String(); got != "@patina-test" {
		fail("abstract Addr %q", got)
	}
	go echoServer(ln)
	dialEcho("@patina-test", "abstract hello")
	_, err = net.Listen("unix", "@patina-test")
	wantErrno(err, syscall.EADDRINUSE, "abstract in use")
	ln.Close()
	_, err = net.Dial("unix", "@patina-test")
	wantErrno(err, syscall.ECONNREFUSED, "dial closed abstract")
	ln, err = net.Listen("unix", "@patina-test")
	must(err, "relisten abstract")
	ln.Close()
	ok("abstract @patina-test listen/dial, freed on close")
}

func concurrent() {
	ln, err := net.Listen("unix", "/tmp/many.sock")
	must(err, "listen")
	defer ln.Close()
	go echoServer(ln)
	var wg sync.WaitGroup
	errs := make(chan error, 50)
	for i := 0; i < 50; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			c, err := net.Dial("unix", "/tmp/many.sock")
			if err != nil {
				errs <- err
				return
			}
			defer c.Close()
			c.SetDeadline(time.Now().Add(20 * time.Second))
			msg := fmt.Sprintf("conn %d says hi", i)
			for j := 0; j < 10; j++ {
				if _, err := io.WriteString(c, msg); err != nil {
					errs <- err
					return
				}
				buf := make([]byte, len(msg))
				if _, err := io.ReadFull(c, buf); err != nil {
					errs <- err
					return
				}
				if string(buf) != msg {
					errs <- fmt.Errorf("conn %d: got %q", i, buf)
					return
				}
			}
		}(i)
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		fail("concurrent: %v", err)
	}
	ok("50 concurrent connections")
}

func pattern(seed int64, n int) []byte {
	b := make([]byte, n)
	rand.New(rand.NewSource(seed)).Read(b)
	return b
}

func bulk() {
	const size = 8 << 20
	a, b := pattern(1, size), pattern(2, size)
	ln, err := net.Listen("unix", "/tmp/bulk.sock")
	must(err, "listen")
	defer ln.Close()
	srvGot := make(chan [32]byte, 1)
	go func() {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		defer c.Close()
		go func() {
			c.Write(b)
			c.(*net.UnixConn).CloseWrite()
		}()
		h := sha256.New()
		io.Copy(h, c)
		var s [32]byte
		copy(s[:], h.Sum(nil))
		srvGot <- s
	}()
	c, err := net.Dial("unix", "/tmp/bulk.sock")
	must(err, "dial")
	defer c.Close()
	c.SetDeadline(time.Now().Add(60 * time.Second))
	go func() {
		c.Write(a)
		c.(*net.UnixConn).CloseWrite()
	}()
	h := sha256.New()
	n, err := io.Copy(h, c)
	must(err, "read bulk")
	if n != size || !bytes.Equal(h.Sum(nil), sha(b)) {
		fail("bulk client got %d bytes, checksum mismatch", n)
	}
	s := <-srvGot
	if !bytes.Equal(s[:], sha(a)) {
		fail("bulk server checksum mismatch")
	}
	ok("8 MiB each direction, checksums match")
}

func sha(b []byte) []byte {
	s := sha256.Sum256(b)
	return s[:]
}

func halfClose() {
	ln, err := net.Listen("unix", "/tmp/half.sock")
	must(err, "listen")
	defer ln.Close()
	go func() {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		defer c.Close()
		req, _ := io.ReadAll(c) // until the client's CloseWrite
		c.Write(append([]byte("reply to "), req...))
	}()
	c, err := net.Dial("unix", "/tmp/half.sock")
	must(err, "dial")
	defer c.Close()
	c.SetDeadline(time.Now().Add(10 * time.Second))
	io.WriteString(c, "request")
	must(c.(*net.UnixConn).CloseWrite(), "CloseWrite")
	got, err := io.ReadAll(c)
	must(err, "read reply")
	if string(got) != "reply to request" {
		fail("half-close reply %q", got)
	}
	ok("CloseWrite: peer sees EOF, then replies")
}

func peerClose() {
	ln, err := net.Listen("unix", "/tmp/pc.sock")
	must(err, "listen")
	defer ln.Close()
	accepted := make(chan net.Conn, 1)
	go func() {
		c, _ := ln.Accept()
		accepted <- c
	}()
	c, err := net.Dial("unix", "/tmp/pc.sock")
	must(err, "dial")
	defer c.Close()
	s := <-accepted
	s.Write([]byte("bye"))
	s.Close()
	c.SetDeadline(time.Now().Add(5 * time.Second))
	got, err := io.ReadAll(c)
	if err != nil || string(got) != "bye" {
		fail("read after peer close: %q %v", got, err)
	}
	ok("read after peer close: data then EOF")

	var werr error
	for i := 0; i < 10 && werr == nil; i++ {
		_, werr = c.Write([]byte("anyone?"))
	}
	if !errors.Is(werr, syscall.EPIPE) && !errors.Is(werr, syscall.ECONNRESET) {
		fail("write after peer close: %v", werr)
	}
	ok("write after peer close: " + werr.(*net.OpError).Err.Error())
}

func deadlines() {
	ln, err := net.Listen("unix", "/tmp/dl.sock")
	must(err, "listen")
	defer ln.Close()
	go func() {
		c, err := ln.Accept()
		if err == nil {
			time.Sleep(2 * time.Second)
			c.Close()
		}
	}()
	c, err := net.Dial("unix", "/tmp/dl.sock")
	must(err, "dial")
	defer c.Close()
	start := time.Now()
	c.SetReadDeadline(time.Now().Add(100 * time.Millisecond))
	_, err = c.Read(make([]byte, 1))
	if !errors.Is(err, os.ErrDeadlineExceeded) {
		fail("read deadline: %v", err)
	}
	if el := time.Since(start); el < 90*time.Millisecond || el > 1500*time.Millisecond {
		fail("read deadline took %v", el)
	}
	ok("read deadline")
}

func socketpairs() {
	// STREAM.
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
	must(err, "socketpair stream")
	syscall.Write(fds[0], []byte("abc"))
	syscall.Write(fds[0], []byte("def"))
	if n := fionread(fds[1]); n != 6 {
		fail("FIONREAD %d want 6", n)
	}
	buf := make([]byte, 64)
	n, err := syscall.Read(fds[1], buf)
	if err != nil || string(buf[:n]) != "abcdef" {
		fail("stream pair read %q %v", buf[:n], err)
	}
	syscall.Write(fds[1], []byte("back"))
	n, _ = syscall.Read(fds[0], buf)
	if string(buf[:n]) != "back" {
		fail("stream pair reverse %q", buf[:n])
	}
	sa, err := syscall.Getsockname(fds[0])
	must(err, "getsockname")
	if u, ok := sa.(*syscall.SockaddrUnix); !ok || (u.Name != "" && u.Name != "@") {
		fail("socketpair name %#v", sa)
	}
	syscall.Close(fds[0])
	n, err = syscall.Read(fds[1], buf)
	if n != 0 || err != nil {
		fail("stream pair EOF: %d %v", n, err)
	}
	syscall.Close(fds[1])
	ok("socketpair SOCK_STREAM round trip, FIONREAD, EOF")

	// DGRAM: boundaries kept.
	fds, err = syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_DGRAM, 0)
	must(err, "socketpair dgram")
	syscall.Write(fds[0], []byte("a"))
	syscall.Write(fds[0], []byte("bb"))
	syscall.Write(fds[0], []byte{})
	for _, want := range []string{"a", "bb", ""} {
		n, err := syscall.Read(fds[1], buf)
		if err != nil || string(buf[:n]) != want {
			fail("dgram pair read %q want %q (%v)", buf[:n], want, err)
		}
	}
	syscall.Close(fds[0])
	syscall.Close(fds[1])
	ok("socketpair SOCK_DGRAM keeps message boundaries")

	// SEQPACKET: boundaries + EOF.
	fds, err = syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_SEQPACKET, 0)
	must(err, "socketpair seqpacket")
	syscall.Write(fds[0], []byte("one"))
	syscall.Write(fds[0], []byte("three"))
	small := make([]byte, 2)
	n, _, rflags, _, err := syscall.Recvmsg(fds[1], small, nil, 0)
	if err != nil || n != 2 || rflags&syscall.MSG_TRUNC == 0 {
		fail("seqpacket truncation: n=%d flags=%#x %v", n, rflags, err)
	}
	n, _ = syscall.Read(fds[1], buf)
	if string(buf[:n]) != "three" {
		fail("seqpacket second record %q", buf[:n])
	}
	syscall.Close(fds[0])
	n, err = syscall.Read(fds[1], buf)
	if n != 0 || err != nil {
		fail("seqpacket EOF: %d %v", n, err)
	}
	syscall.Close(fds[1])
	ok("socketpair SOCK_SEQPACKET boundaries, MSG_TRUNC, EOF")
}

func fionread(fd int) int {
	var n int32
	_, _, e := syscall.Syscall(syscall.SYS_IOCTL, uintptr(fd), 0x541B, uintptr(unsafe.Pointer(&n)))
	if e != 0 {
		fail("FIONREAD: %v", e)
	}
	return int(n)
}

func cloexec(fd int) bool {
	r, _, e := syscall.Syscall(syscall.SYS_FCNTL, uintptr(fd), syscall.F_GETFD, 0)
	if e != 0 {
		fail("F_GETFD: %v", e)
	}
	return r&syscall.FD_CLOEXEC != 0
}

// recvRights receives one message on fd and returns its data and fds.
func recvRights(fd int, oobSpace int, flags int) (string, []int, int) {
	buf := make([]byte, 64)
	oob := make([]byte, oobSpace)
	n, oobn, rflags, _, err := syscall.Recvmsg(fd, buf, oob, flags)
	must(err, "recvmsg")
	msgs, err := syscall.ParseSocketControlMessage(oob[:oobn])
	must(err, "parse cmsg")
	var fds []int
	for i := range msgs {
		f, err := syscall.ParseUnixRights(&msgs[i])
		must(err, "parse rights")
		fds = append(fds, f...)
	}
	return string(buf[:n]), fds, rflags
}

// throughPipe writes via fd and checks it arrives on r.
func throughPipe(fd int, r *os.File, msg string) {
	_, err := syscall.Write(fd, []byte(msg))
	must(err, "write via passed fd")
	buf := make([]byte, 64)
	r.SetReadDeadline(time.Now().Add(5 * time.Second))
	n, err := r.Read(buf)
	if err != nil || string(buf[:n]) != msg {
		fail("pipe got %q (%v) want %q", buf[:n], err, msg)
	}
}

func scmRights() {
	pr, pw, err := os.Pipe()
	must(err, "pipe")
	defer pr.Close()
	defer pw.Close()
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	must(err, "socketpair")
	defer syscall.Close(fds[0])
	defer syscall.Close(fds[1])

	must(syscall.Sendmsg(fds[0], []byte("x"), syscall.UnixRights(int(pw.Fd())), nil, 0), "sendmsg rights")
	data, got, _ := recvRights(fds[1], syscall.CmsgSpace(4), 0)
	if data != "x" || len(got) != 1 || got[0] == int(pw.Fd()) {
		fail("rights over socketpair: %q %v", data, got)
	}
	if cloexec(got[0]) {
		fail("received fd has FD_CLOEXEC without MSG_CMSG_CLOEXEC")
	}
	throughPipe(got[0], pr, "via socketpair")
	syscall.Close(got[0])
	ok("SCM_RIGHTS over socketpair: pipe write end works")

	must(syscall.Sendmsg(fds[0], []byte("y"), syscall.UnixRights(int(pw.Fd())), nil, 0), "sendmsg rights 2")
	_, got, _ = recvRights(fds[1], syscall.CmsgSpace(4), syscall.MSG_CMSG_CLOEXEC)
	if len(got) != 1 || !cloexec(got[0]) {
		fail("MSG_CMSG_CLOEXEC: %v", got)
	}
	syscall.Close(got[0])
	ok("MSG_CMSG_CLOEXEC sets FD_CLOEXEC")

	// Three fds, room for two (CMSG_SPACE(4) = 24 = CMSG_LEN(8), as
	// Linux computes it): MSG_CTRUNC, the third is closed.
	p := int(pw.Fd())
	must(syscall.Sendmsg(fds[0], []byte("z"), syscall.UnixRights(p, p, p), nil, 0), "sendmsg 3 rights")
	_, got, rflags := recvRights(fds[1], syscall.CmsgSpace(4), 0)
	if len(got) != 2 || rflags&syscall.MSG_CTRUNC == 0 {
		fail("MSG_CTRUNC: fds %v flags %#x", got, rflags)
	}
	syscall.Close(got[0])
	syscall.Close(got[1])
	ok("SCM_RIGHTS truncated: MSG_CTRUNC, two of three fds delivered")

	// Fds arrive with the first byte of the send that carried them.
	syscall.Write(fds[0], []byte("plain"))
	must(syscall.Sendmsg(fds[0], []byte("rights"), syscall.UnixRights(p), nil, 0), "sendmsg after data")
	syscall.Write(fds[0], []byte("tail"))
	time.Sleep(10 * time.Millisecond)
	data, got, _ = recvRights(fds[1], syscall.CmsgSpace(4), 0)
	if data != "plainrights" || len(got) != 1 {
		fail("rights boundary: %q %v", data, got)
	}
	syscall.Close(got[0])
	data, got, _ = recvRights(fds[1], syscall.CmsgSpace(4), 0)
	if data != "tail" || len(got) != 0 {
		fail("after rights: %q %v", data, got)
	}
	ok("SCM_RIGHTS stream read stops after the carrying send")

	err = syscall.Sendmsg(fds[0], []byte("bad"), syscall.UnixRights(9999), nil, 0)
	wantErrno(err, syscall.EBADF, "rights with bad fd")
	ok("SCM_RIGHTS with a bad fd is EBADF")

	// Over a net.UnixConn.
	ln, err := net.Listen("unix", "/tmp/rights.sock")
	must(err, "listen")
	defer ln.Close()
	srv := make(chan *net.UnixConn, 1)
	go func() {
		c, err := ln.Accept()
		if err != nil {
			srv <- nil
			return
		}
		srv <- c.(*net.UnixConn)
	}()
	cc, err := net.Dial("unix", "/tmp/rights.sock")
	must(err, "dial")
	defer cc.Close()
	sc := <-srv
	if sc == nil {
		fail("accept failed")
	}
	defer sc.Close()
	_, _, err = cc.(*net.UnixConn).WriteMsgUnix([]byte("fd!"), syscall.UnixRights(p), nil)
	must(err, "WriteMsgUnix")
	sc.SetDeadline(time.Now().Add(5 * time.Second))
	buf := make([]byte, 16)
	oob := make([]byte, syscall.CmsgSpace(4))
	n, oobn, _, _, err := sc.ReadMsgUnix(buf, oob)
	must(err, "ReadMsgUnix")
	msgs, err := syscall.ParseSocketControlMessage(oob[:oobn])
	must(err, "parse")
	if len(msgs) != 1 || string(buf[:n]) != "fd!" {
		fail("ReadMsgUnix: %q %d msgs", buf[:n], len(msgs))
	}
	rf, err := syscall.ParseUnixRights(&msgs[0])
	must(err, "ParseUnixRights")
	throughPipe(rf[0], pr, "via UnixConn")
	syscall.Close(rf[0])
	ok("SCM_RIGHTS over net.UnixConn (WriteMsgUnix/ReadMsgUnix)")

	// SO_PEERCRED on an accepted connection and a socketpair.
	var cred *syscall.Ucred
	rc, _ := sc.SyscallConn()
	rc.Control(func(fd uintptr) {
		cred, err = syscall.GetsockoptUcred(int(fd), syscall.SOL_SOCKET, syscall.SO_PEERCRED)
	})
	must(err, "SO_PEERCRED")
	want := syscall.Ucred{Pid: 1, Uid: 0, Gid: 0}
	if *cred != want {
		fail("SO_PEERCRED accepted %+v", *cred)
	}
	cred, err = syscall.GetsockoptUcred(fds[0], syscall.SOL_SOCKET, syscall.SO_PEERCRED)
	must(err, "SO_PEERCRED pair")
	if *cred != want {
		fail("SO_PEERCRED pair %+v", *cred)
	}
	ok("SO_PEERCRED is {pid 1, uid 0, gid 0}")
}

func credentials() {
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_DGRAM, 0)
	must(err, "socketpair")
	defer syscall.Close(fds[0])
	defer syscall.Close(fds[1])
	must(syscall.SetsockoptInt(fds[1], syscall.SOL_SOCKET, syscall.SO_PASSCRED, 1), "SO_PASSCRED")
	v, err := syscall.GetsockoptInt(fds[1], syscall.SOL_SOCKET, syscall.SO_PASSCRED)
	if err != nil || v != 1 {
		fail("get SO_PASSCRED %d %v", v, err)
	}
	recvCred := func() *syscall.Ucred {
		buf := make([]byte, 16)
		oob := make([]byte, syscall.CmsgSpace(syscall.SizeofUcred))
		_, oobn, _, _, err := syscall.Recvmsg(fds[1], buf, oob, 0)
		must(err, "recvmsg")
		msgs, err := syscall.ParseSocketControlMessage(oob[:oobn])
		must(err, "parse")
		if len(msgs) != 1 {
			fail("creds: %d cmsgs", len(msgs))
		}
		c, err := syscall.ParseUnixCredentials(&msgs[0])
		must(err, "ParseUnixCredentials")
		return c
	}
	syscall.Write(fds[0], []byte("who"))
	if c := recvCred(); *c != (syscall.Ucred{Pid: 1}) {
		fail("SCM_CREDENTIALS %+v", *c)
	}
	must(syscall.Sendmsg(fds[0], []byte("me"), syscall.UnixCredentials(&syscall.Ucred{Pid: 1, Uid: 0, Gid: 0}), nil, 0), "send creds")
	if c := recvCred(); *c != (syscall.Ucred{Pid: 1}) {
		fail("explicit SCM_CREDENTIALS %+v", *c)
	}
	ok("SO_PASSCRED + SCM_CREDENTIALS {1,0,0}")
}

func unixgram() {
	srvPath, cliPath := "/tmp/g-srv.sock", "/tmp/g-cli.sock"
	srv, err := net.ListenUnixgram("unixgram", &net.UnixAddr{Name: srvPath, Net: "unixgram"})
	must(err, "ListenUnixgram")
	defer srv.Close()
	cli, err := net.DialUnix("unixgram", &net.UnixAddr{Name: cliPath, Net: "unixgram"},
		&net.UnixAddr{Name: srvPath, Net: "unixgram"})
	must(err, "DialUnix")
	defer cli.Close()
	srv.SetDeadline(time.Now().Add(5 * time.Second))
	cli.SetDeadline(time.Now().Add(5 * time.Second))
	_, err = cli.Write([]byte("ping"))
	must(err, "write")
	buf := make([]byte, 70000)
	n, from, err := srv.ReadFromUnix(buf)
	must(err, "ReadFromUnix")
	if string(buf[:n]) != "ping" || from == nil || from.Name != cliPath {
		fail("unixgram got %q from %v", buf[:n], from)
	}
	_, err = srv.WriteToUnix([]byte("pong"), from)
	must(err, "WriteToUnix")
	n, err = cli.Read(buf)
	if err != nil || string(buf[:n]) != "pong" {
		fail("unixgram reply %q %v", buf[:n], err)
	}
	ok("unixgram ReadFrom/WriteTo with sender address")

	big := pattern(3, 64<<10)
	_, err = cli.Write(big)
	must(err, "write 64 KiB datagram")
	n, _, err = srv.ReadFromUnix(buf)
	if err != nil || n != len(big) || !bytes.Equal(buf[:n], big) {
		fail("64 KiB datagram: n=%d %v", n, err)
	}
	ok("64 KiB datagram")

	// An unnamed sender reports an empty address.
	anon, err := net.DialUnix("unixgram", nil, &net.UnixAddr{Name: srvPath, Net: "unixgram"})
	must(err, "dial unnamed")
	anon.Write([]byte("anon"))
	n, from, err = srv.ReadFromUnix(buf)
	if err != nil || string(buf[:n]) != "anon" || (from != nil && from.Name != "" && from.Name != "@") {
		fail("unnamed sender: %q %v %v", buf[:n], from, err)
	}
	anon.Close()
	ok("unnamed datagram sender has an empty address")

	// MSG_TRUNC / MSG_PEEK with raw syscalls.
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_DGRAM, 0)
	must(err, "socketpair")
	syscall.Write(fds[0], make([]byte, 100))
	small := make([]byte, 10)
	n, _, err = syscall.Recvfrom(fds[1], small, syscall.MSG_PEEK)
	if err != nil || n != 10 {
		fail("MSG_PEEK: %d %v", n, err)
	}
	n, _, err = syscall.Recvfrom(fds[1], small, syscall.MSG_TRUNC)
	if err != nil || n != 100 {
		fail("MSG_TRUNC returns %d (%v), want 100", n, err)
	}
	syscall.Close(fds[0])
	syscall.Close(fds[1])
	ok("MSG_PEEK and MSG_TRUNC")

	// A datagram to a closed socket is ECONNREFUSED.
	tmp, err := net.ListenUnixgram("unixgram", &net.UnixAddr{Name: "/tmp/g-gone.sock", Net: "unixgram"})
	must(err, "listen gone")
	tmp.Close()
	s, err := syscall.Socket(syscall.AF_UNIX, syscall.SOCK_DGRAM, 0)
	must(err, "socket")
	err = syscall.Sendto(s, []byte("x"), 0, &syscall.SockaddrUnix{Name: "/tmp/g-gone.sock"})
	wantErrno(err, syscall.ECONNREFUSED, "sendto closed")
	// cli is connected to the server: nobody else may send to cli.
	err = syscall.Sendto(s, []byte("x"), 0, &syscall.SockaddrUnix{Name: cliPath})
	wantErrno(err, syscall.EPERM, "send to a socket connected elsewhere")
	err = syscall.Connect(s, &syscall.SockaddrUnix{Name: cliPath})
	wantErrno(err, syscall.EPERM, "connect to a socket connected elsewhere")
	// The server is not connected: anyone may send to it.
	must(syscall.Sendto(s, []byte("x"), 0, &syscall.SockaddrUnix{Name: srvPath}), "sendto server")
	srv.ReadFromUnix(buf)
	syscall.Close(s)
	ok("unixgram ECONNREFUSED to a closed socket, EPERM to a foreign-connected one")

	// A full receive queue: EAGAIN when non-blocking.
	fds, err = syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_DGRAM|syscall.SOCK_NONBLOCK, 0)
	must(err, "socketpair")
	sent := 0
	for ; sent < 1000; sent++ {
		if _, err = syscall.Write(fds[0], []byte("q")); err != nil {
			break
		}
	}
	wantErrno(err, syscall.EAGAIN, "full dgram queue")
	if sent < 10 {
		fail("dgram queue held only %d", sent)
	}
	syscall.Close(fds[0])
	syscall.Close(fds[1])
	ok(fmt.Sprintf("datagram queue bounded (%d messages, then EAGAIN)", sent))
}

func backlog() {
	s, err := syscall.Socket(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	must(err, "socket")
	err = syscall.Listen(s, 1)
	wantErrno(err, syscall.EINVAL, "listen unbound")
	must(syscall.Bind(s, &syscall.SockaddrUnix{Name: "@backlog"}), "bind")
	must(syscall.Listen(s, 0), "listen 0")
	// backlog 0 admits one pending connection (Linux: queue > backlog).
	var clients []int
	dial := func(flags int) error {
		c, err := syscall.Socket(syscall.AF_UNIX, syscall.SOCK_STREAM|flags, 0)
		must(err, "socket")
		clients = append(clients, c)
		return syscall.Connect(c, &syscall.SockaddrUnix{Name: "@backlog"})
	}
	must(dial(0), "first connect")
	err = dial(syscall.SOCK_NONBLOCK)
	wantErrno(err, syscall.EAGAIN, "connect to full backlog")
	done := make(chan error, 1)
	go func() { done <- dial(0) }()
	select {
	case err := <-done:
		fail("blocking connect to a full backlog returned %v", err)
	case <-time.After(100 * time.Millisecond):
	}
	a, _, err := syscall.Accept(s)
	must(err, "accept")
	select {
	case err := <-done:
		must(err, "blocked connect")
	case <-time.After(5 * time.Second):
		fail("blocked connect not released by accept")
	}
	syscall.Close(a)
	for _, c := range clients {
		syscall.Close(c)
	}
	syscall.Close(s)
	ok("backlog: EAGAIN when full, blocking connect waits for accept")

	// Autobind: bind with only sun_family.
	s, err = syscall.Socket(syscall.AF_UNIX, syscall.SOCK_DGRAM, 0)
	must(err, "socket")
	sa := [2]uint16{syscall.AF_UNIX, 0}
	_, _, e := syscall.Syscall(syscall.SYS_BIND, uintptr(s), uintptr(unsafe.Pointer(&sa)), 2)
	if e != 0 {
		fail("autobind: %v", e)
	}
	name, err := syscall.Getsockname(s)
	must(err, "getsockname")
	if u := name.(*syscall.SockaddrUnix); len(u.Name) != 6 || u.Name[0] != '@' {
		fail("autobind name %q", u.Name)
	}
	syscall.Close(s)
	ok("autobind gives an abstract name")
}

func httpOverUnix() {
	path := "/tmp/http.sock"
	ln, err := net.Listen("unix", path)
	must(err, "listen")
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, "hello %s over unix", r.URL.Path)
	})}
	go srv.Serve(ln)
	defer srv.Close()
	client := &http.Client{
		Timeout: 10 * time.Second,
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				var d net.Dialer
				return d.DialContext(ctx, "unix", path)
			},
		},
	}
	for i := 0; i < 3; i++ {
		resp, err := client.Get("http://unix/x")
		must(err, "GET")
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if string(body) != "hello /x over unix" {
			fail("http body %q", body)
		}
	}
	ok("http.Server over a unix listener")
}

func main() {
	streamPath()
	dialErrors()
	unlinkWhileListening()
	abstractName()
	concurrent()
	bulk()
	halfClose()
	peerClose()
	deadlines()
	socketpairs()
	scmRights()
	credentials()
	unixgram()
	backlog()
	httpOverUnix()
	fmt.Println("PASS unixsock")
}
