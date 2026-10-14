// Package cmdline builds the kernel command line that carries argv, the
// environment and boot parameters into a patina guest (design §12).
//
// Format:
//
//	console=<dev> [ip=...] [patina.strace=1] patina.epoch=<unix-seconds> [patina.env=K=V,K=V] -- arg1 arg2
//
// Tokens are separated by single spaces. Everything after the first " -- "
// token is argv[1:] (argv[0] is always "/app", set by the kernel); "--" is
// emitted even when there are no arguments.
//
// Escaping: env keys, env values and argv elements are percent-encoded. The
// encoder escapes every byte <= 0x20 or >= 0x7f and each of  % , = " ' \ .
// The kernel decoder is simply: "%XX" (two hex digits) -> that byte,
// everything else literal. patina.env is split on raw ',' into items and each
// item on the first raw '=' into key and value before decoding.
//
// Because '=' is always escaped in argv, a raw token after "--" of the form
// "virtio_mmio.device=..." cannot have come from the CLI: QEMU's microvm
// machine appends such tokens to the end of the command line when it runs
// with acpi=off (verified on QEMU 11.1: "... -- x virtio_mmio.device=512@0xfeb00e00:12"),
// as Firecracker may too, and the kernel must treat them as kernel
// parameters, not argv. The CLI currently leaves microvm ACPI on (the
// default), in which case nothing is appended.
package cmdline

import (
	"errors"
	"fmt"
	"strconv"
	"strings"
)

// MaxLen is the longest command line the CLI will produce (Linux
// COMMAND_LINE_SIZE on both arches; leaves the VMM no room to append, which
// QEMU microvm does, so the kernel should accept somewhat more).
const MaxLen = 2048

// Options are the inputs to Build.
type Options struct {
	Console string   // value for console=, e.g. "ttyAMA0"; omitted if empty
	Params  []string // extra raw kernel params (ip=..., patina.strace=1); not escaped
	Epoch   int64    // wall-clock seconds for patina.epoch=; omitted if 0
	Env     []string // "K=V" pairs
	Args    []string // argv[1:]
}

func needsEscape(c byte) bool {
	return c <= 0x20 || c >= 0x7f || strings.IndexByte(`%,="'\`, c) >= 0
}

// Escape percent-encodes s per the package rules.
func Escape(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		if needsEscape(c) {
			fmt.Fprintf(&b, "%%%02X", c)
		} else {
			b.WriteByte(c)
		}
	}
	return b.String()
}

// Unescape reverses Escape (reference implementation of the kernel decoder).
func Unescape(s string) (string, error) {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		if s[i] != '%' {
			b.WriteByte(s[i])
			continue
		}
		if i+2 >= len(s) {
			return "", fmt.Errorf("truncated escape in %q", s)
		}
		v, err := strconv.ParseUint(s[i+1:i+3], 16, 8)
		if err != nil {
			return "", fmt.Errorf("bad escape %q in %q", s[i:i+3], s)
		}
		b.WriteByte(byte(v))
		i += 2
	}
	return b.String(), nil
}

// Build renders the command line.
func Build(o Options) (string, error) {
	var toks []string
	if o.Console != "" {
		toks = append(toks, "console="+o.Console)
	}
	for _, p := range o.Params {
		if p == "" || strings.ContainsAny(p, " \t\n") || p == "--" {
			return "", fmt.Errorf("invalid kernel parameter %q", p)
		}
		toks = append(toks, p)
	}
	if o.Epoch != 0 {
		toks = append(toks, "patina.epoch="+strconv.FormatInt(o.Epoch, 10))
	}
	if len(o.Env) > 0 {
		items := make([]string, 0, len(o.Env))
		for _, kv := range o.Env {
			k, v, ok := strings.Cut(kv, "=")
			if !ok || k == "" {
				return "", fmt.Errorf("invalid environment entry %q: want KEY=VALUE", kv)
			}
			items = append(items, Escape(k)+"="+Escape(v))
		}
		toks = append(toks, "patina.env="+strings.Join(items, ","))
	}
	toks = append(toks, "--")
	for _, a := range o.Args {
		if a == "" {
			return "", errors.New("empty argument cannot be represented on the kernel command line")
		}
		toks = append(toks, Escape(a))
	}
	s := strings.Join(toks, " ")
	if len(s) > MaxLen {
		return "", fmt.Errorf("kernel command line is %d bytes, limit %d (too many/long args or env)", len(s), MaxLen)
	}
	return s, nil
}

// Parsed is the result of Parse.
type Parsed struct {
	Params map[string]string // kernel params before "--" (and VMM-appended ones after it)
	Env    []string          // decoded "K=V"
	Args   []string          // decoded argv[1:]
}

// Parse is a reference decoder mirroring what the kernel must do. It exists
// so the encoding is tested end to end and documented by executable code.
func Parse(s string) (*Parsed, error) {
	p := &Parsed{Params: map[string]string{}}
	toks := strings.Fields(s)
	inArgs := false
	for _, t := range toks {
		if !inArgs {
			if t == "--" {
				inArgs = true
				continue
			}
			k, v, _ := strings.Cut(t, "=")
			p.Params[k] = v
			continue
		}
		if strings.HasPrefix(t, "virtio_mmio.device=") {
			p.Params["virtio_mmio.device"] = strings.TrimPrefix(t, "virtio_mmio.device=") // VMM-appended
			continue
		}
		a, err := Unescape(t)
		if err != nil {
			return nil, err
		}
		p.Args = append(p.Args, a)
	}
	if env, ok := p.Params["patina.env"]; ok && env != "" {
		for _, item := range strings.Split(env, ",") {
			k, v, _ := strings.Cut(item, "=")
			dk, err := Unescape(k)
			if err != nil {
				return nil, err
			}
			dv, err := Unescape(v)
			if err != nil {
				return nil, err
			}
			p.Env = append(p.Env, dk+"="+dv)
		}
	}
	return p, nil
}
