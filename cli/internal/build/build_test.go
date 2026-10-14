package build

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParseFileMapping(t *testing.T) {
	ok := map[string]FileMapping{
		"./static:/static":      {"./static", "/static"},
		"a:b/c:/srv/x//":        {"a:b/c", "/srv/x"},
		"/abs/file:/etc/hosts":  {"/abs/file", "/etc/hosts"},
		"rel:/usr/share/../lib": {"rel", "/usr/lib"},
	}
	for in, want := range ok {
		got, err := ParseFileMapping(in)
		if err != nil || got != want {
			t.Errorf("%s: %+v %v", in, got, err)
		}
	}
	for _, bad := range []string{"nocolon", "src:relative", ":/x", "src:/", "src:/app", "src:/app/x", "src:/proc/x", "src:/dev", "src:/sys/kernel"} {
		if _, err := ParseFileMapping(bad); err == nil {
			t.Errorf("%q accepted", bad)
		}
	}
}

func TestFindKernel(t *testing.T) {
	d := t.TempDir()
	k := filepath.Join(d, "kernel-x86_64-pvh.elf")
	os.WriteFile(k, []byte("k"), 0o644)
	t.Setenv("PATINA_KERNEL_DIR", d)
	if p, err := FindKernel("x86_64", ""); err != nil || p != k {
		t.Fatalf("env dir: %q %v", p, err)
	}
	if p, err := FindKernel("x86_64", k); err != nil || p != k {
		t.Fatalf("explicit: %q %v", p, err)
	}
	if _, err := FindKernel("x86_64", filepath.Join(d, "missing")); err == nil {
		t.Fatal("missing explicit kernel accepted")
	}
	t.Setenv("PATINA_KERNEL_DIR", t.TempDir())
	_, err := FindKernel("aarch64", "")
	if err != nil && !strings.Contains(err.Error(), "not implemented yet") {
		t.Fatalf("error should mention that fetching is not implemented: %v", err)
	}
}
