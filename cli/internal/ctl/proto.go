// Package ctl speaks the patina control protocol (design §10): NDJSON over
// a stream (virtio-vsock port 4242, or the patina.ctl virtio-console port).
package ctl

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"
)

// Port is the guest vsock port the kernel listens on.
const Port = 4242

// Op is a host -> guest message. Field order (op first) is stable.
type Op struct {
	Op      string  `json:"op"`
	ID      *uint64 `json:"id,omitempty"`
	Sig     string  `json:"sig,omitempty"`
	GraceMs *int64  `json:"grace_ms,omitempty"`
	EpochNs int64   `json:"epoch_ns,omitempty"`
}

// Event is a guest -> host message. Unknown fields are ignored.
type Event struct {
	Ev       string  `json:"ev"`
	ID       *uint64 `json:"id,omitempty"`
	Code     *int    `json:"code,omitempty"`
	Signal   string  `json:"signal,omitempty"`
	Forced   bool    `json:"forced,omitempty"`
	Msg      string  `json:"msg,omitempty"`
	Level    string  `json:"level,omitempty"`
	Go       string  `json:"go,omitempty"`
	CPUs     int     `json:"cpus,omitempty"`
	MemMB    int     `json:"mem_mb,omitempty"`
	UptimeMs int64   `json:"uptime_ms,omitempty"`

	Raw string `json:"-"` // the line as received (without newline)
}

// ParseEvent decodes one line. A line that is not a JSON object yields an
// error; callers typically report and skip it.
func ParseEvent(line string) (*Event, error) {
	line = strings.TrimRight(line, "\r\n")
	e := &Event{}
	if err := json.Unmarshal([]byte(line), e); err != nil {
		return nil, fmt.Errorf("control: malformed event %q: %w", line, err)
	}
	e.Raw = line
	return e, nil
}

// Encode renders an op as one NDJSON line including the trailing newline.
func (o Op) Encode() []byte {
	b, err := json.Marshal(o)
	if err != nil {
		panic(err) // only plain fields; cannot fail
	}
	return append(b, '\n')
}

// Ping builds {"op":"ping","id":N}.
func Ping(id uint64) Op { return Op{Op: "ping", ID: &id} }

// Stop builds {"op":"stop","grace_ms":N}.
func Stop(grace time.Duration) Op {
	ms := grace.Milliseconds()
	if ms < 0 {
		ms = 0
	}
	return Op{Op: "stop", GraceMs: &ms}
}

// Dump builds {"op":"dump"}.
func Dump() Op { return Op{Op: "dump"} }

// Time builds {"op":"time","epoch_ns":N}.
func Time(t time.Time) Op { return Op{Op: "time", EpochNs: t.UnixNano()} }

// InjectableSignals are the signal names the host may send (design §6).
var InjectableSignals = []string{"SIGTERM", "SIGINT", "SIGHUP", "SIGUSR1", "SIGUSR2", "SIGQUIT"}

// NormalizeSignal accepts "hup", "SIGHUP", "sighup" and returns "SIGHUP".
func NormalizeSignal(s string) (string, error) {
	u := strings.ToUpper(s)
	if !strings.HasPrefix(u, "SIG") {
		u = "SIG" + u
	}
	for _, k := range InjectableSignals {
		if u == k {
			return u, nil
		}
	}
	return "", fmt.Errorf("signal %q cannot be injected (allowed: %s)", s, strings.Join(InjectableSignals, " "))
}

// Signal builds {"op":"signal","sig":NAME}; name must be normalized.
func Signal(name string) Op { return Op{Op: "signal", Sig: name} }

// Describe renders an event for humans.
func (e *Event) Describe() string {
	switch e.Ev {
	case "ready":
		return fmt.Sprintf("ready: %s, %d cpus, %d MiB", e.Go, e.CPUs, e.MemMB)
	case "exit":
		s := "exit"
		if e.Code != nil {
			s += fmt.Sprintf(" code %d", *e.Code)
		}
		if e.Signal != "" {
			s += " (" + e.Signal + ")"
		}
		if e.Forced {
			s += " (forced)"
		}
		return s
	case "pong":
		return fmt.Sprintf("pong: uptime %s", time.Duration(e.UptimeMs)*time.Millisecond)
	case "panic":
		return "KERNEL PANIC: " + e.Msg
	case "log":
		return fmt.Sprintf("kernel %s: %s", e.Level, e.Msg)
	case "error":
		return "error: " + e.Msg
	}
	return e.Raw
}
