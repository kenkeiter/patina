package vmm

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/target"
)

// CloudHypervisor drives the cloud-hypervisor binary with its command-line
// flags (written against v54.0):
//
//	cloud-hypervisor --api-socket path=<run>/ch.sock
//	  --kernel K --initramfs I --cmdline C --cpus boot=N --memory size=<M>M
//	  --serial tty --console off --vsock cid=3,socket=<run>/vsock.sock
//	  --rng src=/dev/urandom [--net tap=<dev>,mac=<mac>]
//
// Flags rather than PUT /api/v1/vm.create + vm.boot because:
//   - one exec both configures and boots, so there is no window in which the
//     VMM is up but unconfigured, and no API-socket readiness race before the
//     VM exists; a bad configuration fails the process with the reason on
//     stderr;
//   - the flag syntax has been stable for many releases, while the
//     vm.create body has changed shape (kernel/cmdline/initramfs moved into
//     "payload" in v28) and is validated against each release's schema.
//
// The API socket is still opened: the driver pings it (GET
// /api/v1/vmm.ping) for the version, and operators can use ch-remote on it.
//
// Console: --serial tty puts the legacy UART on the VMM's stdout: a 16550 at
// port 0x3f8 on x86_64 (ttyS0), a PL011 on aarch64 (ttyAMA0). The
// virtio-console (default tty) is turned off so the two don't share stdout.
// Devices are virtio-pci. The control channel is the vsock host socket with
// the same CONNECT 4242 handshake as Firecracker.
type CloudHypervisor struct{}

// Name implements Driver.
func (*CloudHypervisor) Name() string { return target.CloudHypervisor }

// Console implements Driver.
func (*CloudHypervisor) Console(arch string) string {
	if arch == target.AArch64 {
		return "ttyAMA0"
	}
	return "ttyS0"
}

// KernelParams implements Driver.
func (*CloudHypervisor) KernelParams(cfg *Config) ([]string, error) {
	if err := tapOnly(target.CloudHypervisor, cfg.Net); err != nil {
		return nil, err
	}
	return TapKernelParams(cfg)
}

func apiSocketCH(runDir string) string { return filepath.Join(runDir, "ch.sock") }

// CloudHypervisorGuestCID is the vsock context ID given to every guest.
const CloudHypervisorGuestCID = 3

// CloudHypervisorArgs returns the cloud-hypervisor argv (without the binary).
func CloudHypervisorArgs(cfg *Config) ([]string, error) {
	switch cfg.Arch {
	case target.X86_64, target.AArch64:
	default:
		return nil, fmt.Errorf("cloud-hypervisor: unsupported arch %q", cfg.Arch)
	}
	if cfg.MemMB <= 0 || cfg.CPUs <= 0 {
		return nil, errors.New("memory and cpu count must be positive")
	}
	if err := tapOnly(target.CloudHypervisor, cfg.Net); err != nil {
		return nil, err
	}
	api, vsock := apiSocketCH(cfg.RunDir), VsockSocket(cfg.RunDir)
	// These go inside "k=v,k=v" option lists, which have no quoting.
	for _, p := range []string{api, vsock, cfg.Net.Mode} {
		if strings.ContainsAny(p, ",=") {
			return nil, fmt.Errorf("cloud-hypervisor: %q must not contain ',' or '=' (it goes into a key=value option list)", p)
		}
	}
	a := []string{
		"--api-socket", "path=" + api,
		"--kernel", cfg.Kernel,
		"--initramfs", cfg.Initrd,
		"--cmdline", cfg.Cmdline,
		"--cpus", "boot=" + strconv.Itoa(cfg.CPUs),
		"--memory", "size=" + strconv.Itoa(cfg.MemMB) + "M",
		"--serial", "tty",
		"--console", "off",
		"--vsock", fmt.Sprintf("cid=%d,socket=%s", CloudHypervisorGuestCID, vsock),
		"--rng", "src=/dev/urandom",
	}
	switch cfg.Net.Mode {
	case "", "none":
	default:
		a = append(a, "--net", "tap="+cfg.Net.Mode+",mac="+GuestMAC(cfg.RunDir))
	}
	return a, nil
}

// CloudHypervisorPing is the informational API call made after start.
var CloudHypervisorPing = APIRequest{Method: "GET", Path: "/api/v1/vmm.ping"}

// Start implements Driver.
func (*CloudHypervisor) Start(ctx context.Context, cfg *Config) (Machine, error) {
	args, err := CloudHypervisorArgs(cfg)
	if err != nil {
		return nil, err
	}
	bin, err := lookBin(cfg.Bin, "cloud-hypervisor", "Cloud Hypervisor needs a Linux host with /dev/kvm")
	if err != nil {
		return nil, err
	}
	api, vsock := apiSocketCH(cfg.RunDir), VsockSocket(cfg.RunDir)
	os.Remove(api)
	os.Remove(vsock)
	ep := ctl.Endpoint{Kind: ctl.KindFirecracker, Path: vsock, Port: ctl.Port}
	m, err := startProc(ctx, target.CloudHypervisor, cfg, bin, args, ep, "")
	if err != nil {
		return nil, err
	}
	// Informational only: failures here are reported by the process exit
	// (its stderr is already streaming to the user).
	if err := m.waitSocket(api, 5*time.Second); err != nil {
		fmt.Fprintf(cfg.Log, "patina: cloud-hypervisor: %v\n", err)
		return m, nil
	}
	c := newAPIClient(api)
	defer c.close()
	fmt.Fprintf(cfg.Log, "+ cloud-hypervisor API: %s\n", CloudHypervisorPing)
	b, err := c.do(CloudHypervisorPing)
	if err != nil {
		fmt.Fprintf(cfg.Log, "patina: cloud-hypervisor: %v\n", err)
		return m, nil
	}
	var ping struct {
		Version      string `json:"version"`
		BuildVersion string `json:"build_version"`
	}
	if json.Unmarshal(b, &ping) == nil {
		fmt.Fprintf(cfg.Log, "patina: cloud-hypervisor %s (%s), pid %d\n", ping.Version, ping.BuildVersion, m.PID())
	}
	return m, nil
}
