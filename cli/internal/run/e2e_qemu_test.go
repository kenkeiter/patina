package run

// End-to-end tests of the patina CLI against the real kernel on QEMU:
// `patina build` the testapps, `patina run` them, and drive them with
// `patina ctl` from a second process. Opt-in, because they need the
// kernels in <repo>/out/ (scripts/build-kernel.sh) and QEMU:
//
//	cd cli && PATINA_E2E=1 go test ./internal/run -run E2E -v [-timeout 20m]
//	PATINA_E2E_ARCH=aarch64 (or x86_64) limits the arches.
//
// Every case has a hard 60 s deadline; the summary prints PASS/FAIL per
// case per arch.

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/kenkeiter/patina/cli/internal/build"
	"github.com/kenkeiter/patina/cli/internal/runstate"
	"github.com/kenkeiter/patina/cli/internal/target"
	"github.com/kenkeiter/patina/cli/internal/vmm"
)

const e2eCaseTimeout = 60 * time.Second

type e2eEnv struct {
	root   string   // repo root
	cli    string   // built patina binary
	env    []string // environment for every CLI process
	images map[string]string
	arch   string
}

// e2eProc is a running CLI process.
type e2eProc struct {
	cmd      *exec.Cmd
	out, err syncBuf
	done     chan struct{}
	code     int
}

func (e *e2eEnv) start(t *testing.T, args ...string) *e2eProc {
	t.Helper()
	p := &e2eProc{cmd: exec.Command(e.cli, args...), done: make(chan struct{})}
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
	return p
}

// wait returns the exit code, or fails the test after d (killing the run
// and its VMM).
func (p *e2eProc) wait(t *testing.T, d time.Duration) int {
	t.Helper()
	select {
	case <-p.done:
		return p.code
	case <-time.After(d):
	}
	p.stop()
	t.Fatalf("%v: still running after %v\nstdout:\n%s\nstderr:\n%s", p.cmd.Args, d, tail(p.out.String()), p.err.String())
	return -1
}

// stop ends a run that is still going: two interrupts (graceful stop, then
// kill the VMM), then SIGKILL.
func (p *e2eProc) stop() {
	for _, s := range []os.Signal{os.Interrupt, os.Interrupt} {
		select {
		case <-p.done:
			return
		default:
		}
		p.cmd.Process.Signal(s)
		time.Sleep(300 * time.Millisecond)
	}
	select {
	case <-p.done:
	case <-time.After(3 * time.Second):
		p.cmd.Process.Kill()
		<-p.done
	}
}

// waitOut waits until stdout matches re.
func (p *e2eProc) waitOut(t *testing.T, re string, d time.Duration) {
	t.Helper()
	rx := regexp.MustCompile(re)
	end := time.Now().Add(d)
	for time.Now().Before(end) {
		if rx.MatchString(p.out.String()) {
			return
		}
		select {
		case <-p.done:
			if rx.MatchString(p.out.String()) {
				return
			}
			t.Fatalf("run exited %d before printing /%s/\nstdout:\n%s\nstderr:\n%s", p.code, re, tail(p.out.String()), p.err.String())
		case <-time.After(50 * time.Millisecond):
		}
	}
	p.stop()
	t.Fatalf("no /%s/ within %v\nstdout:\n%s\nstderr:\n%s", re, d, tail(p.out.String()), p.err.String())
}

// ctl runs `patina ctl <args>` to completion.
func (e *e2eEnv) ctl(t *testing.T, args ...string) (int, string, string) {
	t.Helper()
	p := e.start(t, append([]string{"ctl"}, args...)...)
	code := p.wait(t, 40*time.Second)
	return code, p.out.String(), p.err.String()
}

func tail(s string) string {
	l := strings.Split(s, "\n")
	if len(l) > 40 {
		l = l[len(l)-40:]
	}
	return strings.Join(l, "\n")
}

func mustMatch(t *testing.T, what, s, re string) {
	t.Helper()
	if !regexp.MustCompile(re).MatchString(s) {
		t.Fatalf("%s lacks /%s/:\n%s", what, re, tail(s))
	}
}

// runArgs are the common `patina run` flags: small VMs, one name per case.
func (e *e2eEnv) runArgs(name, image string, extra ...string) []string {
	a := []string{"run", "--name", name, "--mem", "256", "--cpus", "2"}
	a = append(a, extra...)
	return append(a, e.images[image])
}

// startSignals boots testapps/signals with prog args and waits until its
// handlers are installed ("signals: ready").
func (e *e2eEnv) startSignals(t *testing.T, name string, runFlags []string, progArgs ...string) *e2eProc {
	t.Helper()
	a := e.runArgs(name, "signals", runFlags...)
	if len(progArgs) > 0 {
		a = append(append(a, "--"), progArgs...)
	}
	p := e.start(t, a...)
	p.waitOut(t, `(?m)^signals: ready`, 45*time.Second)
	if _, err := runstate.Load(name); err != nil {
		p.stop()
		t.Fatalf("run state: %v", err)
	}
	return p
}

func (e *e2eEnv) checkCleanedUp(t *testing.T, name string) {
	t.Helper()
	dir, _ := runstate.Dir(name)
	if _, err := os.Stat(dir); err == nil {
		t.Fatalf("run dir %s left behind", dir)
	}
}

type e2eCase struct {
	name string
	fn   func(t *testing.T, e *e2eEnv, vm string)
}

var e2eCases = []e2eCase{
	{"hello-args", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.start(t, append(e.runArgs(vm, "hello", "-v"), "--", "3", "a", "b")...)
		if c := p.wait(t, e2eCaseTimeout); c != 3 {
			t.Fatalf("exit %d, want 3\n%s\n%s", c, tail(p.out.String()), p.err.String())
		}
		mustMatch(t, "stdout", p.out.String(), `(?m)^argv: \["/app" "3" "a" "b"\]\r?$`)
		mustMatch(t, "stdout", p.out.String(), `(?m)^PASS hello \(exit 3\)`)
		mustMatch(t, "stderr", p.err.String(), `(?m)^patina: control: ready: go1\.[0-9.]+, 2 cpus, 2[0-9]{2} MiB$`)
		if strings.Contains(p.err.String(), "QEMU waiting for connection") {
			t.Fatalf("QEMU chatter on stderr:\n%s", p.err.String())
		}
	}},
	{"stop-handler", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.startSignals(t, vm, nil)
		c, out, errs := e.ctl(t, vm, "stop")
		if c != 0 {
			t.Fatalf("ctl stop exit %d\n%s%s", c, out, errs)
		}
		mustMatch(t, "ctl stdout", out, `\{"ev":"exit","code":0,"forced":false\}`)
		if c := p.wait(t, 20*time.Second); c != 0 {
			t.Fatalf("run exit %d, want 0\n%s", c, tail(p.out.String()))
		}
		mustMatch(t, "stdout", p.out.String(), `(?m)^PASS signals \(handler\)`)
	}},
	{"stop-nohandler", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.startSignals(t, vm, nil, "-mode", "nohandler")
		c, out, errs := e.ctl(t, vm, "stop")
		if c != 143 {
			t.Fatalf("ctl stop exit %d, want 143\n%s%s", c, out, errs)
		}
		mustMatch(t, "ctl stdout", out, `\{"ev":"exit","code":143,"signal":"SIGTERM","forced":false\}`)
		if c := p.wait(t, 20*time.Second); c != 143 {
			t.Fatalf("run exit %d, want 143\n%s", c, tail(p.out.String()))
		}
	}},
	{"stop-forced", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.startSignals(t, vm, nil, "-mode", "ignore")
		start := time.Now()
		c, out, errs := e.ctl(t, vm, "stop", "--grace", "1s")
		if c != 137 {
			t.Fatalf("ctl stop exit %d, want 137\n%s%s", c, out, errs)
		}
		mustMatch(t, "ctl stdout", out, `\{"ev":"exit","code":137,"forced":true\}`)
		if c := p.wait(t, 20*time.Second); c != 137 {
			t.Fatalf("run exit %d, want 137\n%s", c, tail(p.out.String()))
		}
		if d := time.Since(start); d > 3500*time.Millisecond {
			t.Fatalf("forced stop took %v (grace 1s)", d)
		}
	}},
	{"ping", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.startSignals(t, vm, nil, "-mode", "usr1")
		c, out, errs := e.ctl(t, vm, "ping")
		if c != 0 {
			t.Fatalf("ctl ping exit %d\n%s%s", c, out, errs)
		}
		mustMatch(t, "ctl stdout", out, `\{"ev":"pong","id":[0-9]+,"uptime_ms":[1-9][0-9]*\}`)
		if c, out, errs := e.ctl(t, vm, "stop"); c != 0 {
			t.Fatalf("ctl stop exit %d\n%s%s", c, out, errs)
		}
		if c := p.wait(t, 20*time.Second); c != 0 {
			t.Fatalf("run exit %d", c)
		}
	}},
	{"dump", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.startSignals(t, vm, nil)
		if c, out, errs := e.ctl(t, vm, "dump"); c != 0 {
			t.Fatalf("ctl dump exit %d\n%s%s", c, out, errs)
		}
		if c := p.wait(t, 20*time.Second); c != 2 {
			t.Fatalf("run exit %d, want 2\n%s", c, tail(p.out.String()))
		}
		mustMatch(t, "stdout", p.out.String(), `(?m)^SIGQUIT: quit`)
		mustMatch(t, "stdout", p.out.String(), `(?m)^goroutine 1 .*\[`) // "goroutine 1 gp=... m=... [running]:"
	}},
	{"signal-usr1", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.startSignals(t, vm, nil, "-mode", "usr1")
		if c, out, errs := e.ctl(t, vm, "signal", "SIGUSR1"); c != 0 {
			t.Fatalf("ctl signal exit %d\n%s%s", c, out, errs)
		}
		p.waitOut(t, `(?m)^signals: SIGUSR1 at [0-9]+`, 10*time.Second)
		if c, out, errs := e.ctl(t, vm, "stop"); c != 0 {
			t.Fatalf("ctl stop exit %d\n%s%s", c, out, errs)
		}
		if c := p.wait(t, 20*time.Second); c != 0 {
			t.Fatalf("run exit %d", c)
		}
	}},
	{"timeout", func(t *testing.T, e *e2eEnv, vm string) {
		// Handler mode exits 0 on the timeout's SIGTERM; the run still
		// reports 124, like timeout(1).
		start := time.Now()
		p := e.start(t, e.runArgs(vm, "signals", "--timeout", "3s")...)
		if c := p.wait(t, e2eCaseTimeout); c != ExitTimeout {
			t.Fatalf("exit %d, want 124\n%s\n%s", c, tail(p.out.String()), p.err.String())
		}
		mustMatch(t, "stdout", p.out.String(), `(?m)^PASS signals \(handler\)`)
		mustMatch(t, "stderr", p.err.String(), `patina: timeout after 3s; stopping`)
		if d := time.Since(start); d < 3*time.Second {
			t.Fatalf("returned after %v", d)
		}
	}},
	{"interrupt-nohandler", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.startSignals(t, vm, nil, "-mode", "nohandler")
		p.cmd.Process.Signal(os.Interrupt)
		if c := p.wait(t, 20*time.Second); c != 143 {
			t.Fatalf("exit %d, want 143\n%s\n%s", c, tail(p.out.String()), p.err.String())
		}
		mustMatch(t, "stderr", p.err.String(), `patina: interrupted; stopping \(grace 10s`)
	}},
	{"interrupt-twice", func(t *testing.T, e *e2eEnv, vm string) {
		p := e.startSignals(t, vm, nil, "-mode", "ignore")
		p.cmd.Process.Signal(os.Interrupt)
		time.Sleep(500 * time.Millisecond)
		start := time.Now()
		p.cmd.Process.Signal(syscall.SIGINT)
		if c := p.wait(t, 20*time.Second); c != ExitKilled {
			t.Fatalf("exit %d, want 137\n%s\n%s", c, tail(p.out.String()), p.err.String())
		}
		if d := time.Since(start); d > 3*time.Second {
			t.Fatalf("second interrupt took %v to kill", d)
		}
		mustMatch(t, "stderr", p.err.String(), `patina: killing VM`)
	}},
}

func TestE2EQEMU(t *testing.T) {
	if os.Getenv("PATINA_E2E") != "1" {
		t.Skip("set PATINA_E2E=1 to boot the real kernel under QEMU")
	}
	wd, _ := os.Getwd()
	root, ok := build.RepoRoot(wd)
	if !ok {
		t.Skip("not in a patina checkout")
	}
	tmp := t.TempDir()
	cli := filepath.Join(tmp, "patina")
	gb := exec.Command("go", "build", "-o", cli, ".")
	gb.Dir = filepath.Join(root, "cli")
	if out, err := gb.CombinedOutput(); err != nil {
		t.Fatalf("building the CLI: %v\n%s", err, out)
	}
	// Short run-state base: Unix socket paths are limited to 103 bytes.
	xdg, err := os.MkdirTemp("/tmp", "pe2e")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(xdg) })
	t.Setenv("XDG_RUNTIME_DIR", xdg) // for runstate.Load in this process
	env := append(os.Environ(), "XDG_RUNTIME_DIR="+xdg, "PATINA_KERNEL_DIR="+filepath.Join(root, "out"))

	arches := []string{target.AArch64, target.X86_64}
	if a := os.Getenv("PATINA_E2E_ARCH"); a != "" {
		arches = strings.Fields(strings.ReplaceAll(a, ",", " "))
	}
	var (
		mu      sync.Mutex
		results []string
	)
	t.Cleanup(func() {
		fmt.Println("---- patina CLI e2e (QEMU) ----")
		for _, r := range results {
			fmt.Println(r)
		}
	})
	for _, arch := range arches {
		t.Run(arch, func(t *testing.T) {
			record := func(c, s string) {
				mu.Lock()
				results = append(results, fmt.Sprintf("%s %s %s", s, arch, c))
				mu.Unlock()
			}
			if _, err := exec.LookPath(vmm.QEMUBinary(arch)); err != nil {
				record("all", "SKIP")
				t.Skipf("%s not installed", vmm.QEMUBinary(arch))
			}
			if _, err := os.Stat(filepath.Join(root, "out", target.KernelFile(arch))); err != nil {
				record("all", "SKIP")
				t.Skipf("no kernel: scripts/build-kernel.sh %s", arch)
			}
			e := &e2eEnv{root: root, cli: cli, env: env, images: map[string]string{}, arch: arch}
			for _, app := range []string{"hello", "signals"} {
				img := filepath.Join(tmp, app+"-"+arch+".patina")
				ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
				b := exec.CommandContext(ctx, cli, "build", "./"+app, "--target", "qemu-"+arch, "-o", img)
				b.Dir, b.Env = filepath.Join(root, "testapps"), env
				out, err := b.CombinedOutput()
				cancel()
				if err != nil {
					record("build", "FAIL")
					t.Fatalf("patina build %s: %v\n%s", app, err, out)
				}
				e.images[app] = img
			}
			for _, c := range e2eCases {
				vm := "e2e-" + arch + "-" + c.name
				ran := false
				ok := t.Run(c.name, func(t *testing.T) {
					ran = true
					// Every wait inside a case is bounded (see e2eProc.wait).
					c.fn(t, e, vm)
					e.checkCleanedUp(t, vm)
				})
				if !ran { // filtered out by -run
					continue
				}
				s := "PASS"
				if !ok {
					s = "FAIL"
				}
				record(c.name, s)
			}
		})
	}
}
