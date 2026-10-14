//go:build darwin && arm64 && cgo

package vmm

import (
	"bytes"
	"context"
	"debug/elf"
	"encoding/binary"
	"fmt"
	"io"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/kenkeiter/patina/cli/internal/target"
)

func TestVZLimits(t *testing.T) {
	minC, maxC, minM, maxM := VZLimits()
	t.Logf("VZ limits: cpus [%d, %d], memory [%d MiB, %d MiB]", minC, maxC, minM>>20, maxM>>20)
	if minC < 1 || maxC < minC || minM == 0 || maxM < minM {
		t.Fatalf("implausible limits")
	}
}

// TestVZValidateConfig builds the full VZVirtualMachineConfiguration (boot
// loader, console, vsock, entropy, NAT) and runs VZ's own validation.
func TestVZValidateConfig(t *testing.T) {
	d := t.TempDir()
	k, i := filepath.Join(d, "Image"), filepath.Join(d, "initrd")
	os.WriteFile(k, probeImageOrFake(t), 0o644)
	os.WriteFile(i, []byte("070701"), 0o644)
	for _, nat := range []bool{false, true} {
		spec := &VZSpec{Kernel: k, Initrd: i, Cmdline: "console=hvc0 --", CPUs: 2, MemBytes: 512 << 20, NAT: nat, VsockPort: 4242}
		if err := vzValidate(spec); err != nil {
			if strings.Contains(err.Error(), "entitlement") {
				t.Skipf("test binary lacks the virtualization entitlement: %v", err)
			}
			t.Fatalf("nat=%v: %v", nat, err)
		}
	}
	spec := &VZSpec{Kernel: k, Initrd: i, Cmdline: "--", CPUs: 100000, MemBytes: 512 << 20}
	if err := vzValidate(spec); err == nil {
		t.Fatal("absurd CPU count validated")
	}
}

// probeImageOrFake returns the DTB probe Image if clang is available, else
// any bytes (validation does not parse the kernel).
func probeImageOrFake(t *testing.T) []byte {
	img, err := buildProbe(t.TempDir())
	if err != nil {
		return bytes.Repeat([]byte{0}, 4096)
	}
	return img.bytes(probeParams{mask: 0xff, unitMs: 1})
}

// TestVZDialAndKill boots the probe kernel spinning for ~4 s (no vsock
// listener), checks that the control-channel dialer fails promptly instead
// of hanging, then force-stops the VM.
func TestVZDialAndKill(t *testing.T) {
	img, err := buildProbe(t.TempDir())
	if err != nil {
		t.Skip(err)
	}
	d := t.TempDir()
	k, i := filepath.Join(d, "Image"), filepath.Join(d, "initrd")
	// v = DTB byte 0 (0xd0 = 208) * 20 ms.
	os.WriteFile(k, img.bytes(probeParams{mode: 2, mask: 0xff, unitMs: 20}), 0o644)
	os.WriteFile(i, []byte("x"), 0o644)
	var serial bytes.Buffer
	cfg := &Config{Arch: target.AArch64, Kernel: k, Initrd: i, Cmdline: "console=hvc0 --", MemMB: 256, CPUs: 1,
		RunDir: d, Serial: &serial, Stderr: os.Stderr}
	m, err := (&VZ{}).Start(context.Background(), cfg)
	if err != nil {
		if strings.Contains(err.Error(), "entitlement") {
			t.Skipf("test binary lacks the virtualization entitlement (use cli/scripts/vztest.sh): %v", err)
		}
		t.Fatal(err)
	}
	ep := m.Control()
	if ep.Kind != "vz-vsock" || ep.Dialer == nil || m.PID() != 0 {
		t.Fatalf("endpoint %+v", ep)
	}
	for range 3 {
		t0 := time.Now()
		c, err := ep.Dialer(time.Second)
		if err == nil {
			c.Close()
			t.Fatal("vsock connect succeeded with no guest listener")
		}
		t.Logf("dial: %v (%s)", err, time.Since(t0).Round(time.Millisecond))
		if time.Since(t0) > 1500*time.Millisecond {
			t.Fatal("dial did not honour its timeout")
		}
	}
	t0 := time.Now()
	m.Kill()
	ex, err := m.Wait()
	if err != nil || ex.Code != -1 {
		t.Fatalf("after Kill: %+v %v", ex, err)
	}
	t.Logf("killed in %s", time.Since(t0).Round(time.Millisecond))
	if _, err := ep.Dialer(time.Second); err == nil {
		t.Fatal("dial after stop succeeded")
	}
}

// ---- DTB probe -------------------------------------------------------------

type probeImage struct {
	code      []byte
	paramsOff int
}

type probeParams struct {
	mode, offset, shift, mask, unitMs uint64
	needle                            string
}

// buildProbe assembles testdata/vzprobe.S with clang and extracts .text.
func buildProbe(dir string) (*probeImage, error) {
	obj := filepath.Join(dir, "vzprobe.o")
	out, err := exec.Command("clang", "-target", "aarch64-none-elf", "-c", "testdata/vzprobe.S", "-o", obj).CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("clang: %v\n%s", err, out)
	}
	f, err := elf.Open(obj)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	text := f.Section(".text")
	if text == nil {
		return nil, fmt.Errorf("no .text")
	}
	code, err := text.Data()
	if err != nil {
		return nil, err
	}
	syms, err := f.Symbols()
	if err != nil {
		return nil, err
	}
	for _, s := range syms {
		if s.Name == "params" {
			return &probeImage{code: code, paramsOff: int(s.Value)}, nil
		}
	}
	return nil, fmt.Errorf("no params symbol")
}

// bytes renders a Linux arm64 Image with p patched in.
func (pi *probeImage) bytes(p probeParams) []byte {
	body := append([]byte(nil), pi.code...)
	o := pi.paramsOff
	for i, v := range []uint64{p.mode, p.offset, p.shift, p.mask, p.unitMs} {
		binary.LittleEndian.PutUint64(body[o+8*i:], v)
	}
	n := body[o+40:]
	if len(p.needle) >= len(n) {
		panic("needle too long")
	}
	for i := range n {
		n[i] = 0
	}
	copy(n, p.needle)
	for len(body)%8 != 0 {
		body = append(body, 0)
	}
	hdr := make([]byte, 64)
	binary.LittleEndian.PutUint32(hdr[0:], 0x14000000|64/4) // b +64
	binary.LittleEndian.PutUint64(hdr[16:], uint64(64+len(body)+4096))
	binary.LittleEndian.PutUint64(hdr[24:], 0xA)
	copy(hdr[56:], "ARM\x64")
	return append(hdr, body...)
}

type prober struct {
	t    *testing.T
	img  *probeImage
	dir  string
	base time.Duration // measured duration for v = 0
	unit time.Duration
	net  bool
	mu   sync.Mutex
	seq  int
}

// boot runs one probe and returns start -> guest stop.
func (pr *prober) boot(p probeParams) (time.Duration, error) {
	pr.mu.Lock()
	pr.seq++
	dir := filepath.Join(pr.dir, fmt.Sprint(pr.seq))
	pr.mu.Unlock()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return 0, err
	}
	defer os.RemoveAll(dir)
	k := filepath.Join(dir, "Image")
	i := filepath.Join(dir, "initrd")
	if err := os.WriteFile(k, pr.img.bytes(p), 0o644); err != nil {
		return 0, err
	}
	os.WriteFile(i, []byte("probe"), 0o644)
	cfg := &Config{Arch: target.AArch64, Kernel: k, Initrd: i, Cmdline: "console=hvc0 patina.epoch=1 --",
		MemMB: 512, CPUs: 2, RunDir: dir, Serial: io.Discard, Stderr: os.Stderr}
	if pr.net {
		cfg.Net.Mode = "user"
	}
	spec, err := VZSpecFor(cfg)
	if err != nil {
		return 0, err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	m, err := vzStart(ctx, cfg, spec)
	if err != nil {
		return 0, err
	}
	t0 := time.Now()
	ex, err := m.Wait()
	d := time.Since(t0)
	if err != nil {
		return 0, err
	}
	if ex.Code != 0 {
		return 0, fmt.Errorf("probe VM did not stop cleanly (code %d)", ex.Code)
	}
	return d, nil
}

// value runs p and decodes the timing into v.
func (pr *prober) value(p probeParams) (int, error) {
	p.unitMs = uint64(pr.unit / time.Millisecond)
	d, err := pr.boot(p)
	if err != nil {
		return 0, err
	}
	f := float64(d-pr.base) / float64(pr.unit)
	v := int(math.Round(f))
	if math.Abs(f-float64(v)) > 0.3 {
		return v, fmt.Errorf("ambiguous timing %.2f units (%s)", f, d)
	}
	return v, nil
}

func (pr *prober) count(needle string) int {
	v, err := pr.value(probeParams{mode: 0, mask: 0xff, needle: needle})
	if err != nil {
		pr.t.Fatalf("count %q: %v", needle, err)
	}
	return v
}

// TestVZProbeDTB learns what the device tree VZ hands the guest contains by
// timing a probe kernel (see testdata/vzprobe.S): it reads the whole blob a
// nibble per boot, decodes it, and scans PCI ECAM space for devices.
// Opt-in: PATINA_VZ_PROBE=1 cli/scripts/vztest.sh -test.run TestVZProbeDTB -test.v
// PATINA_VZ_PROBE_OUT=<file> saves the raw DTB; PATINA_VZ_PROBE_NET=1 adds
// the NAT NIC.
func TestVZProbeDTB(t *testing.T) {
	if os.Getenv("PATINA_VZ_PROBE") == "" {
		t.Skip("set PATINA_VZ_PROBE=1 (and run via cli/scripts/vztest.sh)")
	}
	img, err := buildProbe(t.TempDir())
	if err != nil {
		t.Skip(err)
	}
	pr := &prober{t: t, img: img, dir: t.TempDir(), unit: 20 * time.Millisecond, net: os.Getenv("PATINA_VZ_PROBE_NET") != ""}
	var ds []time.Duration
	for range 5 {
		d, err := pr.boot(probeParams{mode: 0, mask: 0xff, unitMs: 1, needle: "\x01nomatch"})
		if err != nil {
			if strings.Contains(err.Error(), "entitlement") {
				t.Skipf("test binary lacks the virtualization entitlement: %v", err)
			}
			t.Fatal(err)
		}
		ds = append(ds, d)
	}
	slices.Sort(ds)
	pr.base = ds[len(ds)/2] // median: an occasional boot is ~15ms slow
	t.Logf("baseline boot->stop (v=0): %v", ds)

	for _, n := range strings.Split(os.Getenv("PATINA_VZ_PROBE_NEEDLES"), "|") {
		if n != "" {
			t.Logf("count %-32q %d", n, pr.count(n))
		}
	}

	// totalsize, then every byte as two nibbles, in parallel.
	hdr := pr.readBytes(0, 8)
	size := int(binary.BigEndian.Uint32(hdr[4:]))
	if binary.BigEndian.Uint32(hdr) != 0xd00dfeed || size < 40 || size > 1<<20 {
		t.Fatalf("bad FDT header % x", hdr)
	}
	t0 := time.Now()
	blob := pr.readBytes(0, size)
	// Second pass; re-read disagreeing bytes until two reads agree.
	again := pr.readBytes(0, size)
	var volatile []int
	for i := range blob {
		// A few bytes differ on every boot (random seeds): give up on a
		// byte after a handful of disagreeing reads and report it.
		for n := 0; blob[i] != again[i]; n++ {
			if n == 3 {
				volatile = append(volatile, i)
				break
			}
			blob[i], again[i] = again[i], pr.readBytes(i, 1)[0]
		}
	}
	t.Logf("bytes that change between boots: %v", volatile)
	t.Logf("read %d-byte DTB twice in %s", size, time.Since(t0).Round(time.Second))
	if out := os.Getenv("PATINA_VZ_PROBE_OUT"); out != "" {
		os.WriteFile(out, blob, 0o644)
	}
	tree, err := decodeFDT(blob)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	t.Logf("device tree:\n%s", tree)

	// PCI: scan bus 0 of the ECAM window named by the DTB.
	if base := os.Getenv("PATINA_VZ_PROBE_ECAM"); base != "" {
		var ecam uint64
		fmt.Sscanf(base, "0x%x", &ecam)
		for dev := uint64(0); dev < 32; dev++ {
			a := ecam + dev<<15
			id := pr.readWord(a)
			if id == 0xffffffff || id == 0 {
				continue
			}
			class := pr.readWord(a + 8)
			sub := pr.readWord(a + 0x2c)
			t.Logf("pci 00:%02x.0 vendor %04x device %04x class %06x rev %02x subsys %08x",
				dev, id&0xffff, id>>16, class>>8, class&0xff, sub)
		}
	}
}

// readBytes reads n DTB bytes from off, a nibble per boot, 4 boots at a time.
func (pr *prober) readBytes(off, n int) []byte {
	out := make([]byte, n)
	type job struct{ i, shift int }
	jobs := make(chan job)
	errs := make(chan error, 1)
	done := make(chan struct{})
	const par = 4
	for w := 0; w < par; w++ {
		go func() {
			defer func() { done <- struct{}{} }()
			for j := range jobs {
				v, err := pr.retry(probeParams{mode: 2, offset: uint64(off + j.i), shift: uint64(j.shift), mask: 0xf})
				if err != nil {
					select {
					case errs <- err:
					default:
					}
					continue
				}
				pr.mu.Lock()
				out[j.i] |= byte(v) << j.shift
				pr.mu.Unlock()
			}
		}()
	}
	for i := 0; i < n; i++ {
		jobs <- job{i, 0}
		jobs <- job{i, 4}
	}
	close(jobs)
	for w := 0; w < par; w++ {
		<-done
	}
	select {
	case err := <-errs:
		pr.t.Fatal(err)
	default:
	}
	return out
}

// readWord reads a 32-bit physical word (mode 3), a nibble per boot.
func (pr *prober) readWord(addr uint64) uint32 {
	var w uint32
	for sh := 0; sh < 32; sh += 4 {
		v, err := pr.retry(probeParams{mode: 3, offset: addr, shift: uint64(sh), mask: 0xf})
		if err != nil {
			pr.t.Fatalf("read %#x: %v", addr, err)
		}
		w |= uint32(v) << sh
	}
	return w
}

func (pr *prober) retry(p probeParams) (int, error) {
	var err error
	for range 12 {
		var v int
		if v, err = pr.value(p); err == nil && v >= 0 && v <= int(p.mask) {
			return v, nil
		}
	}
	return 0, err
}

// decodeFDT renders a flattened device tree as dts-like text.
func decodeFDT(b []byte) (string, error) {
	be := binary.BigEndian
	if len(b) < 40 || be.Uint32(b) != 0xd00dfeed {
		return "", fmt.Errorf("not an FDT")
	}
	offStruct, offStrings := be.Uint32(b[8:]), be.Uint32(b[12:])
	cstr := func(o int) string {
		e := bytes.IndexByte(b[o:], 0)
		if e < 0 {
			return ""
		}
		return string(b[o : o+e])
	}
	var sb strings.Builder
	depth := 0
	ind := func() string { return strings.Repeat("  ", depth) }
	for p := int(offStruct); p+4 <= len(b); {
		tok := be.Uint32(b[p:])
		p += 4
		switch tok {
		case 1: // BEGIN_NODE
			name := cstr(p)
			p += (len(name) + 4) &^ 3
			if name == "" {
				name = "/"
			}
			fmt.Fprintf(&sb, "%s%s {\n", ind(), name)
			depth++
		case 2: // END_NODE
			depth--
			fmt.Fprintf(&sb, "%s};\n", ind())
		case 3: // PROP
			ln, no := int(be.Uint32(b[p:])), int(be.Uint32(b[p+4:]))
			p += 8
			v := b[p : p+ln]
			p += (ln + 3) &^ 3
			fmt.Fprintf(&sb, "%s%s%s;\n", ind(), cstr(int(offStrings)+no), fdtValue(v))
		case 4: // NOP
		case 9: // END
			return sb.String(), nil
		default:
			return sb.String(), fmt.Errorf("bad token %#x at %d", tok, p-4)
		}
	}
	return sb.String(), fmt.Errorf("no FDT_END")
}

func fdtValue(v []byte) string {
	if len(v) == 0 {
		return ""
	}
	printable := v[len(v)-1] == 0
	for _, c := range v[:len(v)-1] {
		if c != 0 && (c < 0x20 || c > 0x7e) {
			printable = false
		}
	}
	if printable && v[0] != 0 {
		parts := strings.Split(string(v[:len(v)-1]), "\x00")
		return ` = "` + strings.Join(parts, `", "`) + `"`
	}
	if len(v)%4 == 0 {
		var w []string
		for i := 0; i < len(v); i += 4 {
			w = append(w, fmt.Sprintf("0x%x", binary.BigEndian.Uint32(v[i:])))
		}
		return " = <" + strings.Join(w, " ") + ">"
	}
	return fmt.Sprintf(" = [% x]", v)
}
