// Package build implements `patina build`: compile a Go package for Linux,
// verify the result is loadable, pack it into an initrd and bundle it with a
// kernel.
package build

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"strings"
	"time"

	"github.com/kenkeiter/patina/cli/internal/cpio"
	"github.com/kenkeiter/patina/cli/internal/elfcheck"
	"github.com/kenkeiter/patina/cli/internal/image"
	"github.com/kenkeiter/patina/cli/internal/target"
)

// Options configure Run.
type Options struct {
	Pkg     string // package directory or import path
	Target  target.Target
	Files   []string // "src:dst" mappings
	Kernel  string   // explicit kernel path (optional)
	Out     string   // output .patina path
	Name    string   // image name (optional)
	GoFlags []string // extra `go build` flags, e.g. -tags
	Log     io.Writer
	Version string // CLI version recorded in the manifest
}

// FileMapping is one parsed --files entry.
type FileMapping struct{ Src, Dst string }

// reservedPrefixes are guest paths the kernel synthesizes.
var reservedPrefixes = []string{"proc", "sys", "dev"}

// ParseFileMapping parses "src:dst" (dst must be absolute in the guest).
func ParseFileMapping(s string) (FileMapping, error) {
	i := strings.LastIndex(s, ":/")
	if i <= 0 {
		return FileMapping{}, fmt.Errorf("invalid --files %q: want <host-path>:<absolute-guest-path>", s)
	}
	m := FileMapping{Src: s[:i], Dst: s[i+1:]}
	clean, err := cpio.CleanPath(m.Dst)
	if err != nil {
		return FileMapping{}, fmt.Errorf("invalid --files %q: %w", s, err)
	}
	first := strings.SplitN(clean, "/", 2)[0]
	for _, r := range reservedPrefixes {
		if first == r {
			return FileMapping{}, fmt.Errorf("invalid --files %q: /%s is provided by the kernel", s, r)
		}
	}
	if clean == "app" || strings.HasPrefix(clean, "app/") {
		return FileMapping{}, fmt.Errorf("invalid --files %q: /app is the program binary", s)
	}
	m.Dst = "/" + clean
	return m, nil
}

// RepoRoot finds the patina checkout containing dir (or one of its parents).
func RepoRoot(dir string) (string, bool) {
	for d := dir; ; d = filepath.Dir(d) {
		if _, err := os.Stat(filepath.Join(d, "doc", "patina-design.md")); err == nil {
			if _, err := os.Stat(filepath.Join(d, "cli", "go.mod")); err == nil {
				return d, true
			}
		}
		if filepath.Dir(d) == d {
			return "", false
		}
	}
}

// FindKernel resolves the kernel artifact for arch: explicit path, then
// $PATINA_KERNEL_DIR, then <repo>/out/ (searched from the working directory
// and from the patina executable's location).
func FindKernel(arch, explicit string) (string, error) {
	if explicit != "" {
		fi, err := os.Stat(explicit)
		if err != nil {
			return "", fmt.Errorf("--kernel: %w", err)
		}
		if !fi.Mode().IsRegular() {
			return "", fmt.Errorf("--kernel %s: not a regular file", explicit)
		}
		return explicit, nil
	}
	file := target.KernelFile(arch)
	var tried []string
	try := func(dir string) (string, bool) {
		p := filepath.Join(dir, file)
		tried = append(tried, p)
		if fi, err := os.Stat(p); err == nil && fi.Mode().IsRegular() {
			return p, true
		}
		return "", false
	}
	if d := os.Getenv("PATINA_KERNEL_DIR"); d != "" {
		if p, ok := try(d); ok {
			return p, nil
		}
	}
	var starts []string
	if wd, err := os.Getwd(); err == nil {
		starts = append(starts, wd)
	}
	if exe, err := os.Executable(); err == nil {
		if r, err := filepath.EvalSymlinks(exe); err == nil {
			exe = r
		}
		starts = append(starts, filepath.Dir(exe))
	}
	for _, s := range starts {
		if root, ok := RepoRoot(s); ok {
			if p, ok := try(filepath.Join(root, "out")); ok {
				return p, nil
			}
		}
	}
	// TODO: fetch versioned kernel release artifacts (design §12).
	return "", fmt.Errorf("no %s kernel found (looked in: %s); pass --kernel <path>, set PATINA_KERNEL_DIR, "+
		"or build one with scripts/build-kernel.sh %s (downloading release kernels is not implemented yet)",
		arch, strings.Join(tried, ", "), arch)
}

// GoBuild compiles pkg for linux/<arch> into out.
func GoBuild(pkg, arch, out string, extra []string, log io.Writer) error {
	if log == nil {
		log = io.Discard
	}
	goBin, err := exec.LookPath("go")
	if err != nil {
		return errors.New("go toolchain not found in PATH")
	}
	dir := ""
	arg := pkg
	// A directory argument is built from inside that directory so it resolves
	// against its own module, wherever the CLI is invoked from.
	if fi, err := os.Stat(pkg); err == nil && fi.IsDir() {
		dir, arg = pkg, "."
	}
	args := []string{"build", "-trimpath", "-ldflags=-extldflags=-static"}
	args = append(args, extra...)
	args = append(args, "-o", out, arg)
	cmd := exec.Command(goBin, args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), "GOOS=linux", "GOARCH="+target.GOARCH(arch), "CGO_ENABLED=0")
	var stderr bytes.Buffer
	cmd.Stdout = log
	cmd.Stderr = io.MultiWriter(log, &stderr)
	if log != io.Discard {
		fmt.Fprintf(log, "+ (cd %s) GOOS=linux GOARCH=%s CGO_ENABLED=0 go %s\n", orDot(dir), target.GOARCH(arch), strings.Join(args, " "))
	}
	if err := cmd.Run(); err != nil {
		if log == io.Discard && stderr.Len() > 0 {
			return fmt.Errorf("go build %s failed: %w\n%s", pkg, err, strings.TrimSpace(stderr.String()))
		}
		return fmt.Errorf("go build %s failed: %w", pkg, err)
	}
	return nil
}

func orDot(s string) string {
	if s == "" {
		return "."
	}
	return s
}

// Run performs the whole build.
func Run(o Options) (*image.Manifest, error) {
	if o.Log == nil {
		o.Log = io.Discard
	}
	if o.Out == "" {
		return nil, errors.New("-o <output.patina> is required")
	}
	maps := make([]FileMapping, 0, len(o.Files))
	for _, f := range o.Files {
		m, err := ParseFileMapping(f)
		if err != nil {
			return nil, err
		}
		if _, err := os.Lstat(m.Src); err != nil {
			return nil, fmt.Errorf("--files %s: %w", f, err)
		}
		maps = append(maps, m)
	}
	kernel, err := FindKernel(o.Target.Arch, o.Kernel)
	if err != nil {
		return nil, err
	}

	work, err := os.MkdirTemp("", "patina-build-")
	if err != nil {
		return nil, err
	}
	defer os.RemoveAll(work)

	app := filepath.Join(work, "app")
	if err := GoBuild(o.Pkg, o.Target.Arch, app, o.GoFlags, o.Log); err != nil {
		return nil, err
	}
	info, err := elfcheck.Check(app, target.ELFMachine(o.Target.Arch))
	if err != nil {
		return nil, fmt.Errorf("refusing to package %s: %w", o.Pkg, err)
	}

	b := cpio.NewBuilder()
	if err := b.AddHostFile("/app", app, 0o755); err != nil {
		return nil, err
	}
	for _, m := range maps {
		if err := b.AddTree(m.Src, m.Dst); err != nil {
			return nil, fmt.Errorf("--files %s:%s: %w", m.Src, m.Dst, err)
		}
	}
	initrd := filepath.Join(work, image.InitrdName)
	f, err := os.Create(initrd)
	if err != nil {
		return nil, err
	}
	if _, err := b.WriteTo(f); err != nil {
		f.Close()
		return nil, err
	}
	if err := f.Close(); err != nil {
		return nil, err
	}

	name := o.Name
	if name == "" {
		name = path.Base(info.Path)
		if name == "." || name == "/" || name == "" {
			name = strings.TrimSuffix(filepath.Base(o.Out), ".patina")
		}
	}
	appSum, err := image.FileSHA256(app)
	if err != nil {
		return nil, err
	}
	m := &image.Manifest{
		Name:      name,
		Arch:      o.Target.Arch,
		VMM:       o.Target.VMM,
		Target:    o.Target.String(),
		GoVersion: info.GoVersion,
		GoPackage: info.Path,
		SHA256:    map[string]string{"app": appSum},
		Created:   time.Now().UTC().Truncate(time.Second),
		CLI:       o.Version,
	}
	if err := image.Write(o.Out, m, kernel, initrd); err != nil {
		return nil, err
	}
	return m, nil
}
