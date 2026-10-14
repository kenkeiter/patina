// Package runstate manages the per-VM directory `patina run` writes so that
// `patina ctl <name>` can find the VM:
//
//	$XDG_RUNTIME_DIR/patina/<name>/   (or ~/.patina/run/<name>/)
//	  state.json   State, below
//	  ctl.sock     NDJSON control proxy served by `patina run`
//	  ...          VMM-private files (extracted kernel/initrd, VMM sockets)
package runstate

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"syscall"
	"time"

	"github.com/kenkeiter/patina/cli/internal/ctl"
)

// StateFile is the name of the state file in a run dir.
const StateFile = "state.json"

// ProxySocket is the name of the proxy socket in a run dir.
const ProxySocket = "ctl.sock"

// State is the content of state.json.
type State struct {
	Name     string       `json:"name"`
	PID      int          `json:"pid"`     // the `patina run` process
	VMMPID   int          `json:"vmm_pid"` // the VMM process, if any
	VMM      string       `json:"vmm"`
	Arch     string       `json:"arch"`
	Image    string       `json:"image"`
	Control  ctl.Endpoint `json:"control"`  // proxy socket (always KindStream)
	Upstream ctl.Endpoint `json:"upstream"` // the VMM's own control endpoint
	Started  time.Time    `json:"started"`
	// Net is the guest's DHCP lease, once `patina run` has seen it (the
	// kernel's lease log event or console line); absent before that and
	// for guests without ip=dhcp.
	Net *Net `json:"net,omitempty"`
}

// Net is the guest's IPv4 configuration as leased by DHCP.
type Net struct {
	Addr    string   `json:"addr"`              // e.g. "192.168.64.3"
	Prefix  int      `json:"prefix"`            // e.g. 24
	Gateway string   `json:"gateway,omitempty"` // e.g. "192.168.64.1"
	DNS     []string `json:"dns,omitempty"`
}

var nameRE = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`)

// ValidName reports whether name is usable as a VM name.
func ValidName(name string) error {
	if !nameRE.MatchString(name) {
		return fmt.Errorf("invalid VM name %q (letters, digits, . _ -; max 64)", name)
	}
	return nil
}

// Base returns the directory holding all run dirs.
func Base() (string, error) {
	if d := os.Getenv("XDG_RUNTIME_DIR"); d != "" {
		return filepath.Join(d, "patina"), nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".patina", "run"), nil
}

// Dir returns the run dir for name.
func Dir(name string) (string, error) {
	if err := ValidName(name); err != nil {
		return "", err
	}
	b, err := Base()
	if err != nil {
		return "", err
	}
	return filepath.Join(b, name), nil
}

// maxSunPath is the smallest sun_path among supported hosts (macOS: 104,
// including the NUL).
const maxSunPath = 103

// CheckSocketPath errors if p is too long to bind as a Unix socket.
func CheckSocketPath(p string) error {
	if len(p) > maxSunPath {
		return fmt.Errorf("socket path %s is %d bytes, over the %d-byte Unix socket limit; use a shorter --name or set XDG_RUNTIME_DIR to a short directory", p, len(p), maxSunPath)
	}
	return nil
}

// Create makes the run dir for name. It fails if a live VM already uses the
// name; a stale dir (owner process gone) is replaced.
func Create(name string) (string, error) {
	dir, err := Dir(name)
	if err != nil {
		return "", err
	}
	if st, err := Load(name); err == nil {
		if alive(st.PID) {
			return "", fmt.Errorf("a VM named %q is already running (pid %d)", name, st.PID)
		}
	}
	os.RemoveAll(dir)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	if err := CheckSocketPath(filepath.Join(dir, "qemu-ctl.sock")); err != nil {
		os.RemoveAll(dir)
		return "", err
	}
	return dir, nil
}

// Save writes state.json atomically.
func Save(dir string, st *State) error {
	b, err := json.MarshalIndent(st, "", "  ")
	if err != nil {
		return err
	}
	tmp := filepath.Join(dir, StateFile+".tmp")
	if err := os.WriteFile(tmp, append(b, '\n'), 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, filepath.Join(dir, StateFile))
}

// Load reads the state of the VM called name.
func Load(name string) (*State, error) {
	dir, err := Dir(name)
	if err != nil {
		return nil, err
	}
	b, err := os.ReadFile(filepath.Join(dir, StateFile))
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, fmt.Errorf("no running VM named %q (no %s)", name, filepath.Join(dir, StateFile))
		}
		return nil, err
	}
	st := &State{}
	if err := json.Unmarshal(b, st); err != nil {
		return nil, fmt.Errorf("%s: %w", filepath.Join(dir, StateFile), err)
	}
	return st, nil
}

// List returns the states of all run dirs with a live owner.
func List() ([]*State, error) {
	b, err := Base()
	if err != nil {
		return nil, err
	}
	ents, err := os.ReadDir(b)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var out []*State
	for _, e := range ents {
		if !e.IsDir() || ValidName(e.Name()) != nil {
			continue
		}
		if st, err := Load(e.Name()); err == nil && alive(st.PID) {
			out = append(out, st)
		}
	}
	return out, nil
}

func alive(pid int) bool {
	if pid <= 0 {
		return false
	}
	err := syscall.Kill(pid, 0)
	return err == nil || errors.Is(err, syscall.EPERM)
}
