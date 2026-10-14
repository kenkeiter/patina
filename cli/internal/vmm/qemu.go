package vmm

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/target"
)

// QEMU drives qemu-system-{aarch64,x86_64} with direct kernel boot:
//
//	aarch64: -M virt,gic-version=3, PL011 console (ttyAMA0), hvf/kvm with -cpu host, else TCG -cpu max
//	x86_64:  -M microvm, 16550 console (ttyS0), kvm with -cpu host, else TCG -cpu max,
//	         plus isa-debug-exit at port 0xf4 as an exit-code fallback
//
// The control channel uses the console-port fallback on every host for now
// (QEMU has no vsock on macOS): a virtio-serial-device (virtio-mmio) with one
// virtserialport named "patina.ctl", backed by a Unix socket chardev in
// server mode that `patina run` connects to.
type QEMU struct{}

// Name implements Driver.
func (*QEMU) Name() string { return target.QEMU }

// Console implements Driver.
func (*QEMU) Console(arch string) string {
	if arch == target.AArch64 {
		return "ttyAMA0"
	}
	return "ttyS0"
}

// QEMU user-mode network constants (slirp defaults).
const (
	userGuestIP = "10.0.2.15"
	userGateway = "10.0.2.2"
	userDNS     = "10.0.2.3"
)

// KernelParams implements Driver.
func (*QEMU) KernelParams(cfg *Config) ([]string, error) {
	switch cfg.Net.Mode {
	case "", "none":
		return TapKernelParams(cfg)
	case "user":
		if cfg.Net.IP != "" {
			return nil, errors.New("--ip only applies to a tap device (--net user has fixed addressing)")
		}
		// ip=client:server:gw:mask:host:dev:autoconf:dns0
		return []string{fmt.Sprintf("ip=%s::%s:255.255.255.0:patina:eth0:off:%s", userGuestIP, userGateway, userDNS)}, nil
	default:
		return TapKernelParams(cfg)
	}
}

// DebugExitPort is the I/O port of QEMU's isa-debug-exit device (x86_64).
// Writing v exits QEMU with status (v<<1)|1.
const DebugExitPort = 0xf4

// QEMUBinary returns the qemu-system binary name for arch.
func QEMUBinary(arch string) string {
	if arch == target.AArch64 {
		return "qemu-system-aarch64"
	}
	return "qemu-system-x86_64"
}

// ControlSocket is the path of QEMU's patina.ctl chardev socket in runDir.
func ControlSocket(runDir string) string { return filepath.Join(runDir, "qemu-ctl.sock") }

// AutoAccel picks the accelerator for arch on this host.
func AutoAccel(arch string) string {
	switch {
	case arch == target.AArch64 && runtime.GOOS == "darwin" && runtime.GOARCH == "arm64" && hvSupported():
		return "hvf"
	case runtime.GOOS == "linux" && runtime.GOARCH == target.GOARCH(arch) && kvmUsable():
		return "kvm"
	}
	return "tcg"
}

func hvSupported() bool {
	out, err := exec.Command("sysctl", "-n", "kern.hv_support").Output()
	return err == nil && strings.TrimSpace(string(out)) == "1"
}

func kvmUsable() bool {
	f, err := os.OpenFile("/dev/kvm", os.O_RDWR, 0)
	if err != nil {
		return false
	}
	f.Close()
	return true
}

// QEMUArgs returns the argv (without the binary) for cfg and accel.
func QEMUArgs(cfg *Config, accel string) ([]string, error) {
	if cfg.MemMB <= 0 || cfg.CPUs <= 0 {
		return nil, errors.New("memory and cpu count must be positive")
	}
	cpu := "max"
	if accel == "hvf" || accel == "kvm" {
		cpu = "host"
	}
	var a []string
	switch cfg.Arch {
	case target.AArch64:
		a = append(a, "-M", "virt,gic-version=3")
	case target.X86_64:
		a = append(a, "-M", "microvm")
	default:
		return nil, fmt.Errorf("unsupported arch %q", cfg.Arch)
	}
	a = append(a,
		"-accel", accel,
		"-cpu", cpu,
		"-smp", strconv.Itoa(cfg.CPUs),
		"-m", strconv.Itoa(cfg.MemMB)+"M",
		"-nodefaults", "-no-user-config",
		"-display", "none",
		"-no-reboot",
		"-kernel", cfg.Kernel,
		"-initrd", cfg.Initrd,
		"-append", cfg.Cmdline,
		// Console: first UART (PL011 on virt, 16550 at 0x3f8 on microvm) -> stdout.
		"-chardev", "stdio,id=con0,signal=off",
		"-serial", "chardev:con0",
		// Control channel: virtio-console port "patina.ctl" over a Unix socket.
		"-device", "virtio-serial-device,id=vser0",
		"-chardev", "socket,id=ctl0,path="+ControlSocket(cfg.RunDir)+",server=on,wait=off",
		"-device", "virtserialport,bus=vser0.0,chardev=ctl0,name=patina.ctl",
		"-device", "virtio-rng-device",
	)
	if cfg.Arch == target.X86_64 {
		a = append(a, "-device", fmt.Sprintf("isa-debug-exit,iobase=0x%x,iosize=0x04", DebugExitPort))
	}
	switch cfg.Net.Mode {
	case "", "none":
		if len(cfg.Net.Publish) > 0 {
			return nil, errors.New("--publish needs --net user")
		}
	case "user":
		nd := "user,id=net0"
		for _, p := range cfg.Net.Publish {
			h, g, ok := strings.Cut(p, ":")
			if _, err := strconv.ParseUint(h, 10, 16); !ok || err != nil {
				return nil, fmt.Errorf("invalid --publish %q: want hostport:guestport", p)
			}
			if _, err := strconv.ParseUint(g, 10, 16); err != nil {
				return nil, fmt.Errorf("invalid --publish %q: want hostport:guestport", p)
			}
			nd += fmt.Sprintf(",hostfwd=tcp:127.0.0.1:%s-:%s", h, g)
		}
		a = append(a, "-netdev", nd, "-device", "virtio-net-device,netdev=net0")
	default:
		if len(cfg.Net.Publish) > 0 {
			return nil, errors.New("--publish only applies to --net user")
		}
		a = append(a, "-netdev", "tap,id=net0,ifname="+cfg.Net.Mode+",script=no,downscript=no",
			"-device", "virtio-net-device,netdev=net0")
	}
	return a, nil
}

// Start implements Driver.
func (q *QEMU) Start(ctx context.Context, cfg *Config) (Machine, error) {
	bin, err := exec.LookPath(QEMUBinary(cfg.Arch))
	if err != nil {
		return nil, fmt.Errorf("%s not found in PATH (brew install qemu)", QEMUBinary(cfg.Arch))
	}
	if cfg.Stderr == nil {
		cfg.Stderr = io.Discard
	}
	if cfg.Log == nil {
		cfg.Log = io.Discard
	}
	accel, auto := cfg.Accel, false
	if accel == "" {
		accel, auto = AutoAccel(cfg.Arch), true
	}
	m := &qemuMachine{bin: bin, cfg: cfg, accel: accel, autoAccel: auto, ctx: ctx}
	if err := m.launch(); err != nil {
		return nil, err
	}
	return m, nil
}

type qemuMachine struct {
	bin       string
	cfg       *Config
	accel     string
	autoAccel bool
	ctx       context.Context

	mu      sync.Mutex
	cmd     *exec.Cmd
	stderr  *capBuffer
	started time.Time
	killed  bool
}

func (m *qemuMachine) launch() error {
	args, err := QEMUArgs(m.cfg, m.accel)
	if err != nil {
		return err
	}
	os.Remove(ControlSocket(m.cfg.RunDir))
	cmd := exec.Command(m.bin, args...)
	cmd.Stdin = nil // /dev/null: QEMU must not touch our terminal
	cmd.Stdout = m.cfg.Serial
	m.stderr = &capBuffer{max: 16 << 10}
	cmd.Stderr = io.MultiWriter(m.cfg.Stderr, m.stderr)
	// Own process group: the terminal's Ctrl-C goes to `patina run`, which
	// turns it into a graceful stop instead of QEMU dying on SIGINT.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	fmt.Fprintf(m.cfg.Log, "+ %s %s\n", m.bin, shellQuote(args))
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("starting %s: %w", m.bin, err)
	}
	m.mu.Lock()
	m.cmd, m.started = cmd, time.Now()
	m.mu.Unlock()
	go func() {
		<-m.ctx.Done()
		m.Kill()
	}()
	return nil
}

func (m *qemuMachine) Control() ctl.Endpoint {
	return ctl.Endpoint{Kind: ctl.KindStream, Path: ControlSocket(m.cfg.RunDir)}
}

func (m *qemuMachine) PID() int {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.cmd == nil || m.cmd.Process == nil {
		return 0
	}
	return m.cmd.Process.Pid
}

func (m *qemuMachine) Wait() (*Exit, error) {
	for {
		m.mu.Lock()
		cmd := m.cmd
		m.mu.Unlock()
		err := cmd.Wait()
		code := cmd.ProcessState.ExitCode()
		var ee *exec.ExitError
		if err != nil && !errors.As(err, &ee) {
			return nil, err
		}
		m.mu.Lock()
		killed := m.killed
		m.mu.Unlock()
		// hvf can be compiled in but unusable (e.g. a QEMU binary without the
		// hypervisor entitlement, or nested virtualization): retry on TCG.
		if !killed && code != 0 && m.autoAccel && m.accel == "hvf" &&
			time.Since(m.started) < 5*time.Second && m.stderr.contains("hvf", "HV_", "accelerator") {
			fmt.Fprintf(m.cfg.Stderr, "patina: hvf unavailable, falling back to TCG (slow)\n")
			m.accel = "tcg"
			if err := m.launch(); err != nil {
				return nil, err
			}
			continue
		}
		ex := &Exit{Code: code}
		// isa-debug-exit: status = (v<<1)|1. Status 1 is ambiguous with a
		// QEMU error, so only v >= 1 is recognised.
		if m.cfg.Arch == target.X86_64 && code >= 3 && code%2 == 1 && !killed {
			g := code >> 1
			ex.GuestCode = &g
		}
		return ex, nil
	}
}

func (m *qemuMachine) Kill() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.killed = true
	if m.cmd == nil || m.cmd.Process == nil || m.cmd.ProcessState != nil {
		return nil
	}
	return m.cmd.Process.Kill()
}

// capBuffer keeps the first max bytes written to it.
type capBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
	max int
}

func (c *capBuffer) Write(p []byte) (int, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if room := c.max - c.buf.Len(); room > 0 {
		if len(p) < room {
			room = len(p)
		}
		c.buf.Write(p[:room])
	}
	return len(p), nil
}

func (c *capBuffer) contains(subs ...string) bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	s := c.buf.String()
	for _, x := range subs {
		if strings.Contains(s, x) {
			return true
		}
	}
	return false
}

func shellQuote(args []string) string {
	q := make([]string, len(args))
	for i, a := range args {
		if a != "" && !strings.ContainsAny(a, " \t\n'\"\\$`*?[]{}()<>|&;#~!") {
			q[i] = a
		} else {
			q[i] = "'" + strings.ReplaceAll(a, "'", `'\''`) + "'"
		}
	}
	return strings.Join(q, " ")
}
