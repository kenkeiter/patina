package vmm

import (
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/kenkeiter/patina/cli/internal/target"
)

// fakeAPI is an in-process REST server on a Unix socket that records every
// request, standing in for Firecracker's --api-sock / Cloud Hypervisor's
// --api-socket.
type fakeAPI struct {
	mu   sync.Mutex
	reqs []string
	ct   []string
	fail map[string]string // path -> fault_message (400)
}

func startFakeAPI(t *testing.T) (*fakeAPI, string) {
	t.Helper()
	d, err := os.MkdirTemp("/tmp", "fapi")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(d) })
	sock := filepath.Join(d, "api.sock")
	ln, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	f := &fakeAPI{fail: map[string]string{}}
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		f.mu.Lock()
		line := r.Method + " " + r.URL.Path
		if len(b) > 0 {
			line += " " + string(b)
		}
		f.reqs = append(f.reqs, line)
		f.ct = append(f.ct, r.Header.Get("Content-Type")+"|"+r.Header.Get("Accept"))
		msg, fail := f.fail[r.URL.Path]
		f.mu.Unlock()
		if fail {
			w.WriteHeader(http.StatusBadRequest)
			json.NewEncoder(w).Encode(map[string]string{"fault_message": msg})
			return
		}
		w.WriteHeader(http.StatusNoContent)
	})}
	go srv.Serve(ln)
	t.Cleanup(func() { srv.Close() })
	return f, sock
}

func (f *fakeAPI) requests() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string(nil), f.reqs...)
}

// The Firecracker API sequence is a contract with firecracker.yaml; the
// golden makes changes deliberate.
func TestFirecrackerRequestsGolden(t *testing.T) {
	cfg := &Config{Arch: target.X86_64, Kernel: "/r/kernel-x86_64-pvh.elf", Initrd: "/r/initrd.cpio",
		Cmdline: "console=ttyS0 ip=dhcp patina.epoch=1 -- a%20b", MemMB: 256, CPUs: 2, RunDir: "/r",
		Net: NetConfig{Mode: "tap0"}}
	reqs, err := FirecrackerRequests(cfg)
	if err != nil {
		t.Fatal(err)
	}
	want := []string{
		`PUT /boot-source {"kernel_image_path":"/r/kernel-x86_64-pvh.elf","initrd_path":"/r/initrd.cpio","boot_args":"console=ttyS0 ip=dhcp patina.epoch=1 -- a%20b"}`,
		`PUT /machine-config {"vcpu_count":2,"mem_size_mib":256}`,
		`PUT /vsock {"guest_cid":3,"uds_path":"/r/vsock.sock"}`,
		`PUT /network-interfaces/eth0 {"iface_id":"eth0","host_dev_name":"tap0","guest_mac":"` + GuestMAC("/r") + `"}`,
		`PUT /entropy {}`,
		`PUT /actions {"action_type":"InstanceStart"}`,
	}
	var got []string
	for _, r := range reqs {
		got = append(got, r.String())
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("requests\n got:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if a := strings.Join(FirecrackerArgs(cfg), " "); a != "--api-sock /r/fc.sock --log-path /r/fc.log --level Warning" {
		t.Fatalf("argv %s", a)
	}

	// Without networking there is no network-interfaces call.
	cfg.Net = NetConfig{}
	reqs, _ = FirecrackerRequests(cfg)
	for _, r := range reqs {
		if strings.HasPrefix(r.Path, "/network-interfaces") {
			t.Fatal("network interface configured without --net")
		}
	}

	// The same bytes go over the wire, with JSON headers, in order.
	f, sock := startFakeAPI(t)
	c := newAPIClient(sock)
	defer c.close()
	for _, r := range reqs {
		if _, err := c.do(r); err != nil {
			t.Fatal(err)
		}
	}
	gotWire := f.requests()
	if len(gotWire) != len(reqs) {
		t.Fatalf("server saw %d requests, want %d", len(gotWire), len(reqs))
	}
	for i, r := range reqs {
		if gotWire[i] != r.String() {
			t.Errorf("wire request %d: %s, want %s", i, gotWire[i], r)
		}
		if f.ct[i] != "application/json|application/json" {
			t.Errorf("request %d headers %q", i, f.ct[i])
		}
	}
}

func TestFirecrackerValidation(t *testing.T) {
	base := Config{Arch: target.AArch64, Kernel: "k", MemMB: 128, CPUs: 1, RunDir: "/r"}
	for i, mod := range []func(*Config){
		func(c *Config) { c.CPUs = 3 },
		func(c *Config) { c.CPUs = 34 },
		func(c *Config) { c.MemMB = 0 },
		func(c *Config) { c.Arch = "riscv64" },
		func(c *Config) { c.Net.Mode = "user" },
		func(c *Config) { c.Net = NetConfig{Mode: "tap0", Publish: []string{"1:2"}} },
		func(c *Config) { c.Cmdline = strings.Repeat("x", FirecrackerMaxCmdline+1) },
	} {
		c := base
		mod(&c)
		if _, err := FirecrackerRequests(&c); err == nil {
			t.Errorf("case %d accepted", i)
		}
	}
}

func TestAPIClientFault(t *testing.T) {
	f, sock := startFakeAPI(t)
	f.fail["/vsock"] = "Invalid uds_path"
	c := newAPIClient(sock)
	defer c.close()
	_, err := c.do(APIRequest{"PUT", "/vsock", fcVsock{GuestCID: 3, UDSPath: "/x"}})
	if err == nil || !strings.Contains(err.Error(), "PUT /vsock: 400 Bad Request: Invalid uds_path") {
		t.Fatalf("err %v", err)
	}
}

func TestCloudHypervisorArgsGolden(t *testing.T) {
	cfg := &Config{Arch: target.AArch64, Kernel: "/r/kernel-aarch64.Image", Initrd: "/r/initrd.cpio",
		Cmdline: "console=ttyAMA0 patina.epoch=1 -- x", MemMB: 512, CPUs: 3, RunDir: "/r"}
	a, err := CloudHypervisorArgs(cfg)
	if err != nil {
		t.Fatal(err)
	}
	want := []string{
		"--api-socket", "path=/r/ch.sock",
		"--kernel", "/r/kernel-aarch64.Image",
		"--initramfs", "/r/initrd.cpio",
		"--cmdline", "console=ttyAMA0 patina.epoch=1 -- x",
		"--cpus", "boot=3",
		"--memory", "size=512M",
		"--serial", "tty",
		"--console", "off",
		"--vsock", "cid=3,socket=/r/vsock.sock",
		"--rng", "src=/dev/urandom",
	}
	if strings.Join(a, "\x00") != strings.Join(want, "\x00") {
		t.Fatalf("argv\n got %q\nwant %q", a, want)
	}
	cfg.Net = NetConfig{Mode: "tap7"}
	a, _ = CloudHypervisorArgs(cfg)
	if s := strings.Join(a, " "); !strings.HasSuffix(s, " --net tap=tap7,mac="+GuestMAC("/r")) {
		t.Fatalf("net argv: %s", s)
	}
	for i, mod := range []func(*Config){
		func(c *Config) { c.Net.Mode = "user" },
		func(c *Config) { c.Net.Mode = "tap,x" },
		func(c *Config) { c.RunDir = "/a,b" },
		func(c *Config) { c.CPUs = 0 },
		func(c *Config) { c.Arch = "mips" },
	} {
		c := *cfg
		mod(&c)
		if _, err := CloudHypervisorArgs(&c); err == nil {
			t.Errorf("case %d accepted", i)
		}
	}
}

func TestConsoles(t *testing.T) {
	for _, tc := range []struct {
		d          Driver
		arch, want string
	}{
		{&Firecracker{}, target.X86_64, "ttyS0"},
		{&Firecracker{}, target.AArch64, "ttyS0"},
		{&CloudHypervisor{}, target.X86_64, "ttyS0"},
		{&CloudHypervisor{}, target.AArch64, "ttyAMA0"},
	} {
		if got := tc.d.Console(tc.arch); got != tc.want {
			t.Errorf("%s %s: %s, want %s", tc.d.Name(), tc.arch, got, tc.want)
		}
	}
}

func TestTapKernelParams(t *testing.T) {
	for _, tc := range []struct {
		net  NetConfig
		want string // "" = no params, "!" = error
	}{
		{NetConfig{}, ""},
		{NetConfig{Mode: "none"}, ""},
		{NetConfig{IP: "10.0.0.2/24"}, "!"},
		{NetConfig{Mode: "tap0"}, "ip=dhcp"},
		{NetConfig{Mode: "tap0", IP: "172.16.0.2/24"}, "ip=172.16.0.2:::255.255.255.0:patina:eth0:off"},
		{NetConfig{Mode: "tap0", IP: "172.16.0.2/30,172.16.0.1,1.1.1.1"}, "ip=172.16.0.2::172.16.0.1:255.255.255.252:patina:eth0:off:1.1.1.1"},
		{NetConfig{Mode: "tap0", IP: "10.1.2.3::10.1.2.1:255.255.0.0:h:eth0:off"}, "ip=10.1.2.3::10.1.2.1:255.255.0.0:h:eth0:off"},
		{NetConfig{Mode: "tap0", IP: "dhcp"}, "ip=dhcp"},
		{NetConfig{Mode: "tap0", IP: "10.0.0.2"}, "!"},
		{NetConfig{Mode: "tap0", IP: "10.0.0.2/24,gw"}, "!"},
		{NetConfig{Mode: "tap0", IP: "fd00::2/64"}, "!"},
		{NetConfig{Mode: "tap0", IP: "a b:c"}, "!"},
	} {
		for _, d := range []Driver{&Firecracker{}, &CloudHypervisor{}, &QEMU{}} {
			p, err := d.KernelParams(&Config{Net: tc.net})
			got := strings.Join(p, " ")
			if err != nil {
				got = "!"
			}
			if got != tc.want {
				t.Errorf("%s %+v: %q (err %v), want %q", d.Name(), tc.net, got, err, tc.want)
			}
		}
	}
	if _, err := (&Firecracker{}).KernelParams(&Config{Net: NetConfig{Mode: "user"}}); err == nil {
		t.Error("firecracker accepted --net user")
	}
	if _, err := (&QEMU{}).KernelParams(&Config{Net: NetConfig{Mode: "user", IP: "1.2.3.4/8"}}); err == nil {
		t.Error("qemu accepted --ip with --net user")
	}
}

func TestGuestMAC(t *testing.T) {
	a, b := GuestMAC("/run/a"), GuestMAC("/run/b")
	if a == b || a != GuestMAC("/run/a") {
		t.Fatalf("not stable/distinct: %s %s", a, b)
	}
	hw, err := net.ParseMAC(a)
	if err != nil || hw[0]&1 != 0 || hw[0]&2 == 0 {
		t.Fatalf("%s: want a locally administered unicast MAC (%v)", a, err)
	}
}

// A VMM binary that is not on PATH gives an actionable error.
func TestVMMBinaryMissing(t *testing.T) {
	t.Setenv("PATH", t.TempDir())
	cfg := &Config{Arch: target.X86_64, Kernel: "k", Initrd: "i", MemMB: 64, CPUs: 1, RunDir: t.TempDir()}
	for _, d := range []Driver{&Firecracker{}, &CloudHypervisor{}} {
		_, err := d.Start(context.Background(), cfg)
		if err == nil || !strings.Contains(err.Error(), "not found in PATH") || !strings.Contains(err.Error(), "--vmm-bin") {
			t.Errorf("%s: %v", d.Name(), err)
		}
	}
	cfg.Bin = "/nonexistent/fc"
	if _, err := (&Firecracker{}).Start(context.Background(), cfg); err == nil || !strings.Contains(err.Error(), "--vmm-bin") {
		t.Errorf("--vmm-bin: %v", err)
	}
}
