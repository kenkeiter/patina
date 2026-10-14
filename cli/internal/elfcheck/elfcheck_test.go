package elfcheck

import (
	"debug/elf"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

const prog = "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(\"hi\") }\n"

// buildTiny builds a tiny GOOS=linux program and returns the binary path.
func buildTiny(t *testing.T, goarch string, extra ...string) string {
	t.Helper()
	if testing.Short() {
		t.Skip("builds Go binaries")
	}
	goBin, err := exec.LookPath("go")
	if err != nil {
		t.Skip("go tool not found")
	}
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "go.mod"), []byte("module tiny\n\ngo 1.24\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "main.go"), []byte(prog), 0o644); err != nil {
		t.Fatal(err)
	}
	out := filepath.Join(dir, "tiny")
	args := append([]string{"build", "-trimpath", "-o", out}, extra...)
	args = append(args, ".")
	cmd := exec.Command(goBin, args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), "GOOS=linux", "GOARCH="+goarch, "CGO_ENABLED=0", "GOFLAGS=")
	if b, err := cmd.CombinedOutput(); err != nil {
		t.Skipf("cannot build %v: %v\n%s", extra, err, b)
	}
	return out
}

func TestStaticAccepted(t *testing.T) {
	for _, c := range []struct {
		goarch string
		m      elf.Machine
	}{{"arm64", elf.EM_AARCH64}, {"amd64", elf.EM_X86_64}} {
		bin := buildTiny(t, c.goarch, "-ldflags=-extldflags=-static")
		info, err := Check(bin, c.m)
		if err != nil {
			t.Fatalf("%s: static binary rejected: %v", c.goarch, err)
		}
		if !strings.HasPrefix(info.GoVersion, "go1.") || info.Path != "tiny" {
			t.Fatalf("%s: bad build info %+v", c.goarch, info)
		}
	}
}

func TestWrongArchRejected(t *testing.T) {
	bin := buildTiny(t, "amd64")
	if _, err := Check(bin, elf.EM_AARCH64); err == nil || !strings.Contains(err.Error(), "machine") {
		t.Fatalf("amd64 binary accepted for aarch64: %v", err)
	}
}

func TestPIERejected(t *testing.T) {
	bin := buildTiny(t, "arm64", "-buildmode=pie")
	f, err := elf.Open(bin)
	if err != nil {
		t.Fatal(err)
	}
	if f.Type != elf.ET_DYN {
		t.Skipf("toolchain produced %v for -buildmode=pie", f.Type)
	}
	f.Close()
	_, err = Check(bin, elf.EM_AARCH64)
	if err == nil || !strings.Contains(err.Error(), "PIE") {
		t.Fatalf("PIE binary not rejected as PIE: %v", err)
	}
}

// TestInterpAndDynamicRejected retypes a static binary's PT_NOTE program
// header as PT_INTERP / PT_DYNAMIC to exercise those checks independently of
// ET_DYN.
func TestInterpAndDynamicRejected(t *testing.T) {
	bin := buildTiny(t, "arm64")
	orig, err := os.ReadFile(bin)
	if err != nil {
		t.Fatal(err)
	}
	f, err := elf.Open(bin)
	if err != nil {
		t.Fatal(err)
	}
	// Find a PT_NOTE (harmless to retype) program header index.
	idx := -1
	for i, p := range f.Progs {
		if p.Type == elf.PT_NOTE {
			idx = i
			break
		}
	}
	f.Close()
	if idx < 0 {
		t.Skip("no PT_NOTE to repurpose")
	}
	// ELF64 header: e_phoff at 0x20 (8 bytes LE), e_phentsize at 0x36.
	le := func(b []byte) uint64 {
		var v uint64
		for i := len(b) - 1; i >= 0; i-- {
			v = v<<8 | uint64(b[i])
		}
		return v
	}
	phoff := le(orig[0x20:0x28])
	phentsize := le(orig[0x36:0x38])
	for _, c := range []struct {
		typ  elf.ProgType
		want string
	}{{elf.PT_INTERP, "PT_INTERP"}, {elf.PT_DYNAMIC, "PT_DYNAMIC"}} {
		mod := append([]byte(nil), orig...)
		off := phoff + uint64(idx)*phentsize
		mod[off], mod[off+1], mod[off+2], mod[off+3] = byte(c.typ), byte(c.typ>>8), byte(c.typ>>16), byte(c.typ>>24)
		p := filepath.Join(t.TempDir(), "patched")
		if err := os.WriteFile(p, mod, 0o755); err != nil {
			t.Fatal(err)
		}
		_, err := Check(p, elf.EM_AARCH64)
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%v not rejected: %v", c.typ, err)
		}
	}
}

func TestNonELFRejected(t *testing.T) {
	p := filepath.Join(t.TempDir(), "x")
	os.WriteFile(p, []byte("#!/bin/sh\necho hi\n"), 0o755)
	if _, err := Check(p, elf.EM_AARCH64); err == nil {
		t.Fatal("shell script accepted")
	}
}
