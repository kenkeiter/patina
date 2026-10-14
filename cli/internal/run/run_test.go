package run

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/kenkeiter/patina/cli/internal/cpio"
	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/image"
	"github.com/kenkeiter/patina/cli/internal/runstate"
	"github.com/kenkeiter/patina/cli/internal/target"
	"github.com/kenkeiter/patina/cli/internal/vmm"
)

func TestSentinelWriter(t *testing.T) {
	var out bytes.Buffer
	s := &sentinelWriter{w: &out}
	in := "boot\r\n[patina] exit code 3\r\nnoise [patina] exit code 9\n" + strings.Repeat("x", 1000) + "\n[pat"
	for i := 0; i < len(in); i += 7 { // arbitrary chunking
		j := i + 7
		if j > len(in) {
			j = len(in)
		}
		s.Write([]byte(in[i:j]))
	}
	if out.String() != in {
		t.Fatal("output not passed through verbatim")
	}
	if c := s.Code(); c == nil || *c != 3 {
		t.Fatalf("code %v", c)
	}
	s.Write([]byte("ina] exit code 42\n"))
	if c := s.Code(); c == nil || *c != 42 {
		t.Fatalf("split sentinel: code %v", c)
	}
}

// CLI messages written while the console is mid-line start on a new line.
func TestNoteWriter(t *testing.T) {
	var out bytes.Buffer
	s := &sentinelWriter{w: &out}
	n := &noteWriter{s: s, w: &out}
	s.Write([]byte("boot ok\n[patina] tim"))
	n.Write([]byte("patina: control: ready\n"))
	s.Write([]byte("er: x\n"))
	n.Write([]byte("patina: control: exit code 0\n"))
	want := "boot ok\n[patina] tim\npatina: control: ready\ner: x\npatina: control: exit code 0\n"
	if out.String() != want {
		t.Fatalf("got %q\nwant %q", out.String(), want)
	}
}

type syncBuf struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (s *syncBuf) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.Write(p)
}

func (s *syncBuf) String() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.String()
}

// fakeImage bundles a fake kernel into a .patina for arch.
func fakeImage(t *testing.T, arch string, kernel []byte) string {
	t.Helper()
	dir := t.TempDir()
	kp := filepath.Join(dir, "kernel")
	if err := os.WriteFile(kp, kernel, 0o644); err != nil {
		t.Fatal(err)
	}
	b := cpio.NewBuilder()
	b.AddBytes("/app", 0o755, 0, []byte("not really an ELF"))
	ip := filepath.Join(dir, "initrd.cpio")
	f, _ := os.Create(ip)
	b.WriteTo(f)
	f.Close()
	out := filepath.Join(dir, "fake.patina")
	m := &image.Manifest{Name: "fake", Arch: arch, VMM: target.QEMU, Target: target.QEMU + "-" + arch, GoVersion: "go0", Created: time.Now()}
	if err := image.Write(out, m, kp, ip); err != nil {
		t.Fatal(err)
	}
	return out
}

func needQEMU(t *testing.T, arch string) {
	t.Helper()
	if testing.Short() {
		t.Skip("boots QEMU")
	}
	if _, err := exec.LookPath(vmm.QEMUBinary(arch)); err != nil {
		t.Skipf("%s not installed", vmm.QEMUBinary(arch))
	}
	d, err := os.MkdirTemp("/tmp", "prun")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(d) })
	t.Setenv("XDG_RUNTIME_DIR", d)
}

func runFake(t *testing.T, arch string, kernel []byte, o Options) (int, string, string) {
	t.Helper()
	needQEMU(t, arch)
	var stdout, stderr syncBuf
	o.Image = fakeImage(t, arch, kernel)
	o.Stdout, o.Stderr = &stdout, &stderr
	if o.MemMB == 0 {
		o.MemMB, o.CPUs = 128, 1
	}
	o.Verbose = true
	code, err := Run(o)
	if err != nil {
		t.Fatalf("Run: %v\nstderr:\n%s", err, stderr.String())
	}
	return code, stdout.String(), stderr.String()
}

func TestQEMUAArch64SentinelExit(t *testing.T) {
	code, out, errs := runFake(t, target.AArch64, fakeAArch64Image("hello from fake arm64\n[patina] exit code 7\n", false), Options{Timeout: 60 * time.Second})
	if code != 7 || !strings.Contains(out, "hello from fake arm64") {
		t.Fatalf("code %d\nstdout:\n%s\nstderr:\n%s", code, out, errs)
	}
	if !strings.Contains(errs, "virtserialport,bus=vser0.0,chardev=ctl0,name=patina.ctl") {
		t.Fatalf("control port not configured:\n%s", errs)
	}
	if vmm.AutoAccel(target.AArch64) == "hvf" && (!strings.Contains(errs, "-accel hvf -cpu host") || strings.Contains(errs, "falling back")) {
		t.Fatalf("expected to run under hvf:\n%s", errs)
	}
}

func TestQEMUX86DebugExitFallback(t *testing.T) {
	code, out, errs := runFake(t, target.X86_64, fakeX86PVH("hello from fake x86\n", 5, false), Options{Timeout: 120 * time.Second})
	if code != 5 || !strings.Contains(out, "hello from fake x86") {
		t.Fatalf("code %d\nstdout:\n%s\nstderr:\n%s", code, out, errs)
	}
}

func TestQEMUX86SentinelBeatsDebugExit(t *testing.T) {
	code, out, errs := runFake(t, target.X86_64, fakeX86PVH("[patina] exit code 9\n", 3, false), Options{Timeout: 120 * time.Second})
	if code != 9 {
		t.Fatalf("code %d\nstdout:\n%s\nstderr:\n%s", code, out, errs)
	}
}

func TestQEMUTimeoutWithMuteGuest(t *testing.T) {
	start := time.Now()
	code, out, errs := runFake(t, target.AArch64, fakeAArch64Image("spinning\n", true), Options{Timeout: 2 * time.Second, Grace: 500 * time.Millisecond})
	if code != ExitTimeout {
		t.Fatalf("code %d\nstdout:\n%s\nstderr:\n%s", code, out, errs)
	}
	if d := time.Since(start); d > 20*time.Second {
		t.Fatalf("took %v", d)
	}
	if !strings.Contains(errs, "killing VM") {
		t.Fatalf("expected forced kill after grace:\n%s", errs)
	}
}

// TestQEMUInterruptAndCtl starts a guest that never exits, checks the run
// state and control proxy while it runs, then interrupts it like Ctrl-C.
func TestQEMUInterruptAndCtl(t *testing.T) {
	needQEMU(t, target.AArch64)
	sigs := make(chan os.Signal, 2)
	var stdout, stderr syncBuf
	type result struct {
		code int
		err  error
	}
	res := make(chan result, 1)
	go func() {
		code, err := Run(Options{
			Image: fakeImage(t, target.AArch64, fakeAArch64Image("up\n", true)),
			Name:  "itest", MemMB: 128, CPUs: 1, Grace: 300 * time.Millisecond,
			Timeout: 60 * time.Second, Stdout: &stdout, Stderr: &stderr, Signals: sigs,
		})
		res <- result{code, err}
	}()
	var st *runstate.State
	deadline := time.Now().Add(15 * time.Second)
	for time.Now().Before(deadline) {
		if s, err := runstate.Load("itest"); err == nil && strings.Contains(stdout.String(), "up") {
			st = s
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	if st == nil {
		t.Fatalf("run state never appeared / guest never printed\nstderr:\n%s", stderr.String())
	}
	if st.VMM != "qemu" || st.Arch != target.AArch64 || st.Upstream.Path == "" {
		t.Fatalf("state %+v", st)
	}
	c, err := ctl.Dial(st.Control, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	// The fake guest never answers; the proxy must stay usable regardless.
	// Until the proxy reaches QEMU's socket it replies "not connected".
	for i := 0; ; i++ {
		_, err := ctl.DoPing(c, 1, 300*time.Millisecond, nil)
		if err == ctl.ErrTimeout {
			break
		}
		if i > 20 || err == nil || !strings.Contains(err.Error(), "not connected") {
			t.Fatalf("ping against mute guest: %v", err)
		}
		time.Sleep(100 * time.Millisecond)
	}
	c.Close()

	sigs <- os.Interrupt
	select {
	case r := <-res:
		if r.err != nil || r.code != ExitKilled {
			t.Fatalf("code %d err %v\nstderr:\n%s", r.code, r.err, stderr.String())
		}
	case <-time.After(20 * time.Second):
		t.Fatal("run did not finish after interrupt")
	}
	if _, err := runstate.Load("itest"); err == nil {
		t.Fatal("run state not cleaned up")
	}
}
