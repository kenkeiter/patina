// Package image reads and writes .patina bundles: an uncompressed tar holding
//
//	manifest.json        (always the first member)
//	initrd.cpio          newc cpio with /app and --files content
//	kernel-<arch>...     the kernel artifact named by manifest.kernel
package image

import (
	"archive/tar"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"time"

	"github.com/kenkeiter/patina/cli/internal/target"
)

// FormatVersion is the bundle format written by this CLI.
const FormatVersion = 1

// Member names.
const (
	ManifestName = "manifest.json"
	InitrdName   = "initrd.cpio"
)

// Manifest describes a bundle.
type Manifest struct {
	Format    int               `json:"format"`
	Name      string            `json:"name"`
	Arch      string            `json:"arch"`   // x86_64 | aarch64
	VMM       string            `json:"vmm"`    // firecracker | cloud-hypervisor | qemu | vz
	Target    string            `json:"target"` // <vmm>-<arch>
	GoVersion string            `json:"go_version"`
	GoPackage string            `json:"go_package,omitempty"`
	Kernel    string            `json:"kernel"` // member name of the kernel
	Initrd    string            `json:"initrd"` // member name of the initrd
	SHA256    map[string]string `json:"sha256"` // keys: "app", "kernel", "initrd"
	Created   time.Time         `json:"created"`
	CLI       string            `json:"cli_version,omitempty"`
}

// Validate checks internal consistency.
func (m *Manifest) Validate() error {
	if m.Format != FormatVersion {
		return fmt.Errorf("unsupported bundle format %d (this CLI understands %d)", m.Format, FormatVersion)
	}
	t, err := target.New(m.VMM, m.Arch)
	if err != nil {
		return err
	}
	if m.Target != t.String() {
		return fmt.Errorf("manifest target %q does not match vmm/arch %s", m.Target, t)
	}
	if m.Kernel == "" || m.Initrd == "" || m.Kernel == m.Initrd || m.Kernel == ManifestName || m.Initrd == ManifestName {
		return errors.New("manifest kernel/initrd member names missing or clashing")
	}
	if filepath.Base(m.Kernel) != m.Kernel || filepath.Base(m.Initrd) != m.Initrd {
		return errors.New("manifest member names must be plain file names")
	}
	for _, k := range []string{"kernel", "initrd"} {
		if len(m.SHA256[k]) != 64 {
			return fmt.Errorf("manifest sha256.%s missing or malformed", k)
		}
	}
	return nil
}

// FileSHA256 returns the hex sha256 of a file.
func FileSHA256(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

// Write creates the bundle at out. It fills m.Kernel, m.Initrd and the
// kernel/initrd hashes; m.SHA256["app"] is left to the caller.
func Write(out string, m *Manifest, kernelPath, initrdPath string) error {
	m.Format = FormatVersion
	m.Kernel = target.KernelFile(m.Arch)
	m.Initrd = InitrdName
	if m.SHA256 == nil {
		m.SHA256 = map[string]string{}
	}
	var err error
	if m.SHA256["kernel"], err = FileSHA256(kernelPath); err != nil {
		return err
	}
	if m.SHA256["initrd"], err = FileSHA256(initrdPath); err != nil {
		return err
	}
	if err := m.Validate(); err != nil {
		return err
	}
	mj, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	mj = append(mj, '\n')

	tmp, err := os.CreateTemp(filepath.Dir(out), ".patina-*.tmp")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())
	tw := tar.NewWriter(tmp)
	mtime := m.Created
	if err := tw.WriteHeader(&tar.Header{Name: ManifestName, Mode: 0o644, Size: int64(len(mj)), ModTime: mtime, Format: tar.FormatPAX}); err != nil {
		tmp.Close()
		return err
	}
	if _, err := tw.Write(mj); err != nil {
		tmp.Close()
		return err
	}
	for _, mem := range []struct{ name, src string }{{m.Initrd, initrdPath}, {m.Kernel, kernelPath}} {
		if err := addFile(tw, mem.name, mem.src, mtime); err != nil {
			tmp.Close()
			return err
		}
	}
	if err := tw.Close(); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmp.Name(), out)
}

func addFile(tw *tar.Writer, name, src string, mtime time.Time) error {
	f, err := os.Open(src)
	if err != nil {
		return err
	}
	defer f.Close()
	fi, err := f.Stat()
	if err != nil {
		return err
	}
	if err := tw.WriteHeader(&tar.Header{Name: name, Mode: 0o644, Size: fi.Size(), ModTime: mtime, Format: tar.FormatPAX}); err != nil {
		return err
	}
	_, err = io.Copy(tw, f)
	return err
}

// Bundle is an opened .patina file.
type Bundle struct {
	Path     string
	Manifest Manifest
}

// Open reads and validates the manifest of the bundle at path.
func Open(path string) (*Bundle, error) {
	b := &Bundle{Path: path}
	found := false
	err := b.walk(func(h *tar.Header, r io.Reader) (bool, error) {
		if h.Name != ManifestName {
			return false, fmt.Errorf("%s: first member is %q, want %s (not a .patina bundle?)", path, h.Name, ManifestName)
		}
		data, err := io.ReadAll(io.LimitReader(r, 1<<20))
		if err != nil {
			return false, err
		}
		if err := json.Unmarshal(data, &b.Manifest); err != nil {
			return false, fmt.Errorf("%s: bad manifest: %w", path, err)
		}
		found = true
		return true, nil
	})
	if err != nil {
		return nil, err
	}
	if !found {
		return nil, fmt.Errorf("%s: empty bundle", path)
	}
	if err := b.Manifest.Validate(); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return b, nil
}

// walk calls fn for each tar member until fn returns stop=true.
func (b *Bundle) walk(fn func(h *tar.Header, r io.Reader) (stop bool, err error)) error {
	f, err := os.Open(b.Path)
	if err != nil {
		return err
	}
	defer f.Close()
	tr := tar.NewReader(f)
	for {
		h, err := tr.Next()
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return fmt.Errorf("%s: %w", b.Path, err)
		}
		stop, err := fn(h, tr)
		if err != nil || stop {
			return err
		}
	}
}

// Member streams the named member to fn.
func (b *Bundle) Member(name string, fn func(r io.Reader) error) error {
	found := false
	err := b.walk(func(h *tar.Header, r io.Reader) (bool, error) {
		if h.Name != name {
			return false, nil
		}
		found = true
		return true, fn(r)
	})
	if err == nil && !found {
		err = fmt.Errorf("%s: member %s missing", b.Path, name)
	}
	return err
}

// Extract writes the named member to dst and verifies its sha256 against
// the manifest entry key (if non-empty).
func (b *Bundle) Extract(name, dst, hashKey string) error {
	return b.Member(name, func(r io.Reader) error {
		f, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644)
		if err != nil {
			return err
		}
		h := sha256.New()
		_, err = io.Copy(io.MultiWriter(f, h), r)
		if cerr := f.Close(); err == nil {
			err = cerr
		}
		if err != nil {
			return err
		}
		if hashKey != "" {
			if got, want := hex.EncodeToString(h.Sum(nil)), b.Manifest.SHA256[hashKey]; got != want {
				return fmt.Errorf("%s: %s sha256 mismatch: got %s, manifest says %s", b.Path, name, got, want)
			}
		}
		return nil
	})
}
