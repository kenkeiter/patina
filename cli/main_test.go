package main

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParseArgsInterspersed(t *testing.T) {
	fs := newFlagSet("x", &bytes.Buffer{}, "x")
	tgt := fs.String("target", "", "")
	var files listFlag
	fs.Var(&files, "files", "")
	pos, pass, err := parseArgs(fs, []string{"./pkg", "--target", "qemu-aarch64", "--files", "a:/a", "extra", "--files=b:/b", "--", "-x", "--", "y"})
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(pos, ",") != "./pkg,extra" || *tgt != "qemu-aarch64" || strings.Join(files, ",") != "a:/a,b:/b" {
		t.Fatalf("pos %v target %q files %v", pos, *tgt, files)
	}
	if strings.Join(pass, " ") != "-x -- y" {
		t.Fatalf("passthrough %q", pass)
	}
	fs2 := newFlagSet("y", &bytes.Buffer{}, "y")
	if _, pass, _ := parseArgs(fs2, []string{"img", "--"}); pass == nil || len(pass) != 0 {
		t.Fatalf("empty passthrough %#v", pass)
	}
}

// TestBuildAndInspectHello builds the real testapps/hello for both arches
// with a dummy kernel and inspects the bundle.
func TestBuildAndInspectHello(t *testing.T) {
	if testing.Short() {
		t.Skip("runs go build")
	}
	pkg, err := filepath.Abs("../testapps/hello")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(pkg); err != nil {
		t.Skip("testapps not present")
	}
	d := t.TempDir()
	kernel := filepath.Join(d, "dummy-kernel")
	os.WriteFile(kernel, []byte("not a kernel"), 0o644)
	static := filepath.Join(d, "static")
	os.MkdirAll(filepath.Join(static, "css"), 0o755)
	os.WriteFile(filepath.Join(static, "index.html"), []byte("<h1>hi</h1>"), 0o644)
	os.WriteFile(filepath.Join(static, "css", "a.css"), []byte("body{}"), 0o644)

	for _, tgt := range []string{"qemu-aarch64", "firecracker-x86_64"} {
		out := filepath.Join(d, tgt+".patina")
		var so, se bytes.Buffer
		code := dispatch([]string{"build", pkg, "--target", tgt, "--kernel", kernel, "--files", static + ":/srv/www", "-o", out}, &so, &se)
		if code != 0 {
			t.Fatalf("%s: build exit %d\n%s%s", tgt, code, so.String(), se.String())
		}
		so.Reset()
		se.Reset()
		if code := dispatch([]string{"image", "inspect", out}, &so, &se); code != 0 {
			t.Fatalf("inspect exit %d: %s", code, se.String())
		}
		s := so.String()
		for _, want := range []string{"name:", "hello", "target:", tgt, "go1.", "-rwxr-xr-x", "/app",
			"/srv/www/css/a.css", "/srv/www/index.html", "drwxr-xr-x", "/srv\n"} {
			if !strings.Contains(s, want) {
				t.Errorf("%s: inspect output lacks %q:\n%s", tgt, want, s)
			}
		}
	}

	// Wrong-arch kernel name is fine (dummy), but an unknown target is a usage error.
	var se bytes.Buffer
	if code := dispatch([]string{"build", pkg, "--target", "vz-x86_64", "--kernel", kernel, "-o", filepath.Join(d, "x")}, &bytes.Buffer{}, &se); code != exitUsage {
		t.Fatalf("vz-x86_64 exit %d: %s", code, se.String())
	}
}

// TestImagePack packs a raw kernel + initrd (scripts/run-qemu.sh's VZ path)
// and reads the bundle back.
func TestImagePack(t *testing.T) {
	d := t.TempDir()
	kernel := filepath.Join(d, "k")
	initrd := filepath.Join(d, "i.cpio")
	os.WriteFile(kernel, []byte("not a kernel"), 0o644)
	os.WriteFile(initrd, []byte("not a cpio"), 0o644)
	out := filepath.Join(d, "x.patina")
	var so, se bytes.Buffer
	if code := dispatch([]string{"image", "pack", "--kernel", kernel, "--initrd", initrd, "--target", "vz-aarch64", "-o", out}, &so, &se); code != 0 {
		t.Fatalf("pack exit %d\n%s%s", code, so.String(), se.String())
	}
	so.Reset()
	if code := dispatch([]string{"image", "inspect", out, "--json"}, &so, &se); code != 0 {
		t.Fatalf("inspect exit %d: %s", code, se.String())
	}
	if !strings.Contains(so.String(), `"target": "vz-aarch64"`) || !strings.Contains(so.String(), `"name": "x"`) {
		t.Fatalf("manifest:\n%s", so.String())
	}
	if code := dispatch([]string{"image", "pack", "--kernel", kernel, "-o", out}, &so, &se); code != exitUsage {
		t.Fatalf("missing flags: exit %d, want %d", code, exitUsage)
	}
}
