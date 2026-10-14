package vmm

import (
	"context"
	"runtime"
	"strings"
	"testing"

	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/target"
)

func TestVZRegistered(t *testing.T) {
	d, err := Get(target.VZ)
	if err != nil {
		t.Fatal(err)
	}
	if d.Console(target.AArch64) != "hvc0" {
		t.Fatalf("console %q", d.Console(target.AArch64))
	}
	if ts, ok := d.(TimeSyncer); !ok || !ts.NeedsTimeSync() {
		t.Fatal("vz must ask for a time sync after ready")
	}
	if ts, ok := Driver(&QEMU{}).(TimeSyncer); ok && ts.NeedsTimeSync() {
		t.Fatal("qemu has an RTC; no time sync")
	}
}

func TestVZSpec(t *testing.T) {
	base := Config{Arch: target.AArch64, Kernel: "/k", Initrd: "/i", Cmdline: "console=hvc0 --", MemMB: 512, CPUs: 2}
	s, err := VZSpecFor(&base)
	if err != nil {
		t.Fatal(err)
	}
	if s.MemBytes != 512<<20 || s.CPUs != 2 || s.NAT || s.VsockPort != ctl.Port || s.Cmdline != base.Cmdline {
		t.Fatalf("spec %+v", s)
	}
	p, err := (&VZ{}).KernelParams(&base)
	if err != nil || len(p) != 0 {
		t.Fatalf("no-net params %v %v", p, err)
	}

	nat := base
	nat.Net = NetConfig{Mode: "user"}
	if s, err := VZSpecFor(&nat); err != nil || !s.NAT {
		t.Fatalf("--net user: %+v %v", s, err)
	}
	if p, err := (&VZ{}).KernelParams(&nat); err != nil || strings.Join(p, " ") != "ip=dhcp" {
		t.Fatalf("--net user params %v %v", p, err)
	}

	for name, mut := range map[string]func(c *Config){
		"x86":     func(c *Config) { c.Arch = target.X86_64 },
		"mem":     func(c *Config) { c.MemMB = 0 },
		"cpus":    func(c *Config) { c.CPUs = 0 },
		"accel":   func(c *Config) { c.Accel = "tcg" },
		"tap":     func(c *Config) { c.Net.Mode = "tap0" },
		"publish": func(c *Config) { c.Net = NetConfig{Mode: "user", Publish: []string{"80:80"}} },
	} {
		c := base
		mut(&c)
		if _, err := VZSpecFor(&c); err == nil {
			t.Errorf("%s: accepted", name)
		}
	}
}

func TestVZUnsupportedPlatform(t *testing.T) {
	if runtime.GOOS == "darwin" && runtime.GOARCH == "arm64" {
		t.Skip("supported here")
	}
	_, err := vzStart(context.Background(), &Config{}, &VZSpec{})
	if err == nil || !strings.Contains(err.Error(), "unsupported on this platform") {
		t.Fatalf("got %v", err)
	}
}
