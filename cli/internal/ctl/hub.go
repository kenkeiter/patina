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
	quiesce   chan struct{} // closed by Quiesce: no new upstream dials
	quiOnce   sync.Once
	loopDone  chan struct{} // closed when upstreamLoop returns
}

// Upstream dial retry intervals: fast until the first connection (a guest
// that exits within milliseconds of listening must still be reached before
// it powers off; a refused connect costs ~1 ms on Apple VZ, less on a Unix
// socket), then slower for reconnects.
const (
	firstDialRetry = 5 * time.Millisecond
	redialRetry    = 100 * time.Millisecond
)

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
		quiesce:   make(chan struct{}),
		loopDone:  make(chan struct{}),
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

// Quiesce stops dialing the upstream and waits (at most timeout) until the
// connection that is up, if any, has ended and every event it carried has
// been delivered to OnEvent. `patina run` calls it once the VM is gone, so
// it waits exactly as long as buffered events (the exit event) need, and
// not at all when the channel never attached.
func (h *Hub) Quiesce(timeout time.Duration) bool {
	h.quiOnce.Do(func() { close(h.quiesce) })
	select {
	case <-h.loopDone:
		return true
	case <-time.After(timeout):
		return false
	}
}

func (h *Hub) quiescing() bool {
	select {
	case <-h.done:
		return true
	case <-h.quiesce:
		return true
	default:
		return false
	}
}

func (h *Hub) upstreamLoop() {
	defer close(h.loopDone)
	retry := firstDialRetry
	for {
		if h.quiescing() {
			return
		}
		c, err := Dial(h.upstream, 2*time.Second)
		if err != nil {
			select {
			case <-h.done:
				return
			case <-h.quiesce:
				return
			case <-time.After(retry):
			}
			continue
		}
		retry = redialRetry
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
