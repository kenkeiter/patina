// Package run implements `patina run`: boot a .patina image on a VMM, stream
// its console, proxy its control channel, translate Ctrl-C into a graceful
// stop, and exit with the guest's exit code.
package run

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"sync"
	"time"

	"github.com/kenkeiter/patina/cli/internal/cmdline"
	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/image"
	"github.com/kenkeiter/patina/cli/internal/runstate"
	"github.com/kenkeiter/patina/cli/internal/vmm"
)

// Exit codes the CLI itself produces.
const (
	ExitTimeout = 124 // --timeout expired (like timeout(1))
	ExitKilled  = 137 // guest did not stop within the grace period; VMM killed
	ExitUnknown = 1   // guest ended without reporting a code
)

// Options configure Run.
type Options struct {
	Image   string
	VMM     string // default: the image's target VMM
	Name    string // run-state name; default <image name>-<pid>
	MemMB   int
	CPUs    int
	Strace  bool
	Timeout time.Duration // 0 = none
	Grace   time.Duration // stop grace for Ctrl-C / timeout
	Env     []string
	Args    []string
	Accel   string
	Net     vmm.NetConfig
	Verbose bool

	Stdout  io.Writer        // guest console
	Stderr  io.Writer        // CLI + VMM diagnostics
	Signals <-chan os.Signal // interrupts (Ctrl-C); nil = none
}

// Run boots the image and returns the process exit code.
func Run(o Options) (int, error) {
	if o.Stdout == nil {
		o.Stdout = os.Stdout
	}
	if o.Stderr == nil {
		o.Stderr = os.Stderr
	}
	if o.Grace <= 0 {
		o.Grace = 10 * time.Second
	}
	logw := io.Discard
	if o.Verbose {
		logw = o.Stderr
	}
	b, err := image.Open(o.Image)
	if err != nil {
		return 0, err
	}
	m := b.Manifest
	vmmName := o.VMM
	if vmmName == "" {
		vmmName = m.VMM
	}
	drv, err := vmm.Get(vmmName)
	if err != nil {
		return 0, err
	}
	if vmmName != m.VMM {
		fmt.Fprintf(logw, "patina: image was built for %s, running on %s (same %s kernel)\n", m.VMM, vmmName, m.Arch)
	}

	name := o.Name
	if name == "" {
		name = fmt.Sprintf("%s-%d", m.Name, os.Getpid())
	}
	dir, err := runstate.Create(name)
	if err != nil {
		return 0, err
	}
	defer os.RemoveAll(dir)

	kernel := filepath.Join(dir, m.Kernel)
	initrd := filepath.Join(dir, m.Initrd)
	if err := b.Extract(m.Kernel, kernel, "kernel"); err != nil {
		return 0, err
	}
	if err := b.Extract(m.Initrd, initrd, "initrd"); err != nil {
		return 0, err
	}

	sentinel := &sentinelWriter{w: o.Stdout}
	cfg := &vmm.Config{
		Arch: m.Arch, Kernel: kernel, Initrd: initrd,
		MemMB: o.MemMB, CPUs: o.CPUs, RunDir: dir, Accel: o.Accel, Net: o.Net,
		Serial: sentinel, Stderr: o.Stderr, Log: logw,
	}
	params, err := drv.KernelParams(cfg)
	if err != nil {
		return 0, err
	}
	if o.Strace {
		params = append(params, "patina.strace=1")
	}
	cfg.Cmdline, err = cmdline.Build(cmdline.Options{
		Console: drv.Console(m.Arch),
		Params:  params,
		Epoch:   time.Now().Unix(),
		Env:     o.Env,
		Args:    o.Args,
	})
	if err != nil {
		return 0, err
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	mach, err := drv.Start(ctx, cfg)
	if err != nil {
		return 0, err
	}

	var (
		evMu    sync.Mutex
		exitEv  *ctl.Event
		gotExit = make(chan struct{})
	)
	hub, err := ctl.NewHub(filepath.Join(dir, runstate.ProxySocket), mach.Control(), func(e *ctl.Event) {
		switch e.Ev {
		case "exit":
			evMu.Lock()
			if exitEv == nil {
				exitEv = e
				close(gotExit)
			}
			evMu.Unlock()
		case "panic":
			fmt.Fprintf(o.Stderr, "patina: %s\n", e.Describe())
			return
		}
		fmt.Fprintf(logw, "patina: control: %s\n", e.Describe())
	})
	if err != nil {
		mach.Kill()
		mach.Wait()
		return 0, err
	}
	defer hub.Close()
	go func() {
		<-hub.Connected()
		fmt.Fprintf(logw, "patina: control channel attached (%s)\n", mach.Control())
	}()

	st := &runstate.State{
		Name: name, PID: os.Getpid(), VMMPID: mach.PID(), VMM: vmmName, Arch: m.Arch,
		Image:    o.Image,
		Control:  ctl.Endpoint{Kind: ctl.KindStream, Path: filepath.Join(dir, runstate.ProxySocket)},
		Upstream: mach.Control(),
		Started:  time.Now().UTC(),
	}
	if err := runstate.Save(dir, st); err != nil {
		fmt.Fprintf(o.Stderr, "patina: warning: cannot write run state: %v\n", err)
	}
	fmt.Fprintf(logw, "patina: vm %q (patina ctl %s ...)\n", name, name)

	type waitRes struct {
		ex  *vmm.Exit
		err error
	}
	waitCh := make(chan waitRes, 1)
	go func() {
		ex, err := mach.Wait()
		waitCh <- waitRes{ex, err}
	}()

	var timeoutC <-chan time.Time
	if o.Timeout > 0 {
		t := time.NewTimer(o.Timeout)
		defer t.Stop()
		timeoutC = t.C
	}
	var killC <-chan time.Time
	stopping, timedOut, forced := false, false, false
	beginStop := func(why string) {
		if stopping {
			return
		}
		stopping = true
		if err := hub.Send(ctl.Stop(o.Grace)); err != nil {
			fmt.Fprintf(o.Stderr, "\npatina: %s; cannot send stop (%v), killing VM\n", why, err)
			forced = true
			mach.Kill()
			return
		}
		fmt.Fprintf(o.Stderr, "\npatina: %s; stopping (grace %s, interrupt again to kill)\n", why, o.Grace)
		// The guest enforces the grace itself; allow slack before assuming it
		// never received the request.
		killC = time.After(o.Grace + 2*time.Second)
	}

	var res waitRes
loop:
	for {
		select {
		case res = <-waitCh:
			break loop
		case <-o.Signals:
			if stopping {
				fmt.Fprintf(o.Stderr, "patina: killing VM\n")
				forced = true
				mach.Kill()
				continue
			}
			beginStop("interrupted")
		case <-timeoutC:
			timedOut = true
			beginStop(fmt.Sprintf("timeout after %s", o.Timeout))
		case <-killC:
			fmt.Fprintf(o.Stderr, "patina: guest did not exit within the grace period, killing VM\n")
			forced = true
			mach.Kill()
		}
	}
	if res.err != nil {
		return 0, res.err
	}
	// The exit event precedes poweroff, but is read asynchronously.
	select {
	case <-gotExit:
	case <-time.After(300 * time.Millisecond):
	}

	evMu.Lock()
	ev := exitEv
	evMu.Unlock()
	switch {
	case timedOut:
		return ExitTimeout, nil
	case ev != nil && ev.Code != nil:
		return *ev.Code, nil
	case sentinel.Code() != nil:
		return *sentinel.Code(), nil
	case res.ex.GuestCode != nil:
		return *res.ex.GuestCode, nil
	case forced:
		return ExitKilled, nil
	}
	fmt.Fprintf(o.Stderr, "patina: guest ended without reporting an exit code (%s exited with status %d)\n", vmmName, res.ex.Code)
	return ExitUnknown, nil
}

// sentinelRE matches the kernel's serial exit line (design §5).
var sentinelRE = regexp.MustCompile(`^\[patina\] exit code (-?[0-9]+)\s*$`)

// sentinelWriter passes console output through and remembers the last
// "[patina] exit code N" line.
type sentinelWriter struct {
	w    io.Writer
	mu   sync.Mutex
	line []byte
	over bool
	code *int
}

const maxSentinelLine = 256

func (s *sentinelWriter) Write(p []byte) (int, error) {
	n, err := s.w.Write(p)
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, c := range p {
		if c == '\n' {
			if !s.over {
				if m := sentinelRE.FindSubmatch(trimCR(s.line)); m != nil {
					if v, err := strconv.Atoi(string(m[1])); err == nil {
						s.code = &v
					}
				}
			}
			s.line, s.over = s.line[:0], false
			continue
		}
		if len(s.line) >= maxSentinelLine {
			s.over = true
			continue
		}
		s.line = append(s.line, c)
	}
	return n, err
}

func trimCR(b []byte) []byte {
	for len(b) > 0 && b[len(b)-1] == '\r' {
		b = b[:len(b)-1]
	}
	return b
}

// Code returns the last sentinel exit code seen, if any.
func (s *sentinelWriter) Code() *int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.code
}

// ErrNoImage is returned when no image path is given.
var ErrNoImage = errors.New("usage: patina run <image.patina> [flags] [-- args...]")
