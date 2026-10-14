package ctl

import (
	"bufio"
	"errors"
	"fmt"
	"io"
	"net"
	"strings"
	"sync"
	"time"
)

// Endpoint kinds.
const (
	// KindStream is a Unix stream socket that carries NDJSON directly: the
	// run-state proxy socket, QEMU's patina.ctl chardev, or `--sock`.
	KindStream = "stream"
	// KindFirecracker is a Firecracker / Cloud Hypervisor vsock Unix socket:
	// the host sends "CONNECT <port>\n" and expects "OK <hostport>\n".
	KindFirecracker = "fc-vsock"
	// KindVZ is an Apple Virtualization.framework vsock device. It has no
	// host path: only the `patina run` process that owns the VM can reach
	// it (through Endpoint.Dialer); everyone else goes through the proxy.
	KindVZ = "vz-vsock"
)

// Endpoint says how to reach a control channel.
type Endpoint struct {
	Kind string `json:"kind"`
	Path string `json:"path"`
	Port uint32 `json:"port,omitempty"` // KindFirecracker only; default 4242

	// Dialer, when set, opens the raw NDJSON stream instead of dialing Path
	// (in-process VMMs such as Apple VZ). Not serialized.
	Dialer func(timeout time.Duration) (net.Conn, error) `json:"-"`
}

func (e Endpoint) String() string {
	if e.Kind == KindFirecracker {
		return fmt.Sprintf("vsock %s port %d", e.Path, e.port())
	}
	if e.Kind == KindVZ {
		return fmt.Sprintf("vz vsock port %d", e.port())
	}
	return "socket " + e.Path
}

func (e Endpoint) port() uint32 {
	if e.Port == 0 {
		return Port
	}
	return e.Port
}

// maxLine bounds a single NDJSON line.
const maxLine = 1 << 20

// Client is a connection to a control channel.
type Client struct {
	conn net.Conn
	r    *bufio.Reader
	wmu  sync.Mutex
}

// Dial connects to ep, performing the vsock handshake when needed.
func Dial(ep Endpoint, timeout time.Duration) (*Client, error) {
	var conn net.Conn
	var err error
	switch {
	case ep.Dialer != nil:
		conn, err = ep.Dialer(timeout)
	case ep.Kind == KindVZ:
		err = errors.New("only reachable from the `patina run` process; use its proxy socket")
	default:
		conn, err = net.DialTimeout("unix", ep.Path, timeout)
	}
	if err != nil {
		return nil, fmt.Errorf("control channel %s: %w", ep, err)
	}
	c := &Client{conn: conn, r: bufio.NewReader(conn)}
	switch ep.Kind {
	case KindStream, KindVZ, "":
	case KindFirecracker:
		if err := c.handshake(ep.port(), timeout); err != nil {
			conn.Close()
			return nil, fmt.Errorf("control channel %s: %w", ep, err)
		}
	default:
		conn.Close()
		return nil, fmt.Errorf("unknown control endpoint kind %q", ep.Kind)
	}
	return c, nil
}

// NewClient wraps an existing connection (no handshake).
func NewClient(conn net.Conn) *Client { return &Client{conn: conn, r: bufio.NewReader(conn)} }

func (c *Client) handshake(port uint32, timeout time.Duration) error {
	if timeout > 0 {
		c.conn.SetDeadline(time.Now().Add(timeout))
		defer c.conn.SetDeadline(time.Time{})
	}
	if _, err := fmt.Fprintf(c.conn, "CONNECT %d\n", port); err != nil {
		return err
	}
	line, err := c.readLine()
	if err != nil {
		if errors.Is(err, io.EOF) {
			return fmt.Errorf("vsock CONNECT %d: connection closed (guest not listening yet?)", port)
		}
		return fmt.Errorf("vsock CONNECT %d: %w", port, err)
	}
	if !strings.HasPrefix(line, "OK ") && line != "OK" {
		return fmt.Errorf("vsock CONNECT %d: unexpected reply %q", port, line)
	}
	return nil
}

func (c *Client) readLine() (string, error) {
	var sb strings.Builder
	for {
		frag, err := c.r.ReadSlice('\n')
		sb.Write(frag)
		if sb.Len() > maxLine {
			return "", errors.New("control: line too long")
		}
		if err == bufio.ErrBufferFull {
			continue
		}
		if err != nil {
			if err == io.EOF && sb.Len() > 0 {
				return strings.TrimRight(sb.String(), "\r\n"), nil
			}
			return "", err
		}
		return strings.TrimRight(sb.String(), "\r\n"), nil
	}
}

// ReadLine returns the next non-empty raw line.
func (c *Client) ReadLine() (string, error) {
	for {
		l, err := c.readLine()
		if err != nil {
			return "", err
		}
		if strings.TrimSpace(l) != "" {
			return l, nil
		}
	}
}

// Next returns the next event. Malformed lines are returned as errors that
// wrap ErrMalformed so callers can skip them.
func (c *Client) Next() (*Event, error) {
	l, err := c.ReadLine()
	if err != nil {
		return nil, err
	}
	e, err := ParseEvent(l)
	if err != nil {
		return nil, fmt.Errorf("%w: %v", ErrMalformed, err)
	}
	return e, nil
}

// ErrMalformed marks an unparseable line.
var ErrMalformed = errors.New("malformed control line")

// Send writes one op.
func (c *Client) Send(o Op) error { return c.WriteLine(o.Encode()) }

// WriteLine writes raw bytes (must end in '\n') atomically w.r.t. Send.
func (c *Client) WriteLine(b []byte) error {
	c.wmu.Lock()
	defer c.wmu.Unlock()
	_, err := c.conn.Write(b)
	return err
}

// SetReadDeadline bounds the next reads.
func (c *Client) SetReadDeadline(t time.Time) error { return c.conn.SetReadDeadline(t) }

// Close closes the connection.
func (c *Client) Close() error { return c.conn.Close() }

// WaitFor reads events, passing each to onEvent, until match returns true,
// the deadline passes, or the stream ends.
func (c *Client) WaitFor(deadline time.Time, onEvent func(*Event), match func(*Event) bool) (*Event, error) {
	if !deadline.IsZero() {
		c.SetReadDeadline(deadline)
		defer c.SetReadDeadline(time.Time{})
	}
	for {
		e, err := c.Next()
		if errors.Is(err, ErrMalformed) {
			continue
		}
		if err != nil {
			var ne net.Error
			if errors.As(err, &ne) && ne.Timeout() {
				return nil, ErrTimeout
			}
			return nil, err
		}
		if onEvent != nil {
			onEvent(e)
		}
		if match(e) {
			return e, nil
		}
	}
}

// ErrTimeout is returned by WaitFor when the deadline passes.
var ErrTimeout = errors.New("timed out waiting for the guest")
