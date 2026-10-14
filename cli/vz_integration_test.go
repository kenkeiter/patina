//go:build darwin && arm64

package main

import (
	"bytes"
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// TestVZBootHello boots the real kernel (out/kernel-aarch64.Image) with
// testapps/hello under Apple Virtualization.framework through an
// entitlement-signed `patina` binary (cli/scripts/build.sh), and checks the
// VM starts and stops by itself, cleanly, within the timeout.
//
// Until the kernel drives virtio-console (Phase 3) the guest is silent under
// VZ and reports no exit code, so `patina run` exits 1. Set
// PATINA_VZ_CONSOLE=1 to also require the console output and exit code 0.
func TestVZBootHello(t *testing.T) {
	if testing.Short() {
		t.Skip("boots a VM")
	}
	root, _ := filepath.Abs("..")
	kernel := filepath.Join(root, "out", "kernel-aarch64.Image")
	if _, err := os.Stat(kernel); err != nil {
		t.Skip("no kernel: run scripts/build-kernel.sh aarch64")
	}
	if _, err := exec.LookPath("codesign"); err != nil {
		t.Skip("codesign not available")
	}
	d := t.TempDir()
	bin := filepath.Join(d, "patina")
	if out, err := exec.Command("scripts/build.sh", "-o", bin).CombinedOutput(); err != nil {
		t.Fatalf("scripts/build.sh: %v\n%s", err, out)
	}
	img := filepath.Join(d, "hello.patina")
	build := exec.Command(bin, "build", "./testapps/hello", "--target", "vz-aarch64", "--kernel", kernel, "-o", img)
	build.Dir = root
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("patina build: %v\n%s", err, out)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	var so, se bytes.Buffer
	run := exec.CommandContext(ctx, bin, "run", "-v", "--timeout", "30s", "--grace", "2s", img, "--", "7")
	run.Stdout, run.Stderr = &so, &se
	t0 := time.Now()
	err := run.Run()
	elapsed := time.Since(t0)
	code := run.ProcessState.ExitCode()
	t.Logf("patina run exit %d after %s\nstdout:\n%s\nstderr:\n%s", code, elapsed.Round(time.Millisecond), so.String(), se.String())
	if ctx.Err() != nil {
		t.Fatalf("patina run hung: %v", err)
	}
	if !strings.Contains(se.String(), "guest stopped the VM") {
		t.Fatalf("VM did not stop by itself (exit %d)", code)
	}
	if strings.Contains(se.String(), "patina: vz: ") && !strings.Contains(se.String(), "patina: vz: guest stopped") {
		t.Fatalf("VZ reported an error")
	}
	if os.Getenv("PATINA_VZ_CONSOLE") != "" {
		if !strings.Contains(so.String(), "PASS hello (exit 7)") || code != 7 {
			t.Fatalf("want hello output and exit 7, got exit %d", code)
		}
		return
	}
	switch code {
	case 7, 1: // 1: guest gave no exit code (no console / vsock driver yet)
	default:
		t.Fatalf("unexpected exit code %d", code)
	}
}
