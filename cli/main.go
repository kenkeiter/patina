// Command patina builds, runs and controls patina unikernel images.
//
//	patina build <pkg> --target <vmm>-<arch> [--files src:dst ...] [--kernel path] -o out.patina
//	patina image inspect <image.patina>
//	patina run <image.patina> [--vmm qemu] [--mem 512] [--cpus 2] [--strace] [--timeout d] [-- args...]
//	patina ctl <vm> stop [--grace 30s] | signal SIGHUP | dump | ping | time
//	patina ps
package main

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"strings"
)

// version is overridden with -ldflags "-X main.version=...".
var version = "dev"

// Exit codes for CLI failures. 125 (like `docker run`) keeps CLI errors
// distinguishable from the guest's own exit codes.
const (
	exitUsage = 2
	exitError = 125
)

type usageError struct{ msg string }

func (u *usageError) Error() string { return u.msg }

func usagef(format string, a ...any) error { return &usageError{fmt.Sprintf(format, a...)} }

const usage = `patina — run static Go binaries on the patina unikernel

Usage:
  patina build <pkg> --target <vmm>-<arch> -o <out.patina> [--files src:dst ...] [--kernel path]
  patina image inspect <image.patina>
  patina run <image.patina> [--vmm qemu] [--mem 512] [--cpus 2] [--strace] [--timeout d] [-- args...]
  patina ctl <vm> stop [--grace 30s] | signal <SIG> | dump | ping | time
  patina ctl --sock <path> | --vsock <path> <op> ...
  patina ps
  patina version

Targets: firecracker, cloud-hypervisor, qemu, vz  ×  x86_64, aarch64 (vz: aarch64 only)
Run "patina <command> -h" for command flags.
`

func main() {
	os.Exit(dispatch(os.Args[1:], os.Stdout, os.Stderr))
}

func dispatch(args []string, stdout, stderr io.Writer) int {
	if len(args) == 0 {
		fmt.Fprint(stderr, usage)
		return exitUsage
	}
	var (
		code int
		err  error
	)
	switch args[0] {
	case "build":
		code, err = cmdBuild(args[1:], stdout, stderr)
	case "image":
		code, err = cmdImage(args[1:], stdout, stderr)
	case "run":
		code, err = cmdRun(args[1:], stdout, stderr)
	case "ctl":
		code, err = cmdCtl(args[1:], stdout, stderr)
	case "ps":
		code, err = cmdPs(args[1:], stdout, stderr)
	case "version", "--version":
		fmt.Fprintf(stdout, "patina %s\n", version)
		return 0
	case "help", "-h", "--help":
		fmt.Fprint(stdout, usage)
		return 0
	default:
		fmt.Fprintf(stderr, "patina: unknown command %q\n\n%s", args[0], usage)
		return exitUsage
	}
	if err != nil {
		var ue *usageError
		switch {
		case errors.Is(err, flag.ErrHelp):
			return 0
		case errors.As(err, &ue):
			fmt.Fprintf(stderr, "patina %s: %v\n", args[0], err)
			return exitUsage
		default:
			fmt.Fprintf(stderr, "patina %s: %v\n", args[0], err)
			return exitError
		}
	}
	return code
}

// parseArgs parses flags that may be interspersed with positional arguments
// (`patina build ./pkg --target x -o y`). Everything after the first "--" is
// returned untouched as passthrough.
func parseArgs(fs *flag.FlagSet, args []string) (positional, passthrough []string, err error) {
	for i, a := range args {
		if a == "--" {
			passthrough = args[i+1:]
			if passthrough == nil {
				passthrough = []string{}
			}
			args = args[:i]
			break
		}
	}
	for {
		if err := fs.Parse(args); err != nil {
			if errors.Is(err, flag.ErrHelp) {
				return nil, nil, err
			}
			return nil, nil, &usageError{err.Error()}
		}
		rest := fs.Args()
		if len(rest) == 0 {
			return positional, passthrough, nil
		}
		positional = append(positional, rest[0])
		args = rest[1:]
	}
}

// listFlag is a repeatable string flag.
type listFlag []string

func (l *listFlag) String() string     { return strings.Join(*l, ",") }
func (l *listFlag) Set(v string) error { *l = append(*l, v); return nil }

func newFlagSet(name string, stderr io.Writer, synopsis string) *flag.FlagSet {
	fs := flag.NewFlagSet("patina "+name, flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.Usage = func() {
		fmt.Fprintf(stderr, "Usage: %s\n\nFlags:\n", synopsis)
		fs.PrintDefaults()
	}
	return fs
}
