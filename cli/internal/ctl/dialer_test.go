package ctl

import (
	"net"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

// An in-process VMM (Apple VZ) supplies the upstream stream through
// Endpoint.Dialer; the hub must use it, retry while it fails, and proxy.
func TestHubWithDialer(t *testing.T) {
	guest := make(chan net.Conn, 1)
	fails := 2
	ep := Endpoint{Kind: KindVZ, Port: Port, Dialer: func(time.Duration) (net.Conn, error) {
		if fails > 0 {
			fails--
			return nil, net.ErrClosed
		}
		a, b := net.Pipe()
		guest <- b
		return a, nil
	}}
	d := t.TempDir()
	h, err := NewHub(filepath.Join(d, "hub.sock"), ep, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer h.Close()
	g := <-guest
	<-h.Connected()
	c, err := Dial(Endpoint{Kind: KindStream, Path: filepath.Join(d, "hub.sock")}, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	time.Sleep(50 * time.Millisecond) // let the hub register the client
	go g.Write([]byte(`{"ev":"ready","go":"go1.27","cpus":2}` + "\n"))
	e, err := c.WaitFor(time.Now().Add(2*time.Second), nil, func(e *Event) bool { return e.Ev == "ready" })
	if err != nil || e.CPUs != 2 {
		t.Fatalf("ready: %+v %v", e, err)
	}
	type rd struct {
		b   []byte
		err error
	}
	got := make(chan rd, 1)
	go func() {
		buf := make([]byte, 128)
		g.SetReadDeadline(time.Now().Add(2 * time.Second))
		n, err := g.Read(buf)
		got <- rd{buf[:n], err}
	}()
	if err := h.Send(Time(time.Unix(1, 5))); err != nil {
		t.Fatal(err)
	}
	r := <-got
	buf, n, err := r.b, len(r.b), r.err
	if err != nil || string(buf[:n]) != `{"op":"time","epoch_ns":1000000005}`+"\n" {
		t.Fatalf("guest got %q %v", buf[:n], err)
	}
}

func TestDialVZWithoutDialer(t *testing.T) {
	_, err := Dial(Endpoint{Kind: KindVZ, Port: Port}, time.Second)
	if err == nil || !strings.Contains(err.Error(), "proxy") {
		t.Fatalf("got %v", err)
	}
	if s := (Endpoint{Kind: KindVZ, Port: Port}).String(); s != "vz vsock port 4242" {
		t.Fatalf("String() = %q", s)
	}
}

// Before the first connection the hub retries fast: a guest that exits
// within milliseconds of listening must still be reached.
func TestHubFirstDialRetryIsFast(t *testing.T) {
	var mu sync.Mutex
	fails := 20
	ep := Endpoint{Kind: KindVZ, Port: Port, Dialer: func(time.Duration) (net.Conn, error) {
		mu.Lock()
		defer mu.Unlock()
		if fails > 0 {
			fails--
			return nil, net.ErrClosed
		}
		a, _ := net.Pipe()
		return a, nil
	}}
	t0 := time.Now()
	h, err := NewHub(filepath.Join(shortDir(t), "hub.sock"), ep, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer h.Close()
	select {
	case <-h.Connected():
	case <-time.After(5 * time.Second):
		t.Fatal("never connected")
	}
	// 20 failures at 100 ms would take 2 s; at 5 ms, ~100 ms.
	if d := time.Since(t0); d > time.Second {
		t.Fatalf("connected after %v", d)
	}
}

// Quiesce returns at once when the upstream never attached.
func TestHubQuiesceNeverConnected(t *testing.T) {
	ep := Endpoint{Kind: KindVZ, Port: Port, Dialer: func(time.Duration) (net.Conn, error) {
		return nil, net.ErrClosed
	}}
	h, err := NewHub(filepath.Join(shortDir(t), "hub.sock"), ep, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer h.Close()
	time.Sleep(20 * time.Millisecond)
	t0 := time.Now()
	if !h.Quiesce(time.Second) {
		t.Fatal("Quiesce timed out")
	}
	if d := time.Since(t0); d > 100*time.Millisecond {
		t.Fatalf("Quiesce took %v", d)
	}
}

// Quiesce waits until events already sent on the connection (the exit
// event before poweroff) have reached OnEvent.
func TestHubQuiesceDeliversBufferedEvents(t *testing.T) {
	var mu sync.Mutex
	var seen []string
	used := false
	guest := make(chan net.Conn, 1)
	ep := Endpoint{Kind: KindVZ, Port: Port, Dialer: func(time.Duration) (net.Conn, error) {
		mu.Lock()
		defer mu.Unlock()
		if used {
			return nil, net.ErrClosed // the VM is gone
		}
		used = true
		a, b := net.Pipe()
		guest <- b
		return a, nil
	}}
	h, err := NewHub(filepath.Join(shortDir(t), "hub.sock"), ep, func(e *Event) {
		time.Sleep(20 * time.Millisecond) // a slow consumer
		mu.Lock()
		seen = append(seen, e.Ev)
		mu.Unlock()
	})
	if err != nil {
		t.Fatal(err)
	}
	defer h.Close()
	g := <-guest
	go func() {
		g.Write([]byte(`{"ev":"ready","go":"go1.27","cpus":1}` + "\n" + `{"ev":"exit","code":3}` + "\n"))
		g.Close()
	}()
	<-h.Connected()
	if !h.Quiesce(2 * time.Second) {
		t.Fatal("Quiesce timed out")
	}
	mu.Lock()
	defer mu.Unlock()
	if strings.Join(seen, ",") != "ready,exit" {
		t.Fatalf("seen %v", seen)
	}
}
