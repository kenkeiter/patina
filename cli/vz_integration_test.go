//go:build darwin && arm64

package main

// Apple Virtualization.framework integration tests: the real kernel
// (out/kernel-aarch64.Image) under `patina run --vmm vz`, through an
// entitlement-signed `patina` binary built by scripts/build.sh. They skip
// when the kernel is missing, codesign is unavailable, or the signed binary
// is still refused by VZ.
//
//	cd cli && go test -run TestVZBootHello -v .               (default, quick)
//	cd cli && PATINA_VZ_E2E=1 go test -run TestVZE2E -v .     (Milestone A on VZ)
//	cli/scripts/vztest.sh --e2e                               (the same)
//
// PATINA_NET_EXTERNAL=1 adds the httphello -dns lookup through VZ's NAT
// resolver. PATINA_PARAMS (extra kernel parameters) is passed through to
// every `patina run`.

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/kenkeiter/patina/cli/internal/runstate"
)

// vzEnv is a built, signed CLI plus images.
type vzEnv struct {
	root, bin, kernel string
	env               []string
	images            map[string]string
}

func setupVZ(t *testing.T, apps ...string) *vzEnv {
	t.Helper()
	root, _ := filepath.Abs("..")
	kernel := filepath.Join(root, "out", "kernel-aarch64.Image")
	if _, err := os.Stat(kernel); err != nil {
		t.Skip("no kernel: run scripts/build-kernel.sh aarch64")
	}
	if _, err := exec.LookPath("codesign"); err != nil {
		t.Skip("codesign not available")
	}
	d := t.TempDir()
	e := &vzEnv{root: root, bin: filepath.Join(d, "patina"), kernel: kernel, images: map[string]string{}}
	if out, err := exec.Command("scripts/build.sh", "-o", e.bin).CombinedOutput(); err != nil {
		t.Skipf("cannot build a signed CLI (scripts/build.sh): %v\n%s", err, out)
	}
	// Short run-state base: Unix socket paths are limited to 103 bytes.
	xdg, err := os.MkdirTemp("/tmp", "pvz")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(xdg) })
	t.Setenv("XDG_RUNTIME_DIR", xdg) // runstate.Load in this process
	e.env = append(os.Environ(), "XDG_RUNTIME_DIR="+xdg)
	for _, app := range apps {
		img := filepath.Join(d, app+".patina")
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
		b := exec.CommandContext(ctx, e.bin, "build", "./testapps/"+app, "--target", "vz-aarch64", "--kernel", kernel, "-o", img)
		b.Dir, b.Env = root, e.env
		out, err := b.CombinedOutput()
		cancel()
		if err != nil {
			t.Fatalf("patina build %s: %v\n%s", app, err, out)
		}
		e.images[app] = img
	}
	return e
}

// vzBuf is a goroutine-safe buffer for a child's output.
type vzBuf struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (b *vzBuf) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.b.Write(p)
}

func (b *vzBuf) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.b.String()
}

type vzProc struct {
	cmd      *exec.Cmd
	out, err vzBuf
	done     chan struct{}
	code     int
}

func (e *vzEnv) start(t *testing.T, args ...string) *vzProc {
	t.Helper()
	p := &vzProc{cmd: exec.Command(e.bin, args...), done: make(chan struct{})}
	p.cmd.Env = e.env
	p.cmd.Stdout, p.cmd.Stderr = &p.out, &p.err
	if err := p.cmd.Start(); err != nil {
		t.Fatal(err)
	}
	go func() {
		p.cmd.Wait()
		p.code = p.cmd.ProcessState.ExitCode()
		close(p.done)
	}()
	t.Cleanup(p.kill)
	return p
}

// kill ends a run that is still going (the VM lives in the run process).
func (p *vzProc) kill() {
	select {
	case <-p.done:
		return
	default:
	}
	p.cmd.Process.Signal(os.Interrupt)
	select {
	case <-p.done:
	case <-time.After(3 * time.Second):
		p.cmd.Process.Kill()
		<-p.done
	}
}

func (p *vzProc) wait(t *testing.T, d time.Duration) int {
	t.Helper()
	select {
	case <-p.done:
		return p.code
	case <-time.After(d):
	}
	p.kill()
	t.Fatalf("%v: still running after %v\nstdout:\n%s\nstderr:\n%s", p.cmd.Args, d, p.out.String(), p.err.String())
	return -1
}

// waitFor waits until s() matches re.
func (p *vzProc) waitFor(t *testing.T, what string, s func() string, re string, d time.Duration) []string {
	t.Helper()
	rx := regexp.MustCompile(re)
	end := time.Now().Add(d)
	for {
		if m := rx.FindStringSubmatch(s()); m != nil {
			return m
		}
		select {
		case <-p.done:
			if m := rx.FindStringSubmatch(s()); m != nil {
				return m
			}
			t.Fatalf("run exited %d before %s showed /%s/\nstdout:\n%s\nstderr:\n%s", p.code, what, re, p.out.String(), p.err.String())
		case <-time.After(20 * time.Millisecond):
		}
		if time.Now().After(end) {
			p.kill()
			t.Fatalf("no /%s/ on %s within %v\nstdout:\n%s\nstderr:\n%s", re, what, d, p.out.String(), p.err.String())
		}
	}
}

func (e *vzEnv) ctl(t *testing.T, args ...string) (int, string) {
	t.Helper()
	p := e.start(t, append([]string{"ctl"}, args...)...)
	code := p.wait(t, 40*time.Second)
	return code, p.out.String() + p.err.String()
}

func (e *vzEnv) run(name, app string, flags []string, progArgs ...string) []string {
	a := append([]string{"run", "--vmm", "vz", "--name", name, "--timeout", "90s"}, flags...)
	a = append(a, e.images[app])
	if len(progArgs) > 0 {
		a = append(append(a, "--"), progArgs...)
	}
	return a
}

func vzMustMatch(t *testing.T, what, s, re string) {
	t.Helper()
	if !regexp.MustCompile(re).MatchString(s) {
		t.Fatalf("%s lacks /%s/:\n%s", what, re, s)
	}
}

// skipIfRefused skips when VZ refused the binary (no entitlement).
func skipIfRefused(t *testing.T, stderr string) {
	t.Helper()
	if strings.Contains(stderr, "com.apple.security.virtualization") {
		t.Skipf("VZ refused the CLI (entitlement):\n%s", stderr)
	}
}

// TestVZBootHello boots testapps/hello and checks the VM starts and stops by
// itself, cleanly, within the timeout, with the program's exit code.
func TestVZBootHello(t *testing.T) {
	if testing.Short() {
		t.Skip("boots a VM")
	}
	e := setupVZ(t, "hello")
	p := e.start(t, e.run(fmt.Sprintf("vzhello-%d", os.Getpid()), "hello", []string{"-v", "--grace", "2s"}, "7")...)
	t0 := time.Now()
	code := p.wait(t, 60*time.Second)
	elapsed := time.Since(t0)
	skipIfRefused(t, p.err.String())
	t.Logf("patina run exit %d after %s\nstdout:\n%s\nstderr:\n%s", code, elapsed.Round(time.Millisecond), p.out.String(), p.err.String())
	if !strings.Contains(p.err.String(), "guest stopped the VM") {
		t.Fatalf("VM did not stop by itself (exit %d)", code)
	}
	if strings.Contains(p.err.String(), "patina: vz: ") && !strings.Contains(p.err.String(), "patina: vz: guest stopped") {
		t.Fatalf("VZ reported an error")
	}
	if code != 7 {
		t.Fatalf("exit %d, want 7", code)
	}
}

// bigSHA is sha256 of httphello's /big (D72), computed independently.
func bigSHA() string {
	b := make([]byte, 4<<20)
	for i := range b {
		b[i] = byte(i*31 + i>>12)
	}
	s := sha256.Sum256(b)
	return hex.EncodeToString(s[:])
}

var guestAddrRE = `(?m)^patina: guest address ([0-9.]+) \(gateway ([0-9.]+|none)\)$`

// TestVZE2E is Milestone A on VZ: exit codes, ctl stop / ping, and
// httphello on VZ NAT reached from the host at the guest's DHCP address.
func TestVZE2E(t *testing.T) {
	if os.Getenv("PATINA_VZ_E2E") != "1" {
		t.Skip("set PATINA_VZ_E2E=1 to run the VZ end-to-end tests")
	}
	e := setupVZ(t, "hello", "signals", "httphello")
	pid := os.Getpid()
	vm := func(c string) string { return fmt.Sprintf("vz%d-%s", pid, c) }

	t.Run("hello-exit", func(t *testing.T) {
		// A program that exits within ms of boot: the control channel must
		// still attach and deliver ready and exit (not just the sentinel).
		for i := 0; i < 3; i++ {
			p := e.start(t, e.run(vm(fmt.Sprintf("hello%d", i)), "hello", []string{"-v"}, "3", "a", "b")...)
			c := p.wait(t, 60*time.Second)
			skipIfRefused(t, p.err.String())
			if c != 3 {
				t.Fatalf("exit %d, want 3\n%s\n%s", c, p.out.String(), p.err.String())
			}
			vzMustMatch(t, "stdout", p.out.String(), `(?m)^argv: \["/app" "3" "a" "b"\]\r?$`)
			vzMustMatch(t, "stderr", p.err.String(), `(?m)^patina: control channel attached \(vz vsock port 4242\)$`)
			vzMustMatch(t, "stderr", p.err.String(), `(?m)^patina: control: ready: go1\.[0-9.]+, `)
			vzMustMatch(t, "stderr", p.err.String(), `(?m)^patina: control: exit code 3$`)
		}
	})

	signals := func(t *testing.T, name string, mode string) *vzProc {
		p := e.start(t, e.run(name, "signals", nil, "-mode", mode)...)
		p.waitFor(t, "stdout", p.out.String, `(?m)^signals: ready`, 45*time.Second)
		return p
	}
	t.Run("stop-handler", func(t *testing.T) {
		p := signals(t, vm("stoph"), "handler")
		c, out := e.ctl(t, vm("stoph"), "stop")
		if c != 0 {
			t.Fatalf("ctl stop exit %d\n%s", c, out)
		}
		vzMustMatch(t, "ctl", out, `\{"ev":"exit","code":0,"forced":false\}`)
		if c := p.wait(t, 20*time.Second); c != 0 {
			t.Fatalf("run exit %d\n%s", c, p.out.String())
		}
		vzMustMatch(t, "stdout", p.out.String(), `(?m)^PASS signals \(handler\)`)
	})
	t.Run("stop-nohandler", func(t *testing.T) {
		p := signals(t, vm("stopn"), "nohandler")
		c, out := e.ctl(t, vm("stopn"), "stop")
		if c != 143 {
			t.Fatalf("ctl stop exit %d, want 143\n%s", c, out)
		}
		vzMustMatch(t, "ctl", out, `\{"ev":"exit","code":143,"signal":"SIGTERM","forced":false\}`)
		if c := p.wait(t, 20*time.Second); c != 143 {
			t.Fatalf("run exit %d, want 143", c)
		}
	})
	t.Run("stop-forced", func(t *testing.T) {
		p := signals(t, vm("stopf"), "ignore")
		t0 := time.Now()
		c, out := e.ctl(t, vm("stopf"), "stop", "--grace", "1s")
		if c != 137 {
			t.Fatalf("ctl stop exit %d, want 137\n%s", c, out)
		}
		vzMustMatch(t, "ctl", out, `\{"ev":"exit","code":137,"forced":true\}`)
		if c := p.wait(t, 20*time.Second); c != 137 {
			t.Fatalf("run exit %d, want 137", c)
		}
		if d := time.Since(t0); d > 3500*time.Millisecond {
			t.Fatalf("forced stop took %v (grace 1s)", d)
		}
	})
	t.Run("ping", func(t *testing.T) {
		p := signals(t, vm("ping"), "usr1")
		c, out := e.ctl(t, vm("ping"), "ping")
		if c != 0 {
			t.Fatalf("ctl ping exit %d\n%s", c, out)
		}
		vzMustMatch(t, "ctl", out, `\{"ev":"pong","id":[0-9]+,"uptime_ms":[1-9][0-9]*\}`)
		if c, out := e.ctl(t, vm("ping"), "stop"); c != 0 {
			t.Fatalf("ctl stop exit %d\n%s", c, out)
		}
		if c := p.wait(t, 20*time.Second); c != 0 {
			t.Fatalf("run exit %d", c)
		}
	})

	// httphello on VZ NAT: find the guest at its DHCP address and serve the
	// host from it.
	serve := func(t *testing.T, name string, progArgs ...string) (*vzProc, string) {
		p := e.start(t, e.run(name, "httphello", []string{"--net", "user"}, progArgs...)...)
		t.Cleanup(func() {
			if t.Failed() {
				p.kill()
				t.Logf("patina run stdout:\n%s\nstderr:\n%s", vzTail(p.out.String()), p.err.String())
			}
		})
		m := p.waitFor(t, "stderr", p.err.String, guestAddrRE, 45*time.Second)
		ip := m[1]
		if m[2] != "192.168.64.1" && !strings.HasSuffix(m[2], ".1") {
			t.Logf("unexpected gateway %s", m[2])
		}
		vzMustMatch(t, "stdout", p.out.String(), `(?m)^\[patina\] net: dhcp lease `+regexp.QuoteMeta(ip)+`/[0-9]+ via `)
		st, err := runstate.Load(name)
		if err != nil || st.Net == nil || st.Net.Addr != ip {
			t.Fatalf("state.json net: %+v %v", st, err)
		}
		p.waitFor(t, "stdout", p.out.String, `(?m)^httphello: listening on`, 30*time.Second)
		return p, ip
	}
	t.Run("net-serve", func(t *testing.T) {
		p, ip := serve(t, vm("serve"))
		base := "http://" + ip + ":8080"
		// One connection per request, closed after it (like curl): a pooled
		// spare connection that never carries a request stays StateNew in
		// the guest's net/http, and Shutdown (5 s deadline in httphello)
		// only treats those as idle after 5 s.
		cl := &http.Client{Timeout: 20 * time.Second, Transport: &http.Transport{DisableKeepAlives: true}}
		get := func(path string) ([]byte, time.Duration, time.Duration, error) {
			t0 := time.Now()
			r, err := cl.Get(base + path)
			if err != nil {
				return nil, 0, 0, err
			}
			defer r.Body.Close()
			ttfb := time.Since(t0)
			b, err := io.ReadAll(r.Body)
			if err == nil && r.StatusCode != 200 {
				err = fmt.Errorf("status %s", r.Status)
			}
			return b, ttfb, time.Since(t0), err
		}
		// The first request is informational: right after macOS creates
		// the NAT bridge (no other NAT VM running) the host delays packets
		// in both directions for a few seconds (0.4-5.5 s observed; the
		// guest answers within 1 ms). Later requests must take
		// milliseconds: a missing interrupt would cost the net task's 1 s
		// poll.
		for i := 0; i < 5; i++ {
			b, ttfb, _, err := get("/")
			for try := 1; i == 0 && err != nil && try < 3; try++ {
				t.Logf("first GET: %v; retrying", err)
				b, ttfb, _, err = get("/")
			}
			if err != nil || string(b) != "hello from patina\n" {
				t.Fatalf("GET / = %q, %v", b, err)
			}
			t.Logf("GET / #%d: first byte after %v", i, ttfb.Round(10*time.Microsecond))
			if i > 0 && ttfb > 500*time.Millisecond {
				t.Errorf("GET / #%d took %v (interrupts not delivered?)", i, ttfb)
			}
		}
		var wg sync.WaitGroup
		errs := make(chan error, 20)
		for i := 0; i < 20; i++ {
			wg.Add(1)
			go func() {
				defer wg.Done()
				b, _, _, err := get("/")
				if err == nil && string(b) != "hello from patina\n" {
					err = fmt.Errorf("body %q", b)
				}
				if err != nil {
					errs <- err
				}
			}()
		}
		wg.Wait()
		close(errs)
		for err := range errs {
			t.Fatalf("concurrent GET: %v", err)
		}
		want := bigSHA()
		for i := 0; i < 2; i++ {
			b, ttfb, total, err := get("/big")
			if err != nil {
				t.Fatalf("GET /big: %v", err)
			}
			s := sha256.Sum256(b)
			if got := hex.EncodeToString(s[:]); got != want || len(b) != 4<<20 {
				t.Fatalf("/big: %d bytes sha256 %s, want %s", len(b), got, want)
			}
			t.Logf("GET /big: first byte %v, %d bytes in %v (%.1f MB/s)", ttfb.Round(10*time.Microsecond), len(b), total.Round(time.Millisecond), float64(len(b))/total.Seconds()/1e6)
		}
		c, out := e.ctl(t, vm("serve"), "stop")
		if c != 0 {
			p.wait(t, 20*time.Second)
			t.Fatalf("ctl stop exit %d\n%s\nstdout:\n%s", c, out, vzTail(p.out.String()))
		}
		vzMustMatch(t, "ctl", out, `\{"ev":"exit","code":0,"forced":false\}`)
		if c := p.wait(t, 20*time.Second); c != 0 {
			t.Fatalf("run exit %d\n%s", c, p.out.String())
		}
		vzMustMatch(t, "stdout", p.out.String(), `(?m)^PASS httphello \(graceful shutdown\)`)
	})
	t.Run("net-nohandler", func(t *testing.T) {
		p, ip := serve(t, vm("serven"), "-nohandler")
		r, err := (&http.Client{Timeout: 10 * time.Second}).Get("http://" + ip + ":8080/")
		if err != nil {
			t.Fatal(err)
		}
		r.Body.Close()
		c, out := e.ctl(t, vm("serven"), "stop")
		if c != 143 {
			t.Fatalf("ctl stop exit %d, want 143\n%s", c, out)
		}
		vzMustMatch(t, "ctl", out, `\{"ev":"exit","code":143,"signal":"SIGTERM","forced":false\}`)
		if c := p.wait(t, 20*time.Second); c != 143 {
			t.Fatalf("run exit %d, want 143", c)
		}
	})
	t.Run("net-selftest", func(t *testing.T) {
		args := []string{"-selftest"}
		if os.Getenv("PATINA_NET_EXTERNAL") == "1" {
			args = append(args, "-dns", "example.com")
		}
		p := e.start(t, e.run(vm("self"), "httphello", []string{"--net", "user"}, args...)...)
		if c := p.wait(t, 60*time.Second); c != 0 {
			t.Fatalf("exit %d\n%s\n%s", c, p.out.String(), p.err.String())
		}
		vzMustMatch(t, "stdout", p.out.String(), `(?m)^PASS httphello \(selftest\)`)
		vzMustMatch(t, "stderr", p.err.String(), guestAddrRE)
		if len(args) > 1 {
			vzMustMatch(t, "stdout", p.out.String(), `(?m)^ok dns example\.com -> `)
		}
	})
}

func vzTail(s string) string {
	l := strings.Split(s, "\n")
	if len(l) > 40 {
		l = l[len(l)-40:]
	}
	return strings.Join(l, "\n")
}
