package main

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/kenkeiter/patina/cli/internal/cpio"
	"github.com/kenkeiter/patina/cli/internal/image"
	"github.com/kenkeiter/patina/cli/internal/target"
	"github.com/kenkeiter/patina/cli/internal/vmm/fakevmm"
)

func TestMain(m *testing.M) {
	fakevmm.MaybeRun()
	os.Exit(m.Run())
}

// TestRunVsockVMMsCLI drives `patina run --vmm firecracker|cloud-hypervisor`
// through the command line (flags --vmm-bin, --net, --ip) against the fake
// VMM binaries.
func TestRunVsockVMMsCLI(t *testing.T) {
	binDir, log := fakevmm.Install(t)
	t.Setenv("PATH", "/usr/bin:/bin") // only --vmm-bin can find the fakes
	t.Setenv(fakevmm.EnvScenario, "exit")
	t.Setenv(fakevmm.EnvCode, "42")
	rd, err := os.MkdirTemp("/tmp", "pcli")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(rd)
	t.Setenv("XDG_RUNTIME_DIR", rd)

	d := t.TempDir()
	kp := filepath.Join(d, "kernel")
	os.WriteFile(kp, []byte("fake kernel"), 0o644)
	b := cpio.NewBuilder()
	b.AddBytes("/app", 0o755, 0, []byte("not an ELF"))
	ip := filepath.Join(d, "initrd.cpio")
	f, _ := os.Create(ip)
	b.WriteTo(f)
	f.Close()
	img := filepath.Join(d, "fake.patina")
	m := &image.Manifest{Name: "fake", Arch: target.X86_64, VMM: target.Firecracker, Target: "firecracker-x86_64", GoVersion: "go0", Created: time.Now()}
	if err := image.Write(img, m, kp, ip); err != nil {
		t.Fatal(err)
	}

	for _, v := range []string{fakevmm.Firecracker, fakevmm.CloudHypervisor} {
		os.Remove(log)
		var so, se bytes.Buffer
		code := dispatch([]string{"run", img, "--vmm", v, "--vmm-bin", filepath.Join(binDir, v),
			"--net", "tap0", "--ip", "10.9.0.2/16,10.9.0.1", "--cpus", "1", "--mem", "64", "--timeout", "30s", "--", "x"}, &so, &se)
		if code != 42 {
			t.Fatalf("%s: exit %d\nstdout:\n%s\nstderr:\n%s", v, code, so.String(), se.String())
		}
		recs, err := fakevmm.ReadLog(log)
		if err != nil {
			t.Fatal(err)
		}
		var cmdline string
		for _, r := range recs {
			switch {
			case r.Kind == "api" && r.Path == "/boot-source":
				var bs struct {
					BootArgs string `json:"boot_args"`
				}
				json.Unmarshal(r.Body, &bs)
				cmdline = bs.BootArgs
			case r.Kind == "argv" && v == fakevmm.CloudHypervisor:
				for i, a := range r.Argv {
					if a == "--cmdline" {
						cmdline = r.Argv[i+1]
					}
				}
			}
		}
		if !strings.HasPrefix(cmdline, "console=ttyS0 ip=10.9.0.2::10.9.0.1:255.255.0.0:patina:eth0:off patina.epoch=") || !strings.HasSuffix(cmdline, " -- x") {
			t.Errorf("%s: cmdline %q", v, cmdline)
		}
	}

	// --net user is QEMU-only.
	var se bytes.Buffer
	if code := dispatch([]string{"run", img, "--vmm", "firecracker", "--vmm-bin", filepath.Join(binDir, "firecracker"), "--net", "user"}, &bytes.Buffer{}, &se); code == 0 ||
		!strings.Contains(se.String(), "QEMU-only") {
		t.Fatalf("--net user on firecracker: exit %d: %s", code, se.String())
	}
}
