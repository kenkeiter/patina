//go:build darwin && arm64 && cgo

package vmm

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/kenkeiter/patina/cli/internal/cpio"
	"github.com/kenkeiter/patina/cli/internal/target"
)

// lineLog collects console output and lets a test wait for a line.
type lineLog struct {
	mu   sync.Mutex
	buf  bytes.Buffer
	cond *sync.Cond
}

func newLineLog() *lineLog {
	l := &lineLog{}
	l.cond = sync.NewCond(&l.mu)
	return l
}

func (l *lineLog) Write(p []byte) (int, error) {
	l.mu.Lock()
	l.buf.Write(p)
	l.mu.Unlock()
	l.cond.Broadcast()
	return len(p), nil
}

func (l *lineLog) String() string {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.buf.String()
}

// wait returns the first submatch of re in the log, waiting up to d.
func (l *lineLog) wait(re *regexp.Regexp, d time.Duration) ([]string, bool) {
	t := time.AfterFunc(d, l.cond.Broadcast)
	defer t.Stop()
	end := time.Now().Add(d)
	l.mu.Lock()
	defer l.mu.Unlock()
	for {
		if m := re.FindStringSubmatch(l.buf.String()); m != nil {
			return m, true
		}
		if time.Now().After(end) {
			return nil, false
		}
		l.cond.Wait()
	}
}

func xorshift(n int, seed uint32) []byte {
	b := make([]byte, n)
	x := seed
	for i := range b {
		x ^= x << 13
		x ^= x >> 17
		x ^= x << 5
		b[i] = byte(x)
	}
	return b
}

// buildVsockApp cross-compiles testapps/vsockapp and packs it as /app.
func buildVsockApp(t *testing.T, root, dir string) string {
	t.Helper()
	app := filepath.Join(dir, "vsockapp")
	cmd := exec.Command("go", "build", "-o", app, "./vsockapp")
	cmd.Dir = filepath.Join(root, "testapps")
	cmd.Env = append(os.Environ(), "GOOS=linux", "GOARCH=arm64", "CGO_ENABLED=0")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("build vsockapp: %v\n%s", err, out)
	}
	b := cpio.NewBuilder()
	if err := b.AddHostFile("app", app, 0o755); err != nil {
		t.Fatal(err)
	}
	initrd := filepath.Join(dir, "initrd.cpio")
	f, err := os.Create(initrd)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	if _, err := b.WriteTo(f); err != nil {
		t.Fatal(err)
	}
	return initrd
}

// echoBytes sends n bytes on c while reading the echo (starting after
// stall: the guest's sends then wait for the host's credit), half-closes,
// and checks what came back.
func echoBytes(c net.Conn, n int, seed uint32, stall time.Duration) (time.Duration, error) {
	want := xorshift(n, seed)
	t0 := time.Now()
	c.SetDeadline(time.Now().Add(60 * time.Second))
	werr := make(chan error, 1)
	go func() {
		// Uneven writes, so guest packets and reads do not line up.
		var err error
		for off, k := 0, 1; off < n && err == nil; k++ {
			m := min(n-off, 1+(k*7919)%65536)
			_, err = c.Write(want[off : off+m])
			off += m
		}
		if err == nil {
			err = c.(interface{ CloseWrite() error }).CloseWrite()
		}
		werr <- err
	}()
	time.Sleep(stall)
	got, err := io.ReadAll(c)
	el := time.Since(t0)
	if err != nil {
		return el, fmt.Errorf("read after %d bytes: %v", len(got), err)
	}
	if err := <-werr; err != nil {
		return el, fmt.Errorf("write: %v", err)
	}
	if len(got) != n || sha256.Sum256(got) != sha256.Sum256(want) {
		return el, fmt.Errorf("echo mismatch: got %d bytes, want %d", len(got), n)
	}
	return el, nil
}

// TestVZVsock boots the real kernel with testapps/vsockapp on VZ and
// checks AF_VSOCK end to end (decision D76): the guest's local CID is the
// one VZ assigned; port 4242 stays the kernel's control channel while the
// program's sockets run; two host connections to the guest listener echo
// 1 MiB each, concurrently, in both directions; a host connect to a port
// nobody listens on fails promptly; the guest's own checks (loopback,
// guest->host connect to an unlistened host port -> ECONNRESET, socket()
// errors) pass. A third connection's host reader stalls for 300 ms, so the
// guest's echo runs out of credit and resumes on the host's updates. At
// 1, 2 and 4 vCPUs.
func TestVZVsock(t *testing.T) {
	root, _ := filepath.Abs("../../..")
	kernel := filepath.Join(root, "out", "kernel-aarch64.Image")
	if _, err := os.Stat(kernel); err != nil {
		t.Skipf("no kernel (scripts/build-kernel.sh aarch64): %v", err)
	}
	dir := t.TempDir()
	initrd := buildVsockApp(t, root, dir)
	for _, cpus := range []int{1, 2, 4} {
		t.Run(fmt.Sprintf("cpus=%d", cpus), func(t *testing.T) { vzVsock(t, kernel, initrd, cpus) })
	}
}

func vzVsock(t *testing.T, kernel, initrd string, cpus int) {
	const port = 5000
	const conns = 3
	serial := newLineLog()
	cfg := &Config{Arch: target.AArch64, Kernel: kernel, Initrd: initrd,
		Cmdline: fmt.Sprintf("console=hvc0 -- -port %d -conns %d -quit", port, conns),
		MemMB:   512, CPUs: cpus, RunDir: t.TempDir(), Serial: serial, Stderr: os.Stderr, Log: io.Discard}
	m, err := (&VZ{}).Start(context.Background(), cfg)
	if err != nil {
		if strings.Contains(err.Error(), "entitlement") {
			t.Skipf("test binary lacks the virtualization entitlement (use cli/scripts/vztest.sh): %v", err)
		}
		t.Fatal(err)
	}
	stopped := false
	defer func() {
		if !stopped {
			m.Kill()
			m.Wait()
		}
		if t.Failed() || os.Getenv("PATINA_VZ_CONSOLE") != "" {
			t.Logf("console:\n%s", serial.String())
		}
	}()
	vd, ok := m.(VsockDialer)
	if !ok {
		t.Fatal("VZ machine is not a VsockDialer")
	}

	mm, ok := serial.wait(regexp.MustCompile(`vsockapp: listening cid (\d+) port (\d+)`), 60*time.Second)
	if !ok {
		t.Fatal("guest never listened")
	}
	devCID, _ := serial.wait(regexp.MustCompile(`virtio-vsock: guest cid (\d+)`), 0)
	appCID, _ := serial.wait(regexp.MustCompile(`ok local-cid (\d+)`), 0)
	if devCID == nil || appCID == nil || appCID[1] != devCID[1] || mm[1] != devCID[1] || mm[2] != fmt.Sprint(port) {
		t.Fatalf("CIDs: device %v, IOCTL_VM_SOCKETS_GET_LOCAL_CID %v, listener %v", devCID, appCID, mm)
	}
	t.Logf("guest cid %s (VZ-assigned, from config space) = IOCTL_VM_SOCKETS_GET_LOCAL_CID = getsockname", devCID[1])
	for _, want := range []string{
		"ok loopback-echo cid 1 262144 bytes",
		"ok blocking connect host port 1",
		"ok non-blocking connect host port 1, SO_ERROR",
		"ok bind port 4242 -> address already in use",
	} {
		if !strings.Contains(serial.String(), want) {
			t.Errorf("guest did not report %q", want)
		}
	}
	if m, ok := serial.wait(regexp.MustCompile(`ok (blocking connect host port 1 \([^)]*\) -> .*)`), 0); ok {
		t.Logf("guest: %s", m[1])
	}

	// Nobody listens on port+1: the guest answers RST and the dial fails.
	t0 := time.Now()
	if c, err := vd.DialVsock(port+1, 5*time.Second); err == nil {
		c.Close()
		t.Error("connect to an unlistened guest port succeeded")
	} else {
		t.Logf("dial unlistened guest port %d: %v (%s)", port+1, err, time.Since(t0).Round(time.Millisecond))
		if time.Since(t0) > 2*time.Second {
			t.Error("refused dial was not prompt")
		}
	}

	// The control channel is still the kernel's, alongside the program's
	// sockets.
	cc, err := m.Control().Dialer(5 * time.Second)
	if err != nil {
		t.Fatalf("control channel dial: %v", err)
	}
	cc.SetDeadline(time.Now().Add(10 * time.Second))
	br := bufio.NewReader(cc)
	fmt.Fprintf(cc, "{\"op\":\"ping\",\"id\":7}\n")
	sawPong := false
	for !sawPong {
		line, err := br.ReadString('\n')
		if err != nil {
			t.Fatalf("control channel: %v", err)
		}
		sawPong = strings.Contains(line, `"ev":"pong"`) && strings.Contains(line, `"id":7`)
	}
	t.Log("control channel on 4242: ping -> pong")

	// Concurrent 1 MiB echoes; the last one with a stalled reader.
	const n = 1 << 20
	var wg sync.WaitGroup
	for i := range conns {
		stall := time.Duration(0)
		if i == conns-1 {
			stall = 300 * time.Millisecond
		}
		c, err := vd.DialVsock(port, 5*time.Second)
		if err != nil {
			t.Fatalf("dial guest port %d: %v", port, err)
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			defer c.Close()
			el, err := echoBytes(c, n, uint32(0x9e3779b9+i), stall)
			if err != nil {
				t.Errorf("conn %d: %v", i, err)
				return
			}
			t.Logf("conn %d: 1 MiB each way in %s (reader stalled %s)", i, el.Round(time.Millisecond), stall)
		}()
	}
	wg.Wait()
	cc.Close()
	// Let the guest exit (it waits so that no echo bytes are in flight
	// when it powers off).
	if q, err := vd.DialVsock(port, 5*time.Second); err != nil {
		t.Errorf("quit connection: %v", err)
	} else {
		q.Close()
	}

	if _, ok := serial.wait(regexp.MustCompile(`\[patina\] exit code (\d+)`), 30*time.Second); !ok {
		t.Fatal("guest did not exit")
	}
	ex, err := m.Wait()
	stopped = true
	out := serial.String()
	if err != nil || !strings.Contains(out, "PASS vsockapp") || !strings.Contains(out, "[patina] exit code 0") {
		t.Fatalf("guest result: exit %+v err %v", ex, err)
	}
	if c := strings.Count(out, "ok echo 1048576 bytes from cid 2 port"); c != conns {
		t.Errorf("guest reported %d complete echoes, want %d", c, conns)
	}
}
