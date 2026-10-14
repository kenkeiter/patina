// Package vmm abstracts the virtual machine monitors patina images run on.
// Each driver turns a Config into a running VM and tells the caller how to
// reach that VM's control channel; `patina run` does everything else
// (cmdline, control proxy, exit-code policy) identically for all of them.
package vmm

import (
	"context"
	"fmt"
	"io"
	"sort"
	"strings"

	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/target"
)

// NetConfig selects guest networking.
type NetConfig struct {
	Mode    string   // "" / "none", "user" (QEMU slirp), or a tap device name
	Publish []string // "hostport:guestport" TCP forwards (user mode)
	IP      string   // static guest address for a tap device (see TapKernelParams); "" = DHCP
}

// Config describes a VM to start.
type Config struct {
	Arch    string
	Kernel  string // path to the kernel artifact
	Initrd  string // path to the initrd cpio
	Cmdline string
	MemMB   int
	CPUs    int
	RunDir  string // private scratch dir for this VM (sockets, logs)
	Accel   string // "" = auto; QEMU: hvf | kvm | tcg
	Bin     string // VMM binary override (Firecracker, Cloud Hypervisor); "" = look up in PATH
	Net     NetConfig

	Serial io.Writer // guest console output
	Stderr io.Writer // the VMM's own diagnostics
	Log    io.Writer // verbose CLI logging (never nil; may be io.Discard)
}

// Exit describes how the VMM process ended.
type Exit struct {
	Code int // VMM process exit status (-1 if killed by a signal)
	// GuestCode is an exit code the VMM itself carried out of the guest
	// (QEMU isa-debug-exit), if any.
	GuestCode *int
}

// Machine is a started VM.
type Machine interface {
	// Control is the VMM-side endpoint of the guest control channel.
	Control() ctl.Endpoint
	// PID of the VMM process (0 if not applicable).
	PID() int
	// Wait blocks until the VMM has exited.
	Wait() (*Exit, error)
	// Kill terminates the VMM immediately.
	Kill() error
}

// Driver starts VMs on one kind of VMM.
type Driver interface {
	Name() string
	// Console is the console= value the guest should use on arch.
	Console(arch string) string
	// KernelParams are extra kernel parameters the driver needs (e.g. ip=
	// for its network setup), placed before "--".
	KernelParams(cfg *Config) ([]string, error)
	// Start launches the VM. ctx cancellation kills it.
	Start(ctx context.Context, cfg *Config) (Machine, error)
}

var drivers = map[string]Driver{}

func register(d Driver) { drivers[d.Name()] = d }

// Get returns the driver for a VMM name.
func Get(name string) (Driver, error) {
	if d, ok := drivers[name]; ok {
		return d, nil
	}
	names := make([]string, 0, len(drivers))
	for n := range drivers {
		names = append(names, n)
	}
	sort.Strings(names)
	return nil, fmt.Errorf("unknown VMM %q (known: %s)", name, strings.Join(names, ", "))
}

// unimplemented is a placeholder driver for VMMs not wired up yet.
type unimplemented struct{ name, console string }

func (u unimplemented) Name() string                           { return u.name }
func (u unimplemented) Console(string) string                  { return u.console }
func (u unimplemented) KernelParams(*Config) ([]string, error) { return nil, nil }
func (u unimplemented) Start(context.Context, *Config) (Machine, error) {
	return nil, fmt.Errorf("VMM driver %q is not implemented yet (use --vmm qemu)", u.name)
}

func init() {
	register(&QEMU{})
	// TODO: Firecracker (API socket: boot-source, drives none, vsock with
	// uds_path -> ctl.KindFirecracker endpoint), Cloud Hypervisor (--api-socket,
	// --vsock cid=,socket= -> same handshake), Apple VZ (cgo/ObjC shim,
	// VZVirtioSocketDevice.connect(toPort: 4242), console hvc0).
	register(&Firecracker{})
	register(&CloudHypervisor{})
	register(unimplemented{name: target.VZ, console: "hvc0"})
}
