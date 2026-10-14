package ctl

import (
	"encoding/json"
	"errors"
	"net"
	"os"
	"sync"
	"time"
)

// Hub is the control-channel proxy run by `patina run`. It owns the single
// upstream connection to the guest (which VMM sockets only allow one of) and
// exposes a plain NDJSON Unix socket that any number of `patina ctl`
// processes can connect to. Guest events are broadcast to every client and
// to OnEvent; client lines are forwarded upstream verbatim.
type Hub struct {
	upstream Endpoint
	ln       net.Listener
	onEvent  func(*Event)

	mu        sync.Mutex
	up        *Client
	clients   map[net.Conn]struct{}
	lastReady string
	closed    bool
	done      chan struct{}
	connected chan struct{} // closed on first successful upstream connection
	connOnce  sync.Once
}

// NewHub listens on listenPath and starts connecting to upstream (retrying
// until Close). onEvent is called for every guest event, from one goroutine.
func NewHub(listenPath string, upstream Endpoint, onEvent func(*Event)) (*Hub, error) {
	os.Remove(listenPath)
	ln, err := net.Listen("unix", listenPath)
	if err != nil {
		return nil, err
	}
	h := &Hub{
		upstream:  upstream,
		ln:        ln,
		onEvent:   onEvent,
		clients:   map[net.Conn]struct{}{},
		done:      make(chan struct{}),
		connected: make(chan struct{}),
	}
	go h.acceptLoop()
	go h.upstreamLoop()
	return h, nil
}

// Connected is closed once the upstream channel has been reached.
func (h *Hub) Connected() <-chan struct{} { return h.connected }

// ErrNotConnected is returned by Send before the upstream is reachable.
var ErrNotConnected = errors.New("control channel not connected")

// Send forwards an op upstream.
func (h *Hub) Send(o Op) error { return h.forward(o.Encode()) }

func (h *Hub) forward(line []byte) error {
	h.mu.Lock()
	up := h.up
	h.mu.Unlock()
	if up == nil {
		return ErrNotConnected
	}
	return up.WriteLine(line)
}

func (h *Hub) upstreamLoop() {
	for {
		select {
		case <-h.done:
			return
		default:
		}
		c, err := Dial(h.upstream, 2*time.Second)
		if err != nil {
			select {
			case <-h.done:
				return
			case <-time.After(100 * time.Millisecond):
			}
			continue
		}
		h.mu.Lock()
		if h.closed {
			h.mu.Unlock()
			c.Close()
			return
		}
		h.up = c
		h.mu.Unlock()
		h.connOnce.Do(func() { close(h.connected) })
		for {
			line, err := c.ReadLine()
			if err != nil {
				break
			}
			ev, perr := ParseEvent(line)
			if perr == nil {
				if ev.Ev == "ready" {
					h.mu.Lock()
					h.lastReady = line
					h.mu.Unlock()
				}
				if h.onEvent != nil {
					h.onEvent(ev)
				}
			}
			h.broadcast([]byte(line + "\n"))
		}
		h.mu.Lock()
		if h.up == c {
			h.up = nil
		}
		h.mu.Unlock()
		c.Close()
	}
}

func (h *Hub) broadcast(b []byte) {
	h.mu.Lock()
	conns := make([]net.Conn, 0, len(h.clients))
	for c := range h.clients {
		conns = append(conns, c)
	}
	h.mu.Unlock()
	for _, c := range conns {
		c.SetWriteDeadline(time.Now().Add(2 * time.Second))
		if _, err := c.Write(b); err != nil {
			h.drop(c)
		}
	}
}

func (h *Hub) drop(c net.Conn) {
	h.mu.Lock()
	delete(h.clients, c)
	h.mu.Unlock()
	c.Close()
}

func (h *Hub) acceptLoop() {
	for {
		c, err := h.ln.Accept()
		if err != nil {
			return
		}
		h.mu.Lock()
		if h.closed {
			h.mu.Unlock()
			c.Close()
			return
		}
		h.clients[c] = struct{}{}
		ready := h.lastReady
		h.mu.Unlock()
		if ready != "" {
			// Late joiners still learn the guest is up.
			c.Write([]byte(ready + "\n"))
		}
		go h.clientLoop(c)
	}
}

func (h *Hub) clientLoop(conn net.Conn) {
	defer h.drop(conn)
	cl := NewClient(conn)
	for {
		line, err := cl.ReadLine()
		if err != nil {
			return
		}
		if err := h.forward([]byte(line + "\n")); err != nil {
			msg, _ := json.Marshal(map[string]string{"ev": "error", "msg": err.Error()})
			conn.SetWriteDeadline(time.Now().Add(2 * time.Second))
			conn.Write(append(msg, '\n'))
		}
	}
}

// Close stops the hub, disconnecting all clients and the upstream.
func (h *Hub) Close() error {
	h.mu.Lock()
	if h.closed {
		h.mu.Unlock()
		return nil
	}
	h.closed = true
	close(h.done)
	up := h.up
	h.up = nil
	conns := h.clients
	h.clients = map[net.Conn]struct{}{}
	h.mu.Unlock()
	err := h.ln.Close()
	if up != nil {
		up.Close()
	}
	for c := range conns {
		c.Close()
	}
	return err
}
