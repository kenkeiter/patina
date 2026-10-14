package ctl

import (
	"net"
	"path/filepath"
	"strings"
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
