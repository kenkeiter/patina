package cmdline

import (
	"reflect"
	"strings"
	"testing"
)

func TestBuildExact(t *testing.T) {
	got, err := Build(Options{
		Console: "ttyAMA0",
		Params:  []string{"patina.strace=1"},
		Epoch:   1760000000,
		Env:     []string{"GODEBUG=gctrace=1,madvdontneed=1", "GREETING=hello world", "EMPTY="},
		Args:    []string{"-port", "8080", "a b", "x=y", "100%", "tab\there", "ünï"},
	})
	if err != nil {
		t.Fatal(err)
	}
	want := "console=ttyAMA0 patina.strace=1 patina.epoch=1760000000 " +
		"patina.env=GODEBUG=gctrace%3D1%2Cmadvdontneed%3D1,GREETING=hello%20world,EMPTY= " +
		"-- -port 8080 a%20b x%3Dy 100%25 tab%09here %C3%BCn%C3%AF"
	if got != want {
		t.Fatalf("got  %q\nwant %q", got, want)
	}
}

func TestNoArgsStillHasSeparator(t *testing.T) {
	got, err := Build(Options{Console: "ttyS0"})
	if err != nil {
		t.Fatal(err)
	}
	if got != "console=ttyS0 --" {
		t.Fatalf("got %q", got)
	}
}

func TestRoundTrip(t *testing.T) {
	env := []string{"A=1", "PATH=/bin:/usr/bin", "Q=\"quoted\" 'single' \\back", "COMMA=a,b,c"}
	args := []string{"--flag=value", "has space", "%41", "--", "\x01\x7f", "日本"}
	s, err := Build(Options{Console: "ttyS0", Epoch: 1, Env: env, Args: args})
	if err != nil {
		t.Fatal(err)
	}
	// Simulate QEMU microvm appending device params after "--".
	s += " virtio_mmio.device=512@0xfeb00e00:12"
	p, err := Parse(s)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(p.Env, env) {
		t.Errorf("env %q, want %q", p.Env, env)
	}
	if !reflect.DeepEqual(p.Args, args) {
		t.Errorf("args %q, want %q", p.Args, args)
	}
	if p.Params["console"] != "ttyS0" || p.Params["patina.epoch"] != "1" || p.Params["virtio_mmio.device"] == "" {
		t.Errorf("params %v", p.Params)
	}
	// The first raw "--" token is the separator; a literal "--" argument
	// after it stays an argument (checked by the DeepEqual above).
	if i := strings.Index(s, " -- "); i < strings.Index(s, "patina.env=") {
		t.Errorf("separator misplaced in %q", s)
	}
}

func TestRejects(t *testing.T) {
	if _, err := Build(Options{Args: []string{""}}); err == nil {
		t.Error("empty arg accepted")
	}
	if _, err := Build(Options{Env: []string{"NOEQUALS"}}); err == nil {
		t.Error("env without = accepted")
	}
	if _, err := Build(Options{Env: []string{"=v"}}); err == nil {
		t.Error("env with empty key accepted")
	}
	if _, err := Build(Options{Args: []string{strings.Repeat("x", MaxLen)}}); err == nil {
		t.Error("overlong cmdline accepted")
	}
	if _, err := Build(Options{Params: []string{"a b"}}); err == nil {
		t.Error("param with space accepted")
	}
}

func TestUnescapeErrors(t *testing.T) {
	for _, s := range []string{"%", "%4", "%zz"} {
		if _, err := Unescape(s); err == nil {
			t.Errorf("Unescape(%q) accepted", s)
		}
	}
}
