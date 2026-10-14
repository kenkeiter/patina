package run

import (
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/runstate"
	"github.com/kenkeiter/patina/cli/internal/target"
	"github.com/kenkeiter/patina/cli/internal/vmm"
	"github.com/kenkeiter/patina/cli/internal/vmm/fakevmm"
)

// End-to-end `patina run --vmm firecracker|cloud-hypervisor` against the
// fake VMM binaries (internal/vmm/fakevmm): the test binary itself, linked
// as "firecracker" / "cloud-hypervisor" first on PATH.

func TestMain(m *testing.M) {
	fakevmm.MaybeRun()
	os.Exit(m.Run())
}

// setupFakeVMM installs the fake VMMs, configures the guest scenario, and
// gives the run dirs a short base (Unix socket path limits). It returns the
// fake's log path.
func setupFakeVMM(t *testing.T, scenario string, env ...string) string {
	t.Helper()
	_, log := fakevmm.Install(t)
	t.Setenv(fakevmm.EnvScenario, scenario)
	for _, k := range []string{fakevmm.EnvCode, fakevmm.EnvSentinel, fakevmm.EnvRefuse, fakevmm.EnvFail} {
		t.Setenv(k, "")
	}
	for i := 0; i+1 < len(env); i += 2 {
		t.Setenv(env[i], env[i+1])
	}
	d, err := os.MkdirTemp("/tmp", "pfk")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(d) })
	t.Setenv("XDG_RUNTIME_DIR", d)
	return log
}

func runVsockVMM(t *testing.T, vmmName, arch string, o Options) (int, string, string, error) {
	t.Helper()
	var stdout, stderr syncBuf
	o.Image = fakeImage(t, arch, []byte("fake kernel"))
	o.VMM = vmmName
	if o.Stdout != nil {
		o.Stdout = io.MultiWriter(o.Stdout, &stdout)
	} else {
		o.Stdout = &stdout
	}
	o.Stderr = &stderr
	if o.MemMB == 0 {
		o.MemMB, o.CPUs = 128, 2
	}
	if o.Timeout == 0 {
		o.Timeout = 30 * time.Second
	}
	o.Verbose = true
	code, err := Run(o)
	return code, stdout.String(), stderr.String(), err
}

func fakeLog(t *testing.T, path string) []fakevmm.Record {
	t.Helper()
	recs, err := fakevmm.ReadLog(path)
	if err != nil {
		t.Fatal(err)
	}
	return recs
}

func kinds(recs []fakevmm.Record, kind string) []fakevmm.Record {
	var out []fakevmm.Record
	for _, r := range recs {
		if r.Kind == kind {
			out = append(out, r)
		}
	}
	return out
}

func TestFirecrackerExitEvent(t *testing.T) {
	log := setupFakeVMM(t, "exit", fakevmm.EnvCode, "7", fakevmm.EnvSentinel, "99", fakevmm.EnvRefuse, "2")
	code, out, errs, err := runVsockVMM(t, target.Firecracker, target.X86_64, Options{
		Name: "fcexit", Net: vmm.NetConfig{Mode: "tap0"}, Args: []string{"hello world"}, Env: []string{"A=b c"},
	})
	if err != nil || code != 7 {
		t.Fatalf("code %d err %v (the exit event must win over the sentinel's 99)\nstdout:\n%s\nstderr:\n%s", code, err, out, errs)
	}
	if !strings.Contains(out, "hello from the fake guest") || !strings.Contains(out, "[patina] exit code 99") {
		t.Fatalf("console not streamed:\n%s", out)
	}

	recs := fakeLog(t, log)
	argv := kinds(recs, "argv")
	if len(argv) != 1 || len(argv[0].Argv) != 6 || argv[0].Argv[0] != "--api-sock" || !strings.HasSuffix(argv[0].Argv[1], "/fcexit/fc.sock") ||
		argv[0].Argv[2] != "--log-path" || argv[0].Argv[4] != "--level" || argv[0].Argv[5] != "Warning" {
		t.Fatalf("argv %+v", argv)
	}
	dir := filepath.Dir(argv[0].Argv[1])

	// The exact API sequence and bodies.
	api := kinds(recs, "api")
	var got []string
	for _, r := range api {
		got = append(got, r.Method+" "+r.Path)
	}
	want := "PUT /boot-source,PUT /machine-config,PUT /vsock,PUT /network-interfaces/eth0,PUT /entropy,PUT /actions"
	if strings.Join(got, ",") != want {
		t.Fatalf("API sequence\n got %s\nwant %s", strings.Join(got, ","), want)
	}
	var boot struct {
		Kernel   string `json:"kernel_image_path"`
		Initrd   string `json:"initrd_path"`
		BootArgs string `json:"boot_args"`
	}
	if err := json.Unmarshal(api[0].Body, &boot); err != nil {
		t.Fatal(err)
	}
	if boot.Kernel != filepath.Join(dir, "kernel-x86_64-pvh.elf") || boot.Initrd != filepath.Join(dir, "initrd.cpio") {
		t.Errorf("boot-source paths %+v (run dir %s)", boot, dir)
	}
	if !strings.HasPrefix(boot.BootArgs, "console=ttyS0 ip=dhcp patina.epoch=") || !strings.HasSuffix(boot.BootArgs, " patina.env=A=b%20c -- hello%20world") {
		t.Errorf("boot_args %q", boot.BootArgs)
	}
	for i, w := range []string{
		`{"vcpu_count":2,"mem_size_mib":128}`,
		`{"guest_cid":3,"uds_path":"` + filepath.Join(dir, "vsock.sock") + `"}`,
		`{"iface_id":"eth0","host_dev_name":"tap0","guest_mac":"` + vmm.GuestMAC(dir) + `"}`,
		`{}`,
		`{"action_type":"InstanceStart"}`,
	} {
		if string(api[i+1].Body) != w {
			t.Errorf("%s body %s, want %s", api[i+1].Path, api[i+1].Body, w)
		}
	}

	// Two refused CONNECTs (guest not listening yet), then the handshake.
	if r := kinds(recs, "refused"); len(r) != 2 || r[0].Line != "CONNECT 4242" {
		t.Errorf("refused %+v", r)
	}
	if c := kinds(recs, "connect"); len(c) != 1 || c[0].Line != "CONNECT 4242" {
		t.Errorf("connect %+v", c)
	}
	if !strings.Contains(errs, "+ firecracker API: PUT /actions {\"action_type\":\"InstanceStart\"}") ||
		!strings.Contains(errs, "control channel attached (vsock "+filepath.Join(dir, "vsock.sock")+" port 4242)") {
		t.Errorf("verbose log:\n%s", errs)
	}
	if _, err := os.Stat(dir); !os.IsNotExist(err) {
		t.Errorf("run dir %s not cleaned up", dir)
	}
}

func TestFirecrackerSerialFallback(t *testing.T) {
	setupFakeVMM(t, "serial", fakevmm.EnvCode, "5")
	code, out, errs, err := runVsockVMM(t, target.Firecracker, target.AArch64, Options{})
	if err != nil || code != 5 {
		t.Fatalf("code %d err %v\nstdout:\n%s\nstderr:\n%s", code, err, out, errs)
	}
	if !strings.Contains(out, "booting with cmdline: console=ttyS0 patina.epoch=") {
		t.Fatalf("stdout:\n%s", out)
	}
}

func TestFirecrackerNoExitCode(t *testing.T) {
	setupFakeVMM(t, "silent")
	code, out, errs, err := runVsockVMM(t, target.Firecracker, target.X86_64, Options{})
	if err != nil || code != ExitUnknown || !strings.Contains(errs, "without reporting an exit code (firecracker exited with status 0)") {
		t.Fatalf("code %d err %v\nstdout:\n%s\nstderr:\n%s", code, err, out, errs)
	}
}

func TestFirecrackerAPIError(t *testing.T) {
	setupFakeVMM(t, "exit", fakevmm.EnvFail, "/vsock")
	_, out, errs, err := runVsockVMM(t, target.Firecracker, target.X86_64, Options{Name: "fcfail"})
	if err == nil || !strings.Contains(err.Error(), "PUT /vsock: 400 Bad Request: fake failure injected for /vsock") {
		t.Fatalf("err %v\nstdout:\n%s\nstderr:\n%s", err, out, errs)
	}
	if !strings.Contains(errs, "fake firecracker: logger configured") {
		t.Errorf("firecracker log not replayed on failure:\n%s", errs)
	}
	if _, err := runstate.Load("fcfail"); err == nil {
		t.Error("run state left behind")
	}
}

func TestCloudHypervisorExitEvent(t *testing.T) {
	log := setupFakeVMM(t, "exit", fakevmm.EnvCode, "3")
	code, out, errs, err := runVsockVMM(t, target.CloudHypervisor, target.AArch64, Options{
		Name: "chexit", MemMB: 256, CPUs: 3,
		Net: vmm.NetConfig{Mode: "tap1", IP: "172.16.0.2/24,172.16.0.1"},
	})
	if err != nil || code != 3 {
		t.Fatalf("code %d err %v\nstdout:\n%s\nstderr:\n%s", code, err, out, errs)
	}
	recs := fakeLog(t, log)
	argv := kinds(recs, "argv")
	if len(argv) != 1 {
		t.Fatalf("argv records %+v", argv)
	}
	a := argv[0].Argv
	dir := strings.TrimSuffix(strings.TrimPrefix(a[1], "path="), "/ch.sock")
	cmdline := a[7]
	if !strings.HasPrefix(cmdline, "console=ttyAMA0 ip=172.16.0.2::172.16.0.1:255.255.255.0:patina:eth0:off patina.epoch=") || !strings.HasSuffix(cmdline, " --") {
		t.Fatalf("cmdline %q", cmdline)
	}
	want, err := vmm.CloudHypervisorArgs(&vmm.Config{
		Arch: target.AArch64, Kernel: filepath.Join(dir, "kernel-aarch64.Image"), Initrd: filepath.Join(dir, "initrd.cpio"),
		Cmdline: cmdline, MemMB: 256, CPUs: 3, RunDir: dir, Net: vmm.NetConfig{Mode: "tap1"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(a, "\x00") != strings.Join(want, "\x00") {
		t.Fatalf("argv\n got %q\nwant %q", a, want)
	}
	if api := kinds(recs, "api"); len(api) != 1 || api[0].Method != "GET" || api[0].Path != "/api/v1/vmm.ping" {
		t.Fatalf("API calls %+v", api)
	}
	if c := kinds(recs, "connect"); len(c) != 1 || c[0].Line != "CONNECT 4242" {
		t.Errorf("connect %+v", c)
	}
	if !strings.Contains(errs, "patina: cloud-hypervisor v54.0-fake (fake)") {
		t.Errorf("ping not logged:\n%s", errs)
	}
}

// Ctrl-C becomes {"op":"stop"} over vsock; the guest's exit event carries 143.
func TestCloudHypervisorInterrupt(t *testing.T) {
	log := setupFakeVMM(t, "stop")
	sigs := make(chan os.Signal, 1)
	var stdout syncBuf
	done := make(chan struct{})
	var code int
	var errs string
	var err error
	go func() {
		defer close(done)
		var o Options
		o.Signals, o.Grace = sigs, 5*time.Second
		o.Stdout = &stdout
		code, _, errs, err = runVsockVMM(t, target.CloudHypervisor, target.X86_64, o)
	}()
	waitFor(t, func() bool { return strings.Contains(stdout.String(), "waiting for stop") })
	sigs <- os.Interrupt
	<-done
	if err != nil || code != 143 {
		t.Fatalf("code %d err %v\nstderr:\n%s", code, err, errs)
	}
	ops := kinds(fakeLog(t, log), "op")
	if len(ops) != 1 || ops[0].Line != `{"op":"stop","grace_ms":5000}` {
		t.Fatalf("ops %+v", ops)
	}
}

// `patina ctl <vm>` reaches a Firecracker guest through the run-state proxy.
func TestFirecrackerCtlViaProxy(t *testing.T) {
	setupFakeVMM(t, "stop", fakevmm.EnvRefuse, "1")
	type result struct {
		code int
		errs string
		err  error
	}
	res := make(chan result, 1)
	go func() {
		code, _, errs, err := runVsockVMM(t, target.Firecracker, target.X86_64, Options{Name: "fcctl"})
		res <- result{code, errs, err}
	}()
	var st *runstate.State
	waitFor(t, func() bool {
		s, err := runstate.Load("fcctl")
		st = s
		return err == nil
	})
	if st.VMM != target.Firecracker || st.Upstream.Kind != ctl.KindFirecracker || !strings.HasSuffix(st.Upstream.Path, "/fcctl/vsock.sock") || st.VMMPID == 0 {
		t.Fatalf("state %+v", st)
	}
	c, err := ctl.Dial(st.Control, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	// Until the hub has completed CONNECT 4242 the proxy answers "not connected".
	waitFor(t, func() bool {
		e, err := ctl.DoPing(c, 9, 2*time.Second, nil)
		return err == nil && e.Ev == "pong" && e.ID != nil && *e.ID == 9
	})
	code, err := ctl.DoStop(c, 3*time.Second, 10*time.Second, nil)
	if err != nil || code != 143 {
		t.Fatalf("ctl stop: code %d err %v", code, err)
	}
	r := <-res
	if r.err != nil || r.code != 143 {
		t.Fatalf("run: code %d err %v\nstderr:\n%s", r.code, r.err, r.errs)
	}
}

// A guest whose control channel never comes up is killed after --timeout.
func TestCloudHypervisorTimeoutMuteGuest(t *testing.T) {
	setupFakeVMM(t, "mute")
	start := time.Now()
	code, out, errs, err := runVsockVMM(t, target.CloudHypervisor, target.X86_64, Options{Timeout: time.Second, Grace: 200 * time.Millisecond})
	if err != nil || code != ExitTimeout || !strings.Contains(errs, "killing VM") {
		t.Fatalf("code %d err %v\nstdout:\n%s\nstderr:\n%s", code, err, out, errs)
	}
	if d := time.Since(start); d > 15*time.Second {
		t.Fatalf("took %v", d)
	}
}

func waitFor(t *testing.T, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatal("condition not reached within 15s")
		}
		time.Sleep(20 * time.Millisecond)
	}
}
