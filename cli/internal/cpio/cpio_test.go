package cpio

import (
	"bytes"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

// minimalEntry/parseMinimal are a deliberately independent newc parser so the
// round-trip test does not just check the Reader against the Writer's bugs.
type minimalEntry struct {
	name string
	mode uint32
	data string
}

func parseMinimal(t *testing.T, b []byte) []minimalEntry {
	t.Helper()
	var out []minimalEntry
	off := 0
	align := func(n int) int { return (n + 3) &^ 3 }
	for {
		if off+110 > len(b) {
			t.Fatalf("truncated header at %d", off)
		}
		h := b[off : off+110]
		if string(h[:6]) != "070701" {
			t.Fatalf("bad magic at %d: %q", off, h[:6])
		}
		field := func(i int) uint32 {
			v, err := strconv.ParseUint(string(h[6+8*i:14+8*i]), 16, 32)
			if err != nil {
				t.Fatal(err)
			}
			return uint32(v)
		}
		mode, size, nsz := field(1), int(field(6)), int(field(11))
		if field(12) != 0 {
			t.Fatalf("check field nonzero")
		}
		name := string(b[off+110 : off+110+nsz-1])
		if b[off+110+nsz-1] != 0 {
			t.Fatalf("name not NUL terminated")
		}
		dataOff := align(off + 110 + nsz)
		if (off+110+nsz)%4 != 0 {
			for _, z := range b[off+110+nsz : dataOff] {
				if z != 0 {
					t.Fatalf("nonzero padding")
				}
			}
		}
		data := string(b[dataOff : dataOff+size])
		off = align(dataOff + size)
		if name == "TRAILER!!!" {
			if off != len(b) {
				t.Fatalf("trailing garbage: archive %d bytes, trailer ends at %d", len(b), off)
			}
			return out
		}
		out = append(out, minimalEntry{name, mode, data})
	}
}

func buildSample(t *testing.T) []byte {
	t.Helper()
	b := NewBuilder()
	src := t.TempDir()
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	must(os.MkdirAll(filepath.Join(src, "static", "css"), 0o755))
	must(os.WriteFile(filepath.Join(src, "static", "index.html"), []byte("<h1>hi</h1>\n"), 0o644))
	must(os.WriteFile(filepath.Join(src, "static", "css", "a.css"), []byte("x"), 0o600))
	must(os.Symlink("index.html", filepath.Join(src, "static", "home.html")))
	must(os.WriteFile(filepath.Join(src, "hosts"), []byte("127.0.0.1 localhost\n"), 0o644))

	must(b.AddBytes("/app", 0o755, 1700000000, []byte("\x7fELF-fake-binary")))
	must(b.AddTree(filepath.Join(src, "static"), "/srv/www"))
	must(b.AddTree(filepath.Join(src, "hosts"), "/etc/hosts"))
	must(b.AddBytes("/odd/len", 0o644, 0, []byte("abc")))
	var buf bytes.Buffer
	if _, err := b.WriteTo(&buf); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

func TestRoundTripMinimalParser(t *testing.T) {
	ents := parseMinimal(t, buildSample(t))
	got := map[string]minimalEntry{}
	var order []string
	for _, e := range ents {
		got[e.name] = e
		order = append(order, e.name)
	}
	want := []struct {
		name string
		mode uint32
		data string
	}{
		{"app", TypeReg | 0o755, "\x7fELF-fake-binary"},
		{"etc", TypeDir | 0o755, ""},
		{"etc/hosts", TypeReg | 0o644, "127.0.0.1 localhost\n"},
		{"odd", TypeDir | 0o755, ""},
		{"odd/len", TypeReg | 0o644, "abc"},
		{"srv", TypeDir | 0o755, ""},
		{"srv/www", TypeDir | 0o755, ""},
		{"srv/www/css", TypeDir | 0o755, ""},
		{"srv/www/css/a.css", TypeReg | 0o600, "x"},
		{"srv/www/home.html", TypeSymlink | 0o777, "index.html"},
		{"srv/www/index.html", TypeReg | 0o644, "<h1>hi</h1>\n"},
	}
	if len(ents) != len(want) {
		t.Fatalf("got entries %v, want %d", order, len(want))
	}
	for _, w := range want {
		e, ok := got[w.name]
		if !ok {
			t.Errorf("missing %s", w.name)
			continue
		}
		if e.mode != w.mode || e.data != w.data {
			t.Errorf("%s: mode %o data %q, want %o %q", w.name, e.mode, e.data, w.mode, w.data)
		}
	}
	// Parents before children.
	seen := map[string]bool{}
	for _, n := range order {
		if i := strings.LastIndexByte(n, '/'); i >= 0 && !seen[n[:i]] {
			t.Errorf("%s appears before its parent", n)
		}
		seen[n] = true
	}
}

func TestReaderMatchesWriter(t *testing.T) {
	r := NewReader(bytes.NewReader(buildSample(t)))
	n := 0
	for {
		h, err := r.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
		data, err := io.ReadAll(r)
		if err != nil {
			t.Fatal(err)
		}
		if int64(len(data)) != h.Size {
			t.Fatalf("%s: read %d bytes, header says %d", h.Name, len(data), h.Size)
		}
		if h.Name == "app" && (h.Mode != TypeReg|0o755 || h.MTime != 1700000000) {
			t.Fatalf("app header %+v", h)
		}
		n++
	}
	if n != 11 {
		t.Fatalf("read %d entries, want 11", n)
	}
}

func TestDuplicateAndConflict(t *testing.T) {
	b := NewBuilder()
	if err := b.AddBytes("/a/b", 0o644, 0, nil); err != nil {
		t.Fatal(err)
	}
	if err := b.AddBytes("a/b", 0o644, 0, nil); err == nil {
		t.Fatal("duplicate file accepted")
	}
	if err := b.AddBytes("/a/b/c", 0o644, 0, nil); err == nil {
		t.Fatal("file under a file accepted")
	}
	if err := b.AddDir("/a", 0o700, 5); err != nil {
		t.Fatalf("explicit dir over implicit: %v", err)
	}
	if b.entries["a"].hdr.Mode != TypeDir|0o700 {
		t.Fatalf("explicit dir mode not applied")
	}
	if _, err := CleanPath("/"); err == nil {
		t.Fatal("root accepted")
	}
	if p, _ := CleanPath("/../../etc//x/"); p != "etc/x" {
		t.Fatalf("CleanPath escape: %q", p)
	}
}

// TestSystemCpioReadsArchive cross-checks against the host cpio(1) if any.
func TestSystemCpioReadsArchive(t *testing.T) {
	cpioBin, err := exec.LookPath("cpio")
	if err != nil {
		t.Skip("no cpio(1) on host")
	}
	cmd := exec.Command(cpioBin, "-it")
	cmd.Stdin = bytes.NewReader(buildSample(t))
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("cpio -it: %v\n%s", err, out)
	}
	for _, n := range []string{"app", "etc/hosts", "srv/www/css/a.css", "srv/www/home.html"} {
		if !strings.Contains(string(out), n) {
			t.Errorf("cpio -it listing lacks %s:\n%s", n, out)
		}
	}
}
