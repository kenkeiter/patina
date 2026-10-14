package vmm

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/kenkeiter/patina/cli/internal/ctl"
)

// procMachine is a VMM running as a child process whose serial console is
// its stdout (Firecracker, Cloud Hypervisor). Stopping the guest is the
// control channel's job; Kill is SIGKILL.
type procMachine struct {
	name    string
	cfg     *Config
	control ctl.Endpoint
	logFile string // VMM log file replayed to Stderr if the VMM fails

	cmd    *exec.Cmd
	stdinW *os.File
	done   chan struct{} // closed once the process has exited
	err    error         // non-ExitError from cmd.Wait

	mu     sync.Mutex
	killed bool
}

// lookBin resolves the VMM binary: an explicit --vmm-bin, else PATH.
func lookBin(override, name, hint string) (string, error) {
	if override != "" {
		p, err := exec.LookPath(override)
		if err != nil {
			return "", fmt.Errorf("--vmm-bin %s: %w", override, err)
		}
		return p, nil
	}
	p, err := exec.LookPath(name)
	if err != nil {
		return "", fmt.Errorf("%s not found in PATH (%s; or pass --vmm-bin)", name, hint)
	}
	return p, nil
}

func startProc(ctx context.Context, name string, cfg *Config, bin string, args []string, control ctl.Endpoint, logFile string) (*procMachine, error) {
	if cfg.Stderr == nil {
		cfg.Stderr = io.Discard
	}
	if cfg.Log == nil {
		cfg.Log = io.Discard
	}
	if cfg.Serial == nil {
		cfg.Serial = io.Discard
	}
	// Serial input is the VMM's stdin. Give it a pipe nobody writes to:
	// /dev/null cannot be registered with epoll (Firecracker polls its
	// stdin), and the terminal belongs to `patina run`.
	stdinR, stdinW, err := os.Pipe()
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(bin, args...)
	cmd.Stdin = stdinR
	// Always a pipe, never our own *os.File: Firecracker sets O_NONBLOCK on
	// its stdout, which would leak onto a shared terminal file description.
	cmd.Stdout = struct{ io.Writer }{cfg.Serial}
	cmd.Stderr = cfg.Stderr
	// Own process group: Ctrl-C goes to `patina run`, which turns it into a
	// graceful stop instead of the VMM dying on SIGINT.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	fmt.Fprintf(cfg.Log, "+ %s %s\n", bin, shellQuote(args))
	if err := cmd.Start(); err != nil {
		stdinR.Close()
		stdinW.Close()
		return nil, fmt.Errorf("starting %s: %w", bin, err)
	}
	stdinR.Close()
	m := &procMachine{name: name, cfg: cfg, control: control, logFile: logFile, cmd: cmd, stdinW: stdinW, done: make(chan struct{})}
	go func() {
		err := cmd.Wait()
		if _, ok := err.(*exec.ExitError); !ok {
			m.err = err
		}
		stdinW.Close()
		close(m.done)
	}()
	go func() {
		select {
		case <-ctx.Done():
			m.Kill()
		case <-m.done:
		}
	}()
	return m, nil
}

func (m *procMachine) Control() ctl.Endpoint { return m.control }

func (m *procMachine) PID() int { return m.cmd.Process.Pid }

func (m *procMachine) exited() bool {
	select {
	case <-m.done:
		return true
	default:
		return false
	}
}

func (m *procMachine) Wait() (*Exit, error) {
	<-m.done
	if m.err != nil {
		return nil, m.err
	}
	code := m.cmd.ProcessState.ExitCode()
	m.mu.Lock()
	killed := m.killed
	m.mu.Unlock()
	if code != 0 && !killed {
		m.replayLog()
	}
	return &Exit{Code: code}, nil
}

func (m *procMachine) Kill() error {
	m.mu.Lock()
	m.killed = true
	m.mu.Unlock()
	if m.exited() {
		return nil
	}
	err := m.cmd.Process.Kill()
	if err != nil && m.exited() {
		return nil
	}
	return err
}

// abort kills the VMM after a failed start and waits for it.
func (m *procMachine) abort() {
	m.Kill()
	<-m.done
}

// replayLog copies the VMM's log file (if any) to Stderr.
func (m *procMachine) replayLog() {
	if m.logFile == "" {
		return
	}
	b, err := os.ReadFile(m.logFile)
	if err != nil || len(bytes.TrimSpace(b)) == 0 {
		return
	}
	const max = 8 << 10
	if len(b) > max {
		b = b[len(b)-max:]
	}
	fmt.Fprintf(m.cfg.Stderr, "patina: %s log (%s):\n%s", m.name, m.logFile, b)
	if b[len(b)-1] != '\n' {
		fmt.Fprintln(m.cfg.Stderr)
	}
}

// waitSocket polls until the Unix socket at path accepts connections, the
// process exits, or timeout passes.
func (m *procMachine) waitSocket(path string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for {
		c, err := net.DialTimeout("unix", path, time.Second)
		if err == nil {
			c.Close()
			return nil
		}
		if m.exited() {
			return fmt.Errorf("%s exited (status %d) before its API socket %s came up", m.name, m.cmd.ProcessState.ExitCode(), path)
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("%s API socket %s did not come up within %s: %v", m.name, path, timeout, err)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// APIRequest is one HTTP request to a VMM's REST API socket.
type APIRequest struct {
	Method string
	Path   string
	Body   any // marshalled as JSON; nil = no body
}

// BodyJSON is the request body as sent.
func (r APIRequest) BodyJSON() []byte {
	if r.Body == nil {
		return nil
	}
	b, err := json.Marshal(r.Body)
	if err != nil {
		panic(err) // plain structs only
	}
	return b
}

func (r APIRequest) String() string {
	if b := r.BodyJSON(); b != nil {
		return r.Method + " " + r.Path + " " + string(b)
	}
	return r.Method + " " + r.Path
}

// apiClient speaks HTTP/1.1 over a Unix socket (Firecracker's --api-sock,
// Cloud Hypervisor's --api-socket). The host part of the URL is ignored.
type apiClient struct {
	sock string
	hc   *http.Client
}

func newAPIClient(sock string) *apiClient {
	return &apiClient{sock: sock, hc: &http.Client{
		Timeout: 30 * time.Second,
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				var d net.Dialer
				return d.DialContext(ctx, "unix", sock)
			},
			DisableCompression: true,
		},
	}}
}

// do sends r and returns the response body; non-2xx statuses are errors
// carrying Firecracker's fault_message or the body text.
func (c *apiClient) do(r APIRequest) ([]byte, error) {
	var body io.Reader
	b := r.BodyJSON()
	if b != nil {
		body = bytes.NewReader(b)
	}
	req, err := http.NewRequest(r.Method, "http://localhost"+r.Path, body)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/json")
	if b != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := c.hc.Do(req)
	if err != nil {
		return nil, fmt.Errorf("%s %s: %w", r.Method, r.Path, err)
	}
	defer resp.Body.Close()
	rb, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode/100 != 2 {
		msg := strings.TrimSpace(string(rb))
		var fault struct {
			FaultMessage string `json:"fault_message"`
		}
		if json.Unmarshal(rb, &fault) == nil && fault.FaultMessage != "" {
			msg = fault.FaultMessage
		}
		if msg == "" {
			msg = "(no body)"
		}
		return nil, fmt.Errorf("%s %s: %s: %s", r.Method, r.Path, resp.Status, msg)
	}
	return rb, nil
}

func (c *apiClient) close() { c.hc.CloseIdleConnections() }
