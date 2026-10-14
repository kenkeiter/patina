package vmm

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/target"
)

// Firecracker drives the firecracker binary through its REST API socket
// (swagger: src/firecracker/swagger/firecracker.yaml, written against
// v1.17.0):
//
//	firecracker --api-sock <run>/fc.sock --log-path <run>/fc.log --level Warning
//	PUT /boot-source            {kernel_image_path, initrd_path, boot_args}
//	PUT /machine-config         {vcpu_count, mem_size_mib}
//	PUT /vsock                  {guest_cid: 3, uds_path: <run>/vsock.sock}
//	PUT /network-interfaces/eth0 {iface_id, host_dev_name, guest_mac}   (--net <tap>)
//	PUT /entropy                {}
//	PUT /actions                {action_type: InstanceStart}
//
// Kernel: PVH ELF on x86_64, Linux arm64 Image on aarch64. Console: the
// 16550 UART (port 0x3f8 on x86_64; an MMIO ns16550a in the DTB on aarch64)
// whose output Firecracker writes to its stdout. The control channel is the
// vsock device's host Unix socket (CONNECT 4242). Firecracker's logger also
// defaults to stdout, so it is pointed at fc.log to keep the console clean.
//
// Stop is the control channel's job (SendCtrlAltDel is not used: it needs an
// i8042 driver in the guest and does nothing on aarch64); a forced stop
// kills the process.
type Firecracker struct{}

// FirecrackerGuestCID is the vsock context ID given to every guest.
const FirecrackerGuestCID = 3

// FirecrackerMaxCmdline bounds boot_args. Firecracker's command line buffer
// is 2048 bytes (CMDLINE_MAX_SIZE, NUL included) on both arches, and it
// appends " pci=off" plus, on x86_64, one " virtio_mmio.device=4K@0x...:N"
// per virtio device and, on aarch64, " earlycon=uart,mmio,0x...".
const FirecrackerMaxCmdline = 2048 - 256

// Name implements Driver.
func (*Firecracker) Name() string { return target.Firecracker }

// Console implements Driver. Both arches use a 16550 (8250 driver: ttyS0).
func (*Firecracker) Console(string) string { return "ttyS0" }

// KernelParams implements Driver.
func (*Firecracker) KernelParams(cfg *Config) ([]string, error) {
	if err := tapOnly(target.Firecracker, cfg.Net); err != nil {
		return nil, err
	}
	return TapKernelParams(cfg)
}

// Run-dir files shared by the vsock-based drivers.
func apiSocketFC(runDir string) string { return filepath.Join(runDir, "fc.sock") }
func logFileFC(runDir string) string   { return filepath.Join(runDir, "fc.log") }

// VsockSocket is the host side of the guest's vsock device (Firecracker
// uds_path, Cloud Hypervisor --vsock socket=).
func VsockSocket(runDir string) string { return filepath.Join(runDir, "vsock.sock") }

// FirecrackerArgs returns the firecracker argv (without the binary).
func FirecrackerArgs(cfg *Config) []string {
	return []string{"--api-sock", apiSocketFC(cfg.RunDir), "--log-path", logFileFC(cfg.RunDir), "--level", "Warning"}
}

// Firecracker API bodies (field order is the order sent).
type (
	fcBootSource struct {
		KernelImagePath string `json:"kernel_image_path"`
		InitrdPath      string `json:"initrd_path,omitempty"`
		BootArgs        string `json:"boot_args"`
	}
	fcMachineConfig struct {
		VcpuCount  int `json:"vcpu_count"`
		MemSizeMib int `json:"mem_size_mib"`
	}
	fcVsock struct {
		GuestCID int    `json:"guest_cid"`
		UDSPath  string `json:"uds_path"`
	}
	fcNetIface struct {
		IfaceID     string `json:"iface_id"`
		HostDevName string `json:"host_dev_name"`
		GuestMAC    string `json:"guest_mac"`
	}
	fcEntropy struct{}
	fcAction  struct {
		ActionType string `json:"action_type"`
	}
)

// FirecrackerRequests returns the API calls that configure and start cfg's
// microVM, in order.
func FirecrackerRequests(cfg *Config) ([]APIRequest, error) {
	switch cfg.Arch {
	case target.X86_64, target.AArch64:
	default:
		return nil, fmt.Errorf("firecracker: unsupported arch %q", cfg.Arch)
	}
	if cfg.MemMB <= 0 {
		return nil, errors.New("memory size must be positive")
	}
	// firecracker.yaml MachineConfiguration: "Number of vCPUs (either 1 or an
	// even number)", maximum 32.
	if cfg.CPUs < 1 || cfg.CPUs > 32 || (cfg.CPUs > 1 && cfg.CPUs%2 != 0) {
		return nil, fmt.Errorf("firecracker needs 1 or an even number of vCPUs up to 32 (got %d)", cfg.CPUs)
	}
	if err := tapOnly(target.Firecracker, cfg.Net); err != nil {
		return nil, err
	}
	if len(cfg.Cmdline) > FirecrackerMaxCmdline {
		return nil, fmt.Errorf("firecracker: kernel command line is %d bytes; at most %d fit alongside what Firecracker appends (2048-byte limit)", len(cfg.Cmdline), FirecrackerMaxCmdline)
	}
	r := []APIRequest{
		{"PUT", "/boot-source", fcBootSource{KernelImagePath: cfg.Kernel, InitrdPath: cfg.Initrd, BootArgs: cfg.Cmdline}},
		{"PUT", "/machine-config", fcMachineConfig{VcpuCount: cfg.CPUs, MemSizeMib: cfg.MemMB}},
		{"PUT", "/vsock", fcVsock{GuestCID: FirecrackerGuestCID, UDSPath: VsockSocket(cfg.RunDir)}},
	}
	switch cfg.Net.Mode {
	case "", "none":
	default:
		r = append(r, APIRequest{"PUT", "/network-interfaces/eth0", fcNetIface{IfaceID: "eth0", HostDevName: cfg.Net.Mode, GuestMAC: GuestMAC(cfg.RunDir)}})
	}
	r = append(r,
		APIRequest{"PUT", "/entropy", fcEntropy{}},
		APIRequest{"PUT", "/actions", fcAction{ActionType: "InstanceStart"}},
	)
	return r, nil
}

// Start implements Driver.
func (*Firecracker) Start(ctx context.Context, cfg *Config) (Machine, error) {
	reqs, err := FirecrackerRequests(cfg)
	if err != nil {
		return nil, err
	}
	bin, err := lookBin(cfg.Bin, "firecracker", "Firecracker needs a Linux host with /dev/kvm")
	if err != nil {
		return nil, err
	}
	api, vsock, logf := apiSocketFC(cfg.RunDir), VsockSocket(cfg.RunDir), logFileFC(cfg.RunDir)
	os.Remove(api)
	os.Remove(vsock)
	// --log-path must name an existing file or FIFO.
	if err := os.WriteFile(logf, nil, 0o600); err != nil {
		return nil, err
	}
	ep := ctl.Endpoint{Kind: ctl.KindFirecracker, Path: vsock, Port: ctl.Port}
	m, err := startProc(ctx, target.Firecracker, cfg, bin, FirecrackerArgs(cfg), ep, logf)
	if err != nil {
		return nil, err
	}
	if err := m.waitSocket(api, 5*time.Second); err != nil {
		m.abort()
		m.replayLog()
		return nil, err
	}
	c := newAPIClient(api)
	defer c.close()
	for _, r := range reqs {
		fmt.Fprintf(cfg.Log, "+ firecracker API: %s\n", r)
		if _, err := c.do(r); err != nil {
			m.abort()
			m.replayLog()
			return nil, fmt.Errorf("firecracker: %w", err)
		}
	}
	return m, nil
}
