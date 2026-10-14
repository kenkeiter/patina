package ctl

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

// shortDir returns a temp dir short enough for a Unix socket path (macOS
// limits sun_path to 104 bytes and t.TempDir() is often longer).
func shortDir(t *testing.T) string {
	t.Helper()
	d, err := os.MkdirTemp("/tmp", "pctl")
	if err != nil {
		d, err = os.MkdirTemp("", "pctl")
		if err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() { os.RemoveAll(d) })
	return d
}

// fakeGuest emulates the kernel side of the control channel.
type fakeGuest struct {
	t         *testing.T
	ln        net.Listener
	vsock     bool // require the Firecracker CONNECT handshake
	mu        sync.Mutex
	ops       []map[string]any
	handshake string
	conns     int
}

func newFakeGuest(t *testing.T, vsock bool) (*fakeGuest, string) {
	p := filepath.Join(shortDir(t), "g.sock")
	ln, err := net.Listen("unix", p)
	if err != nil {
		t.Fatal(err)
	}
	g := &fakeGuest{t: t, ln: ln, vsock: vsock}
	t.Cleanup(func() { ln.Close() })
	go g.serve()
	return g, p
}

func (g *fakeGuest) serve() {
	for {
		c, err := g.ln.Accept()
		if err != nil {
			return
		}
		g.mu.Lock()
		g.conns++
		g.mu.Unlock()
		go g.handle(c)
	}
}

func (g *fakeGuest) handle(c net.Conn) {
	defer c.Close()
	r := bufio.NewReader(c)
	if g.vsock {
		l, err := r.ReadString('\n')
		if err != nil {
			return
		}
		g.mu.Lock()
		g.handshake = l
		g.mu.Unlock()
		if l != "CONNECT 4242\n" {
			return // Firecracker just closes on a bad request
		}
		fmt.Fprintf(c, "OK 1073741824\n")
	}
	fmt.Fprintf(c, `{"ev":"ready","go":"go1.27.1","cpus":2,"mem_mb":512}`+"\n")
	for {
		l, err := r.ReadString('\n')
		if err != nil {
			return
		}
		var op map[string]any
		if err := json.Unmarshal([]byte(l), &op); err != nil {
			fmt.Fprintf(c, `{"ev":"error","msg":"bad json"}`+"\n")
			continue
		}
		g.mu.Lock()
		g.ops = append(g.ops, op)
		g.mu.Unlock()
		switch op["op"] {
		case "ping":
			// An unrelated event first, then garbage, then the reply.
			fmt.Fprintf(c, `{"ev":"log","level":"warn","msg":"ENOSYS 334"}`+"\n")
			fmt.Fprintf(c, "not json\n\n")
			fmt.Fprintf(c, `{"ev":"pong","id":%v,"uptime_ms":1234,"extra":{"nested":[1,2]}}`+"\n", op["id"])
		case "stop":
			if op["grace_ms"].(float64) == 0 {
				fmt.Fprintf(c, `{"ev":"exit","code":137,"forced":true}`+"\n")
			} else {
				fmt.Fprintf(c, `{"ev":"exit","code":143,"signal":"SIGTERM"}`+"\n")
			}
			return
		case "signal", "dump", "time":
		default:
			fmt.Fprintf(c, `{"ev":"error","msg":"unknown op"}`+"\n")
		}
	}
}

func (g *fakeGuest) lastOp() map[string]any {
	g.mu.Lock()
	defer g.mu.Unlock()
	if len(g.ops) == 0 {
		return nil
	}
	return g.ops[len(g.ops)-1]
}

func TestOpEncoding(t *testing.T) {
	cases := map[string]Op{
		`{"op":"ping","id":1}`:                   Ping(1),
		`{"op":"stop","grace_ms":30000}`:         Stop(30 * time.Second),
		`{"op":"stop","grace_ms":0}`:             Stop(0),
		`{"op":"signal","sig":"SIGHUP"}`:         Signal("SIGHUP"),
		`{"op":"dump"}`:                          Dump(),
		`{"op":"time","epoch_ns":1500000000000}`: Time(time.Unix(1500, 0)),
	}
	for want, op := range cases {
		if got := string(op.Encode()); got != want+"\n" {
			t.Errorf("got %q want %q", got, want)
		}
	}
}

func TestNormalizeSignal(t *testing.T) {
	for in, want := range map[string]string{"hup": "SIGHUP", "SIGTERM": "SIGTERM", "sigquit": "SIGQUIT", "USR1": "SIGUSR1"} {
		if got, err := NormalizeSignal(in); err != nil || got != want {
			t.Errorf("%s: %q %v", in, got, err)
		}
	}
	for _, bad := range []string{"SIGKILL", "SIGSEGV", "9", ""} {
		if _, err := NormalizeSignal(bad); err == nil {
			t.Errorf("%q accepted", bad)
		}
	}
}

func TestParseEvent(t *testing.T) {
	e, err := ParseEvent(`{"ev":"exit","code":0,"forced":false,"future":"x"}` + "\r\n")
	if err != nil || e.Ev != "exit" || e.Code == nil || *e.Code != 0 {
		t.Fatalf("%+v %v", e, err)
	}
	if _, err := ParseEvent("[patina] exit code 3"); err == nil {
		t.Fatal("non-JSON accepted")
	}
}

func testClientAgainst(t *testing.T, vsock bool) {
	g, p := newFakeGuest(t, vsock)
	ep := Endpoint{Kind: KindStream, Path: p}
	if vsock {
		ep.Kind = KindFirecracker
	}
	c, err := Dial(ep, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()

	var out bytes.Buffer
	pong, err := DoPing(c, 7, 2*time.Second, &out)
	if err != nil {
		t.Fatal(err)
	}
	if pong.UptimeMs != 1234 || *pong.ID != 7 {
		t.Fatalf("pong %+v", pong)
	}
	if !strings.Contains(out.String(), `"ev":"ready"`) || !strings.Contains(out.String(), "ENOSYS 334") {
		t.Fatalf("events not printed: %q", out.String())
	}

	if err := DoOneWay(c, Signal("SIGHUP"), 100*time.Millisecond, nil); err != nil {
		t.Fatal(err)
	}
	if op := g.lastOp(); op["op"] != "signal" || op["sig"] != "SIGHUP" {
		t.Fatalf("guest saw %v", op)
	}
	if err := DoOneWay(c, Op{Op: "bogus"}, time.Second, nil); err == nil || !strings.Contains(err.Error(), "unknown op") {
		t.Fatalf("guest error not surfaced: %v", err)
	}

	code, err := DoStop(c, 30*time.Second, 2*time.Second, &out)
	if err != nil {
		t.Fatal(err)
	}
	if code != 143 {
		t.Fatalf("code %d", code)
	}
	if op := g.lastOp(); op["grace_ms"].(float64) != 30000 {
		t.Fatalf("grace %v", op)
	}
	if vsock && g.handshake != "CONNECT 4242\n" {
		t.Fatalf("handshake %q", g.handshake)
	}
}

func TestClientStream(t *testing.T)      { testClientAgainst(t, false) }
func TestClientFirecracker(t *testing.T) { testClientAgainst(t, true) }

func TestFirecrackerHandshakeRejected(t *testing.T) {
	p := filepath.Join(shortDir(t), "fc.sock")
	ln, err := net.Listen("unix", p)
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	go func() {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		bufio.NewReader(c).ReadString('\n')
		c.Close() // what Firecracker does when nothing listens on the port
	}()
	_, err = Dial(Endpoint{Kind: KindFirecracker, Path: p}, time.Second)
	if err == nil || !strings.Contains(err.Error(), "CONNECT 4242") {
		t.Fatalf("got %v", err)
	}
}

func TestGuestNeverAnswers(t *testing.T) {
	p := filepath.Join(shortDir(t), "mute.sock")
	ln, err := net.Listen("unix", p)
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	go func() {
		c, err := ln.Accept()
		if err == nil {
			defer c.Close()
			time.Sleep(2 * time.Second)
		}
	}()
	c, err := Dial(Endpoint{Kind: KindStream, Path: p}, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	start := time.Now()
	if _, err := DoPing(c, 1, 200*time.Millisecond, nil); err != ErrTimeout {
		t.Fatalf("got %v", err)
	}
	if time.Since(start) > time.Second {
		t.Fatal("timeout not honored")
	}
}

func TestHubProxiesAndBroadcasts(t *testing.T) {
	_, gp := newFakeGuest(t, true)
	var mu sync.Mutex
	var seen []string
	hp := filepath.Join(shortDir(t), "hub.sock")
	h, err := NewHub(hp, Endpoint{Kind: KindFirecracker, Path: gp}, func(e *Event) {
		mu.Lock()
		seen = append(seen, e.Ev)
		mu.Unlock()
	})
	if err != nil {
		t.Fatal(err)
	}
	defer h.Close()
	select {
	case <-h.Connected():
	case <-time.After(3 * time.Second):
		t.Fatal("hub never connected upstream")
	}
	// Wait for the ready event to be cached so the late client sees it.
	deadline := time.Now().Add(2 * time.Second)
	for {
		mu.Lock()
		n := len(seen)
		mu.Unlock()
		if n > 0 || time.Now().After(deadline) {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}

	a, err := Dial(Endpoint{Kind: KindStream, Path: hp}, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer a.Close()
	b, err := Dial(Endpoint{Kind: KindStream, Path: hp}, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if e, err := a.WaitFor(time.Now().Add(time.Second), nil, func(e *Event) bool { return true }); err != nil || e.Ev != "ready" {
		t.Fatalf("late joiner did not get ready: %v %v", e, err)
	}
	if _, err := DoPing(a, 42, 2*time.Second, nil); err != nil {
		t.Fatal(err)
	}
	// b sees the broadcast pong for a's ping too.
	if e, err := b.WaitFor(time.Now().Add(time.Second), nil, func(e *Event) bool { return e.Ev == "pong" }); err != nil || *e.ID != 42 {
		t.Fatalf("broadcast: %v %v", e, err)
	}
	// Stop from the hub itself (what run does on Ctrl-C).
	if err := h.Send(Stop(10 * time.Second)); err != nil {
		t.Fatal(err)
	}
	e, err := b.WaitFor(time.Now().Add(2*time.Second), nil, func(e *Event) bool { return e.Ev == "exit" })
	if err != nil || *e.Code != 143 {
		t.Fatalf("exit via hub: %v %v", e, err)
	}
	mu.Lock()
	defer mu.Unlock()
	if !strings.Contains(strings.Join(seen, ","), "exit") {
		t.Fatalf("onEvent saw %v", seen)
	}
}

func TestHubNotConnected(t *testing.T) {
	d := shortDir(t)
	h, err := NewHub(filepath.Join(d, "hub.sock"), Endpoint{Kind: KindStream, Path: filepath.Join(d, "nothing.sock")}, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer h.Close()
	if err := h.Send(Dump()); err != ErrNotConnected {
		t.Fatalf("got %v", err)
	}
	c, err := Dial(Endpoint{Kind: KindStream, Path: filepath.Join(d, "hub.sock")}, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	if _, err := DoPing(c, 1, time.Second, nil); err == nil || !strings.Contains(err.Error(), "not connected") {
		t.Fatalf("got %v", err)
	}
}
