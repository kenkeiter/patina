// Package target names the (VMM, architecture) pairs patina images are built
// for and the per-arch facts that follow from them.
package target

import (
	"debug/elf"
	"fmt"
	"strings"
)

// Architectures, spelled the way the kernel artifacts are.
const (
	X86_64  = "x86_64"
	AArch64 = "aarch64"
)

// VMMs.
const (
	Firecracker     = "firecracker"
	CloudHypervisor = "cloud-hypervisor"
	QEMU            = "qemu"
	VZ              = "vz"
)

// VMMs lists every known VMM.
var VMMs = []string{Firecracker, CloudHypervisor, QEMU, VZ}

// Target is a VMM + architecture pair such as "qemu-aarch64".
type Target struct {
	VMM  string
	Arch string
}

func (t Target) String() string { return t.VMM + "-" + t.Arch }

// NormalizeArch accepts Go and kernel spellings and returns the kernel one.
func NormalizeArch(a string) (string, error) {
	switch strings.ToLower(a) {
	case "x86_64", "amd64", "x86-64":
		return X86_64, nil
	case "aarch64", "arm64":
		return AArch64, nil
	}
	return "", fmt.Errorf("unknown architecture %q (want x86_64 or aarch64)", a)
}

// ValidVMM reports whether v is a known VMM name.
func ValidVMM(v string) bool {
	for _, k := range VMMs {
		if v == k {
			return true
		}
	}
	return false
}

// Parse parses "<vmm>-<arch>", e.g. "cloud-hypervisor-x86_64".
func Parse(s string) (Target, error) {
	i := strings.LastIndexByte(s, '-')
	if i <= 0 {
		return Target{}, fmt.Errorf("invalid target %q: want <vmm>-<arch>, e.g. qemu-aarch64", s)
	}
	return New(s[:i], s[i+1:])
}

// New validates a VMM/arch pair.
func New(vmm, arch string) (Target, error) {
	a, err := NormalizeArch(arch)
	if err != nil {
		return Target{}, err
	}
	if !ValidVMM(vmm) {
		return Target{}, fmt.Errorf("unknown VMM %q (want one of %s)", vmm, strings.Join(VMMs, ", "))
	}
	if vmm == VZ && a != AArch64 {
		return Target{}, fmt.Errorf("target vz-%s: Apple Virtualization.framework targets are aarch64 only", a)
	}
	return Target{VMM: vmm, Arch: a}, nil
}

// GOARCH returns the Go architecture name for a kernel arch.
func GOARCH(arch string) string {
	if arch == X86_64 {
		return "amd64"
	}
	return "arm64"
}

// ELFMachine returns the ELF e_machine expected for arch.
func ELFMachine(arch string) elf.Machine {
	if arch == X86_64 {
		return elf.EM_X86_64
	}
	return elf.EM_AARCH64
}

// KernelFile is the release artifact name of the kernel for arch. The same
// artifact serves every VMM of that arch.
func KernelFile(arch string) string {
	if arch == X86_64 {
		return "kernel-x86_64-pvh.elf"
	}
	return "kernel-aarch64.Image"
}
