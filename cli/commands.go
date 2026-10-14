package main

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/signal"
	"syscall"
	"text/tabwriter"
	"time"

	"github.com/kenkeiter/patina/cli/internal/build"
	"github.com/kenkeiter/patina/cli/internal/cpio"
	"github.com/kenkeiter/patina/cli/internal/ctl"
	"github.com/kenkeiter/patina/cli/internal/image"
	"github.com/kenkeiter/patina/cli/internal/run"
	"github.com/kenkeiter/patina/cli/internal/runstate"
	"github.com/kenkeiter/patina/cli/internal/target"
	"github.com/kenkeiter/patina/cli/internal/vmm"
)

func cmdBuild(args []string, stdout, stderr io.Writer) (int, error) {
	fs := newFlagSet("build", stderr, "patina build <pkg> --target <vmm>-<arch> -o <out.patina> [flags]")
	tgt := fs.String("target", "", "target `vmm-arch`, e.g. qemu-aarch64, firecracker-x86_64 (required)")
	out := fs.String("o", "", "output `file` (.patina) (required)")
	kernel := fs.String("kernel", "", "kernel artifact `path` (default: $PATINA_KERNEL_DIR, then <repo>/out/)")
	name := fs.String("name", "", "image name (default: last element of the package path)")
	tags := fs.String("tags", "", "comma-separated Go build tags")
	verbose := fs.Bool("v", false, "show the go build invocation and output")
	var files listFlag
	fs.Var(&files, "files", "copy host `src:dst` into the image (repeatable; directories are recursive)")
	pos, pass, err := parseArgs(fs, args)
	if err != nil {
		return 0, err
	}
	if len(pos) != 1 || pass != nil {
		return 0, usagef("want exactly one package argument")
	}
	if *tgt == "" || *out == "" {
		return 0, usagef("--target and -o are required")
	}
	t, err := target.Parse(*tgt)
	if err != nil {
		return 0, usagef("%v", err)
	}
	opts := build.Options{Pkg: pos[0], Target: t, Files: files, Kernel: *kernel, Out: *out, Name: *name, Version: version}
	if *tags != "" {
		opts.GoFlags = []string{"-tags", *tags}
	}
	if *verbose {
		opts.Log = stderr
	}
	m, err := build.Run(opts)
	if err != nil {
		return 0, err
	}
	fmt.Fprintf(stdout, "built %s: %s for %s (%s, kernel %s)\n", *out, m.Name, m.Target, m.GoVersion, m.Kernel)
	return 0, nil
}

func cmdImage(args []string, stdout, stderr io.Writer) (int, error) {
	if len(args) == 0 || args[0] != "inspect" {
		return 0, usagef("usage: patina image inspect <image.patina> [--json]")
	}
	fs := newFlagSet("image inspect", stderr, "patina image inspect <image.patina> [--json]")
	asJSON := fs.Bool("json", false, "print the manifest as JSON only")
	pos, _, err := parseArgs(fs, args[1:])
	if err != nil {
		return 0, err
	}
	if len(pos) != 1 {
		return 0, usagef("want exactly one image")
	}
	b, err := image.Open(pos[0])
	if err != nil {
		return 0, err
	}
	m := b.Manifest
	if *asJSON {
		enc := json.NewEncoder(stdout)
		enc.SetIndent("", "  ")
		return 0, enc.Encode(m)
	}
	tw := tabwriter.NewWriter(stdout, 0, 4, 2, ' ', 0)
	fmt.Fprintf(tw, "name:\t%s\n", m.Name)
	fmt.Fprintf(tw, "target:\t%s\n", m.Target)
	fmt.Fprintf(tw, "go:\t%s\n", m.GoVersion)
	if m.GoPackage != "" {
		fmt.Fprintf(tw, "package:\t%s\n", m.GoPackage)
	}
	fmt.Fprintf(tw, "kernel:\t%s\n", m.Kernel)
	fmt.Fprintf(tw, "initrd:\t%s\n", m.Initrd)
	fmt.Fprintf(tw, "created:\t%s\n", m.Created.Format(time.RFC3339))
	if m.CLI != "" {
		fmt.Fprintf(tw, "cli:\t%s\n", m.CLI)
	}
	for _, k := range []string{"app", "kernel", "initrd"} {
		if v, ok := m.SHA256[k]; ok {
			fmt.Fprintf(tw, "sha256 %s:\t%s\n", k, v)
		}
	}
	tw.Flush()
	fmt.Fprintf(stdout, "\n%s:\n", m.Initrd)
	return 0, b.Member(m.Initrd, func(r io.Reader) error {
		cr := cpio.NewReader(r)
		tw := tabwriter.NewWriter(stdout, 0, 4, 2, ' ', 0)
		defer tw.Flush()
		for {
			h, err := cr.Next()
			if err == io.EOF {
				return nil
			}
			if err != nil {
				return err
			}
			extra := ""
			if h.IsSymlink() {
				t, err := io.ReadAll(io.LimitReader(cr, 4096))
				if err != nil {
					return err
				}
				extra = " -> " + string(t)
			}
			fmt.Fprintf(tw, "  %s\t%d:%d\t%10d\t%s\t/%s%s\n", h.FileMode(), h.UID, h.GID, h.Size,
				time.Unix(h.MTime, 0).UTC().Format("2006-01-02 15:04"), h.Name, extra)
		}
	})
}

func cmdRun(args []string, stdout, stderr io.Writer) (int, error) {
	fs := newFlagSet("run", stderr, "patina run <image.patina> [flags] [-- args...]")
	vmmName := fs.String("vmm", "", "VMM to run on (default: the image's target VMM)")
	vmmBin := fs.String("vmm-bin", "", "firecracker / cloud-hypervisor binary `path` (default: from PATH)")
	mem := fs.Int("mem", 512, "guest memory in `MiB`")
	cpus := fs.Int("cpus", 2, "guest vCPUs")
	strace := fs.Bool("strace", false, "trace guest syscalls to the console (patina.strace=1)")
	timeout := fs.Duration("timeout", 0, "stop the VM after this `duration` and exit 124 (0 = no limit)")
	grace := fs.Duration("grace", 10*time.Second, "grace period for interrupt/timeout stops")
	name := fs.String("name", "", "VM name for `patina ctl` (default <image>-<pid>)")
	accel := fs.String("accel", "", "QEMU accelerator: hvf, kvm or tcg (default: auto)")
	netMode := fs.String("net", "", "guest network: none, user (QEMU slirp), or a tap device name")
	ip := fs.String("ip", "", "static guest address with a tap device: `ADDR/PREFIX[,GATEWAY[,DNS]]` or a raw Linux ip= value (default: ip=dhcp)")
	verbose := fs.Bool("v", false, "log VMM argv and control events to stderr")
	var env, publish listFlag
	fs.Var(&env, "env", "guest environment `KEY=VALUE` (repeatable)")
	fs.Var(&publish, "publish", "forward host TCP `hostport:guestport` (with --net user; repeatable)")
	pos, pass, err := parseArgs(fs, args)
	if err != nil {
		return 0, err
	}
	if len(pos) != 1 {
		return 0, usagef("want exactly one image (pass guest args after --)")
	}
	if *name != "" {
		if err := runstate.ValidName(*name); err != nil {
			return 0, usagef("%v", err)
		}
	}
	sigs := make(chan os.Signal, 4)
	// SIGHUP too (terminal closed): QEMU runs in its own process group and
	// would otherwise outlive `patina run`.
	signal.Notify(sigs, os.Interrupt, syscall.SIGTERM, syscall.SIGHUP)
	defer signal.Stop(sigs)
	return run.Run(run.Options{
		Image: pos[0], VMM: *vmmName, VMMBin: *vmmBin, Name: *name, MemMB: *mem, CPUs: *cpus, Strace: *strace,
		Timeout: *timeout, Grace: *grace, Env: env, Args: pass, Accel: *accel,
		Net:     vmm.NetConfig{Mode: *netMode, Publish: publish, IP: *ip},
		Verbose: *verbose, Stdout: stdout, Stderr: stderr, Signals: sigs,
	})
}

func cmdCtl(args []string, stdout, stderr io.Writer) (int, error) {
	fs := newFlagSet("ctl", stderr, "patina ctl <vm> <op> [arg] [flags]  |  patina ctl --sock|--vsock <path> <op> [arg]\n\nOps: stop, signal <SIG>, dump, ping, time")
	grace := fs.Duration("grace", 30*time.Second, "stop: time the program gets to exit after SIGTERM before a forced stop")
	timeout := fs.Duration("timeout", 0, "how long to wait for a reply (default: ping 5s, stop grace+15s)")
	linger := fs.Duration("linger", 500*time.Millisecond, "signal/dump/time: how long to print events after sending")
	vsockPath := fs.String("vsock", "", "Firecracker/Cloud Hypervisor vsock Unix socket `path` (sends CONNECT <port>)")
	port := fs.Uint("port", ctl.Port, "guest vsock port for --vsock")
	sockPath := fs.String("sock", "", "raw NDJSON stream socket `path`")
	pos, pass, err := parseArgs(fs, args)
	if err != nil {
		return 0, err
	}
	if pass != nil {
		return 0, usagef("unexpected --")
	}
	var ep ctl.Endpoint
	switch {
	case *vsockPath != "" && *sockPath != "":
		return 0, usagef("--vsock and --sock are mutually exclusive")
	case *vsockPath != "":
		ep = ctl.Endpoint{Kind: ctl.KindFirecracker, Path: *vsockPath, Port: uint32(*port)}
	case *sockPath != "":
		ep = ctl.Endpoint{Kind: ctl.KindStream, Path: *sockPath}
	default:
		if len(pos) == 0 {
			return 0, usagef("want <vm> or --sock/--vsock")
		}
		st, err := runstate.Load(pos[0])
		if err != nil {
			return 0, err
		}
		ep = st.Control
		pos = pos[1:]
	}
	if len(pos) == 0 {
		return 0, usagef("missing op (stop, signal, dump, ping, time)")
	}
	op, opArgs := pos[0], pos[1:]
	wantArgs := 0
	if op == "signal" {
		wantArgs = 1
	}
	if len(opArgs) != wantArgs {
		return 0, usagef("%s: wrong number of arguments", op)
	}
	var send ctl.Op
	switch op {
	case "stop", "ping":
	case "signal":
		sig, err := ctl.NormalizeSignal(opArgs[0])
		if err != nil {
			return 0, usagef("%v", err)
		}
		send = ctl.Signal(sig)
	case "dump":
		send = ctl.Dump()
	case "time":
		send = ctl.Time(time.Now())
	default:
		return 0, usagef("unknown op %q (stop, signal, dump, ping, time)", op)
	}

	c, err := ctl.Dial(ep, 5*time.Second)
	if err != nil {
		return 0, err
	}
	defer c.Close()
	switch op {
	case "stop":
		t := *timeout
		if t == 0 {
			t = *grace + 15*time.Second
		}
		code, err := ctl.DoStop(c, *grace, t, stdout)
		if err != nil {
			return 0, err
		}
		return code, nil
	case "ping":
		t := *timeout
		if t == 0 {
			t = 5 * time.Second
		}
		_, err := ctl.DoPing(c, uint64(time.Now().UnixNano()&0xffffffff), t, stdout)
		return 0, err
	default:
		return 0, ctl.DoOneWay(c, send, *linger, stdout)
	}
}

func cmdPs(args []string, stdout, stderr io.Writer) (int, error) {
	if len(args) != 0 {
		return 0, usagef("usage: patina ps")
	}
	sts, err := runstate.List()
	if err != nil {
		return 0, err
	}
	tw := tabwriter.NewWriter(stdout, 0, 4, 2, ' ', 0)
	fmt.Fprintln(tw, "NAME\tVMM\tARCH\tPID\tADDR\tSTARTED\tIMAGE")
	for _, s := range sts {
		addr := "-"
		if s.Net != nil {
			addr = s.Net.Addr
		}
		fmt.Fprintf(tw, "%s\t%s\t%s\t%d\t%s\t%s\t%s\n", s.Name, s.VMM, s.Arch, s.PID, addr, s.Started.Local().Format(time.Stamp), s.Image)
	}
	return 0, tw.Flush()
}
