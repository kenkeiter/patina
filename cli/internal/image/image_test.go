package image

import (
	"archive/tar"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func writeSample(t *testing.T) (string, *Manifest) {
	t.Helper()
	d := t.TempDir()
	k := filepath.Join(d, "k")
	i := filepath.Join(d, "i")
	os.WriteFile(k, []byte("kernel bytes"), 0o644)
	os.WriteFile(i, []byte("initrd bytes"), 0o644)
	m := &Manifest{Name: "hello", Arch: "aarch64", VMM: "qemu", Target: "qemu-aarch64", GoVersion: "go1.27.1",
		SHA256: map[string]string{"app": strings.Repeat("a", 64)}, Created: time.Unix(1760000000, 0).UTC()}
	out := filepath.Join(d, "x.patina")
	if err := Write(out, m, k, i); err != nil {
		t.Fatal(err)
	}
	return out, m
}

func TestWriteOpenExtract(t *testing.T) {
	out, m := writeSample(t)
	if m.Kernel != "kernel-aarch64.Image" || m.Initrd != "initrd.cpio" || m.Format != 1 {
		t.Fatalf("manifest %+v", m)
	}
	if sum := sha256.Sum256([]byte("kernel bytes")); m.SHA256["kernel"] != hex.EncodeToString(sum[:]) {
		t.Fatalf("kernel hash %s", m.SHA256["kernel"])
	}
	b, err := Open(out)
	if err != nil {
		t.Fatal(err)
	}
	if b.Manifest.Name != "hello" || b.Manifest.GoVersion != "go1.27.1" || !b.Manifest.Created.Equal(m.Created) {
		t.Fatalf("round trip %+v", b.Manifest)
	}
	dst := filepath.Join(t.TempDir(), "k")
	if err := b.Extract(b.Manifest.Kernel, dst, "kernel"); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(dst); string(got) != "kernel bytes" {
		t.Fatalf("extracted %q", got)
	}
	// Tar layout: manifest first, then initrd, then kernel.
	f, _ := os.Open(out)
	defer f.Close()
	tr := tar.NewReader(f)
	var names []string
	for {
		h, err := tr.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
		names = append(names, h.Name)
		if h.Name == ManifestName {
			var raw map[string]any
			json.NewDecoder(tr).Decode(&raw)
			for _, k := range []string{"name", "arch", "vmm", "target", "go_version", "kernel", "initrd", "sha256", "created"} {
				if _, ok := raw[k]; !ok {
					t.Errorf("manifest.json lacks %q", k)
				}
			}
		}
	}
	if strings.Join(names, ",") != "manifest.json,initrd.cpio,kernel-aarch64.Image" {
		t.Fatalf("members %v", names)
	}
}

func TestExtractDetectsTamper(t *testing.T) {
	out, _ := writeSample(t)
	b, err := Open(out)
	if err != nil {
		t.Fatal(err)
	}
	b.Manifest.SHA256["initrd"] = strings.Repeat("0", 64)
	if err := b.Extract(InitrdName, filepath.Join(t.TempDir(), "i"), "initrd"); err == nil || !strings.Contains(err.Error(), "mismatch") {
		t.Fatalf("got %v", err)
	}
}

func TestValidate(t *testing.T) {
	good := Manifest{Format: 1, Arch: "aarch64", VMM: "vz", Target: "vz-aarch64", Kernel: "k", Initrd: "i",
		SHA256: map[string]string{"kernel": strings.Repeat("a", 64), "initrd": strings.Repeat("b", 64)}}
	if err := good.Validate(); err != nil {
		t.Fatal(err)
	}
	mut := []func(*Manifest){
		func(m *Manifest) { m.Format = 2 },
		func(m *Manifest) { m.Arch = "x86_64"; m.Target = "vz-x86_64" },
		func(m *Manifest) { m.Target = "qemu-aarch64" },
		func(m *Manifest) { m.Kernel = "../k" },
		func(m *Manifest) { m.Initrd = m.Kernel },
		func(m *Manifest) { m.SHA256 = map[string]string{} },
	}
	for i, f := range mut {
		m := good
		m.SHA256 = map[string]string{"kernel": good.SHA256["kernel"], "initrd": good.SHA256["initrd"]}
		f(&m)
		if err := m.Validate(); err == nil {
			t.Errorf("mutation %d accepted", i)
		}
	}
	p := filepath.Join(t.TempDir(), "notpatina")
	os.WriteFile(p, []byte("hello"), 0o644)
	if _, err := Open(p); err == nil {
		t.Error("garbage opened")
	}
}
