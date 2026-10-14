package vmm

import (
	"strings"
	"testing"

	"github.com/kenkeiter/patina/cli/internal/target"
)

// The QEMU argv is a contract with the kernel (console device, control port
// name, debug-exit port); these goldens make changes deliberate.
func TestQEMUArgsGolden(t *testing.T) {
	cfg := &Config{Arch: target.AArch64, Kernel: "/r/k", Initrd: "/r/i", Cmdline: "console=ttyAMA0 --", MemMB: 512, CPUs: 2, RunDir: "/r"}
	got, err := QEMUArgs(cfg, "hvf")
	if err != nil {
		t.Fatal(err)
	}
	want := "-M virt,gic-version=3 -accel hvf -cpu host -smp 2 -m 512M -nodefaults -no-user-config -display none -no-reboot " +
		"-kernel /r/k -initrd /r/i -append console=ttyAMA0 -- " +
		"-chardev stdio,id=con0,signal=off -serial chardev:con0 " +
		"-device virtio-serial-device,id=vser0 -chardev socket,id=ctl0,path=/r/qemu-ctl.sock,server=on,wait=on " +
		"-device virtserialport,bus=vser0.0,chardev=ctl0,name=patina.ctl -device virtio-rng-device"
	if s := strings.Join(got, " "); s != want {
		t.Fatalf("aarch64 argv\n got %s\nwant %s", s, want)
	}

	cfg.Arch = target.X86_64
	cfg.Net = NetConfig{Mode: "user", Publish: []string{"8080:8080"}}
	got, err = QEMUArgs(cfg, "tcg")
	if err != nil {
		t.Fatal(err)
	}
	s := strings.Join(got, " ")
	for _, frag := range []string{
		"-M microvm -accel tcg -cpu max",
		"-device isa-debug-exit,iobase=0xf4,iosize=0x04",
		"-netdev user,id=net0,hostfwd=tcp:127.0.0.1:8080-:8080 -device virtio-net-device,netdev=net0",
	} {
		if !strings.Contains(s, frag) {
			t.Errorf("x86_64 argv lacks %q:\n%s", frag, s)
		}
	}
	p, _ := (&QEMU{}).KernelParams(cfg)
	if len(p) != 1 || p[0] != "ip=10.0.2.15::10.0.2.2:255.255.255.0:patina:eth0:off:10.0.2.3" {
		t.Errorf("user-net params %v", p)
	}
}

func TestQEMUStderrFilter(t *testing.T) {
	var out strings.Builder
	f := &lineFilter{w: &out, drop: qemuBenign}
	in := "qemu-system-aarch64: -chardev socket,id=ctl0,path=/r/qemu-ctl.sock,server=on,wait=on: info: QEMU waiting for connection on: disconnected:unix:/r/qemu-ctl.sock,server=on\n" +
		"qemu-system-aarch64: real error\nunterminated"
	for i := 0; i < len(in); i += 5 {
		f.Write([]byte(in[i:min(i+5, len(in))]))
	}
	f.flush()
	if got := out.String(); got != "qemu-system-aarch64: real error\nunterminated" {
		t.Fatalf("got %q", got)
	}
}

func TestQEMUArgsErrors(t *testing.T) {
	base := Config{Arch: target.AArch64, MemMB: 64, CPUs: 1, RunDir: "/r"}
	bad := []Config{
		func() Config { c := base; c.MemMB = 0; return c }(),
		func() Config { c := base; c.Arch = "riscv64"; return c }(),
		func() Config { c := base; c.Net.Publish = []string{"1:2"}; return c }(),
		func() Config { c := base; c.Net = NetConfig{Mode: "user", Publish: []string{"x:2"}}; return c }(),
	}
	for i, c := range bad {
		if _, err := QEMUArgs(&c, "tcg"); err == nil {
			t.Errorf("case %d accepted", i)
		}
	}
}

func TestDriversRegistered(t *testing.T) {
	for _, n := range target.VMMs {
		d, err := Get(n)
		if err != nil {
			t.Fatal(err)
		}
		if n == target.VZ {
			if _, err := d.Start(nil, &Config{}); err == nil || !strings.Contains(err.Error(), "not implemented") {
				t.Errorf("%s: %v", n, err)
			}
		}
	}
	if _, err := Get("xen"); err == nil {
		t.Error("unknown VMM accepted")
	}
}
