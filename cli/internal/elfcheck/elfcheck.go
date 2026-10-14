// Package elfcheck verifies that a binary is something the patina kernel can
// load: a 64-bit, static, non-PIE Linux ELF executable for the right machine,
// built by Go without cgo.
package elfcheck

import (
	"debug/buildinfo"
	"debug/elf"
	"fmt"
)

// Info is what the CLI records about a verified binary.
type Info struct {
	GoVersion string // e.g. "go1.27.1"
	Path      string // main package import path
	Module    string // main module path
	Entry     uint64
}

// Check validates the ELF at path for machine m.
func Check(path string, m elf.Machine) (*Info, error) {
	f, err := elf.Open(path)
	if err != nil {
		return nil, fmt.Errorf("%s: not an ELF file: %w", path, err)
	}
	defer f.Close()
	if err := checkELF(f, m); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	info := &Info{Entry: f.Entry}
	bi, err := buildinfo.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("%s: no Go build info (is this a Go binary?): %w", path, err)
	}
	info.GoVersion = bi.GoVersion
	info.Path = bi.Path
	info.Module = bi.Main.Path
	for _, s := range bi.Settings {
		if s.Key == "CGO_ENABLED" && s.Value == "1" {
			return nil, fmt.Errorf("%s: built with CGO_ENABLED=1; patina needs CGO_ENABLED=0", path)
		}
	}
	return info, nil
}

func checkELF(f *elf.File, m elf.Machine) error {
	if f.Class != elf.ELFCLASS64 {
		return fmt.Errorf("ELF class %v, want ELFCLASS64", f.Class)
	}
	if f.Machine != m {
		return fmt.Errorf("ELF machine %v, want %v (built for the wrong architecture?)", f.Machine, m)
	}
	if f.OSABI != elf.ELFOSABI_NONE && f.OSABI != elf.ELFOSABI_LINUX {
		return fmt.Errorf("ELF OS ABI %v, want Linux/SYSV (build with GOOS=linux)", f.OSABI)
	}
	switch f.Type {
	case elf.ET_EXEC:
	case elf.ET_DYN:
		return fmt.Errorf("ELF type ET_DYN: position-independent (PIE) or shared object; patina needs a static non-PIE executable (do not use -buildmode=pie)")
	default:
		return fmt.Errorf("ELF type %v, want ET_EXEC", f.Type)
	}
	for _, p := range f.Progs {
		switch p.Type {
		case elf.PT_INTERP:
			return fmt.Errorf("has PT_INTERP (dynamically linked); build with CGO_ENABLED=0 -ldflags=-extldflags=-static")
		case elf.PT_DYNAMIC:
			return fmt.Errorf("has PT_DYNAMIC (dynamically linked); build with CGO_ENABLED=0 -ldflags=-extldflags=-static")
		}
	}
	return nil
}
