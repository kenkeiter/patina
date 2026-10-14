package target

import "testing"

func TestParse(t *testing.T) {
	ok := map[string]Target{
		"qemu-aarch64":            {QEMU, AArch64},
		"qemu-arm64":              {QEMU, AArch64},
		"firecracker-x86_64":      {Firecracker, X86_64},
		"cloud-hypervisor-x86_64": {CloudHypervisor, X86_64},
		"cloud-hypervisor-amd64":  {CloudHypervisor, X86_64},
		"vz-aarch64":              {VZ, AArch64},
	}
	for in, want := range ok {
		got, err := Parse(in)
		if err != nil || got != want {
			t.Errorf("%s: %v %v", in, got, err)
		}
	}
	for _, bad := range []string{"vz-x86_64", "qemu", "-aarch64", "xen-x86_64", "qemu-riscv64", ""} {
		if _, err := Parse(bad); err == nil {
			t.Errorf("%q accepted", bad)
		}
	}
	if KernelFile(AArch64) != "kernel-aarch64.Image" || KernelFile(X86_64) != "kernel-x86_64-pvh.elf" {
		t.Error("kernel file names")
	}
}
