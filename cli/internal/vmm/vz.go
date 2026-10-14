package vmm

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"time"

	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/target"
)

// VZ drives Apple's Virtualization.framework in-process through a cgo /
// Objective-C shim (vz_darwin_arm64.go; darwin/arm64 with cgo only):
//
//	VZLinuxBootLoader (Image + initrd + cmdline), VZGenericPlatformConfiguration,
//	virtio-console serial port (hvc0) -> cfg.Serial,
//	virtio-vsock (control channel: connectToPort 4242),
//	virtio-entropy, and with --net a virtio-net NIC on VZ's NAT (DHCP).
//
// The VM runs on a private dispatch queue. There is no VMM process, so PID
// is 0 and Kill stops the VM through the framework. The `patina` binary must
// carry the com.apple.security.virtualization entitlement (cli/scripts/build.sh
// signs it).
type VZ struct{}

func init() { register(&VZ{}) }

// TimeSyncer is implemented by drivers whose guests have no wall clock at
// boot: `patina run` sends {"op":"time"} after the guest's ready event.
type TimeSyncer interface {
	NeedsTimeSync() bool
}

// Name implements Driver.
func (*VZ) Name() string { return target.VZ }

// Console implements Driver. VZ's only console is virtio-console.
func (*VZ) Console(string) string { return "hvc0" }

// NeedsTimeSync implements TimeSyncer: VZ exposes no RTC to the guest.
func (*VZ) NeedsTimeSync() bool { return true }

// KernelParams implements Driver. VZ's NAT attachment runs a DHCP server
// (192.168.64.0/24 by default, host at .1), so the guest uses ip=dhcp.
func (*VZ) KernelParams(cfg *Config) ([]string, error) {
	spec, err := VZSpecFor(cfg)
	if err != nil {
		return nil, err
	}
	if spec.NAT {
		return []string{"ip=dhcp"}, nil
	}
	return nil, nil
}

// VZSpec is the platform-neutral description of the VZ machine built for a
// Config; the darwin shim turns it into a VZVirtualMachineConfiguration.
type VZSpec struct {
	Kernel, Initrd, Cmdline string
	CPUs                    int
	MemBytes                uint64
	NAT                     bool   // virtio-net on VZNATNetworkDeviceAttachment
	VsockPort               uint32 // guest port the control channel connects to
}

// VZSpecFor validates cfg for VZ and returns the machine description.
func VZSpecFor(cfg *Config) (*VZSpec, error) {
	if cfg.Arch != target.AArch64 {
		return nil, fmt.Errorf("vz: Apple Virtualization.framework runs aarch64 guests only (got %s)", cfg.Arch)
	}
	if cfg.MemMB <= 0 || cfg.CPUs <= 0 {
		return nil, errors.New("memory and cpu count must be positive")
	}
	if cfg.Accel != "" {
		return nil, fmt.Errorf("vz: --accel does not apply (Virtualization.framework always uses Hypervisor.framework)")
	}
	s := &VZSpec{
		Kernel: cfg.Kernel, Initrd: cfg.Initrd, Cmdline: cfg.Cmdline,
		CPUs: cfg.CPUs, MemBytes: uint64(cfg.MemMB) << 20,
		VsockPort: ctl.Port,
	}
	switch cfg.Net.Mode {
	case "", "none":
		if len(cfg.Net.Publish) > 0 {
			return nil, errors.New("vz: --publish is not supported (VZ NAT has no port forwarding; reach the guest at its DHCP address)")
		}
	case "user", "nat":
		if len(cfg.Net.Publish) > 0 {
			return nil, errors.New("vz: --publish is not supported (VZ NAT has no port forwarding; reach the guest at its DHCP address)")
		}
		s.NAT = true
	default:
		return nil, fmt.Errorf("vz: --net %q is not supported (use --net user for VZ NAT)", cfg.Net.Mode)
	}
	return s, nil
}

// Start implements Driver.
func (*VZ) Start(ctx context.Context, cfg *Config) (Machine, error) {
	spec, err := VZSpecFor(cfg)
	if err != nil {
		return nil, err
	}
	for _, p := range []string{spec.Kernel, spec.Initrd} {
		if _, err := os.Stat(p); err != nil {
			return nil, fmt.Errorf("vz: %w", err)
		}
	}
	return vzStart(ctx, cfg, spec)
}

// vzMachine is the platform-neutral half of a running VZ VM.
type vzMachine struct {
	port   uint32
	dialFn func(port uint32, timeout time.Duration) (net.Conn, error)
	wait   func() (*Exit, error)
	kill   func() error
}

func (m *vzMachine) Control() ctl.Endpoint {
	return ctl.Endpoint{Kind: ctl.KindVZ, Port: m.port, Dialer: func(timeout time.Duration) (net.Conn, error) {
		return m.dialFn(m.port, timeout)
	}}
}

func (m *vzMachine) PID() int             { return 0 }
func (m *vzMachine) Wait() (*Exit, error) { return m.wait() }
func (m *vzMachine) Kill() error          { return m.kill() }
