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
	"strings"
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
	ExitPanic   = 255 // kernel panic (the panic event, design §10.2)
)

// Options configure Run.
type Options struct {
	Image   string
	VMM     string // default: the image's target VMM
	VMMBin  string // VMM binary override (default: looked up in PATH)
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
	// CLI messages share the terminal with the guest console: write them
	// between console lines (noteWriter), never inside one.
	sentinel := &sentinelWriter{w: o.Stdout}
	o.Stderr = &noteWriter{s: sentinel, w: o.Stderr}
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

	cfg := &vmm.Config{
		Arch: m.Arch, Kernel: kernel, Initrd: initrd,
		MemMB: o.MemMB, CPUs: o.CPUs, RunDir: dir, Accel: o.Accel, Net: o.Net, Bin: o.VMMBin,
		Serial: sentinel, Stderr: o.Stderr, Log: logw,
	}
	params, err := drv.KernelParams(cfg)
	if err != nil {
		return 0, err
	}
	if o.Strace {
		params = append(params, "patina.strace=1")
	}
	// PATINA_PARAMS: extra kernel parameters (space-separated), for tests and
	// experiments (e.g. patina.pci.irq=intx). Not part of the normal CLI surface.
	if extra := os.Getenv("PATINA_PARAMS"); extra != "" {
		params = append(params, strings.Fields(extra)...)
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
		evMu     sync.Mutex
		exitEv   *ctl.Event
		panicked bool
		gotExit  = make(chan struct{})
		hub      *ctl.Hub
		hubSet   = make(chan struct{}) // closed once hub is assigned
	)
	ts, ok := drv.(vmm.TimeSyncer)
	timeSync := ok && ts.NeedsTimeSync()
	hub, err = ctl.NewHub(filepath.Join(dir, runstate.ProxySocket), mach.Control(), func(e *ctl.Event) {
		switch e.Ev {
		case "ready":
			// No RTC (Apple VZ): step the guest clock from patina.epoch's
			// one-second resolution to the host's (design §3.3).
			if timeSync {
				go func() {
					<-hubSet
					if err := hub.Send(ctl.Time(time.Now())); err != nil {
						fmt.Fprintf(o.Stderr, "patina: warning: cannot sync guest clock: %v\n", err)
					}
				}()
			}
		case "exit":
			evMu.Lock()
			if exitEv == nil {
				exitEv = e
				close(gotExit)
			}
			evMu.Unlock()
		case "panic":
			evMu.Lock()
			panicked = true
			evMu.Unlock()
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
	close(hubSet)
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
			fmt.Fprintf(o.Stderr, "patina: %s; cannot send stop (%v), killing VM\n", why, err)
			forced = true
			mach.Kill()
			return
		}
		fmt.Fprintf(o.Stderr, "patina: %s; stopping (grace %s, interrupt again to kill)\n", why, o.Grace)
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
	ev, kpanic := exitEv, panicked
	evMu.Unlock()
	// Precedence (doc/cli-contract.md): --timeout, then the exit event, a
	// panic event, the serial sentinel, the VMM's own carrier
	// (isa-debug-exit), and finally a kill by the CLI.
	switch {
	case timedOut:
		// Like timeout(1): 124 whatever the guest did with the stop.
		return ExitTimeout, nil
	case ev != nil && ev.Code != nil:
		return *ev.Code, nil
	case kpanic:
		return ExitPanic, nil
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
	mid  bool // the console is in the middle of a line
	code *int
}

const maxSentinelLine = 256

func (s *sentinelWriter) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	n, err := s.w.Write(p)
	if len(p) > 0 {
		s.mid = p[len(p)-1] != '\n'
	}
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

// noteWriter writes CLI messages (one Write per message) to w so that they
// never land inside a guest console line: it holds the console's lock while
// writing and starts a new line first if the console is mid-line.
type noteWriter struct {
	s *sentinelWriter
	w io.Writer
}

func (n *noteWriter) Write(p []byte) (int, error) {
	n.s.mu.Lock()
	defer n.s.mu.Unlock()
	if n.s.mid {
		n.w.Write([]byte("\n"))
		n.s.mid = false
	}
	return n.w.Write(p)
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
