// Package fakevmm is a stand-in for the firecracker and cloud-hypervisor
// binaries, so `patina run --vmm firecracker|cloud-hypervisor` runs end to
// end in `go test` on hosts without KVM. It is test-only: nothing in the
// patina binary imports it.
//
// A test package calls MaybeRun from TestMain; Install then puts symlinks
// named "firecracker" and "cloud-hypervisor" to the test binary first on
// PATH. When the test binary is re-executed under one of those names,
// MaybeRun becomes that VMM:
//
//   - firecracker: serves the REST API on --api-sock (checking each PUT
//     against firecracker.yaml's required fields and ordering rules), and on
//     PUT /actions InstanceStart "boots" the guest.
//   - cloud-hypervisor: parses the CLI flags, serves GET /api/v1/vmm.ping on
//     --api-socket path=..., and boots the guest immediately.
//
// The fake guest prints to stdout (the serial console), listens on the
// vsock host socket with the CONNECT <port> / OK <hostport> handshake, and
// speaks NDJSON events per design §10. Every argv, API request, vsock
// handshake and op is appended as a JSON line to $PATINA_FAKEVMM_LOG.
//
// Behaviour is selected by environment variables (inherited from the test):
//
//	PATINA_FAKEVMM_SCENARIO  exit (default): ready event, console output, exit event, sentinel, power off
//	                         serial: the guest never accepts vsock; sentinel only
//	                         stop:   serve ping, wait for {"op":"stop"}, exit 143 (SIGTERM)
//	                         mute:   print one line and spin; never accepts vsock
//	                         silent: power off without reporting any exit code
//	PATINA_FAKEVMM_CODE      exit code for the exit event (default 0)
//	PATINA_FAKEVMM_SENTINEL  code printed in the "[patina] exit code N" line (default: CODE)
//	PATINA_FAKEVMM_REFUSE    number of CONNECTs to drop before the guest listens (default 0)
//	PATINA_FAKEVMM_FAIL      Firecracker API path to fail with 400 (e.g. /vsock)
package fakevmm

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

// Binary names the fake answers to.
const (
	Firecracker     = "firecracker"
	CloudHypervisor = "cloud-hypervisor"
)

// Environment variables (see the package comment).
const (
	EnvLog      = "PATINA_FAKEVMM_LOG"
	EnvScenario = "PATINA_FAKEVMM_SCENARIO"
	EnvCode     = "PATINA_FAKEVMM_CODE"
	EnvSentinel = "PATINA_FAKEVMM_SENTINEL"
	EnvRefuse   = "PATINA_FAKEVMM_REFUSE"
	EnvFail     = "PATINA_FAKEVMM_FAIL"
)

// Record is one line of the fake's log.
type Record struct {
	VMM    string          `json:"vmm"`
	Kind   string          `json:"kind"` // argv | api | connect | refused | op
	Argv   []string        `json:"argv,omitempty"`
	Method string          `json:"method,omitempty"`
	Path   string          `json:"path,omitempty"`
	Body   json.RawMessage `json:"body,omitempty"`
	Line   string          `json:"line,omitempty"`
}

func (r Record) String() string {
	switch r.Kind {
	case "argv":
		return "argv " + strings.Join(r.Argv, " ")
	case "api":
		if len(r.Body) > 0 {
			return r.Method + " " + r.Path + " " + string(r.Body)
		}
		return r.Method + " " + r.Path
	}
	return r.Kind + " " + r.Line
}

// MaybeRun turns the process into a fake VMM if it was invoked under one of
// the fake's names, and never returns in that case.
func MaybeRun() {
	switch filepath.Base(os.Args[0]) {
	case Firecracker:
		os.Exit(firecracker(os.Args[1:]))
	case CloudHypervisor:
		os.Exit(cloudHypervisor(os.Args[1:]))
	}
}

// Install links the running test binary into a fresh directory as
// "firecracker" and "cloud-hypervisor", prepends it to PATH, points
// PATINA_FAKEVMM_LOG at a fresh file, and returns (dir, log path). The test
// package's TestMain must call MaybeRun.
func Install(t testing.TB) (dir, log string) {
	t.Helper()
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	dir = t.TempDir()
	for _, n := range []string{Firecracker, CloudHypervisor} {
		if err := os.Symlink(exe, filepath.Join(dir, n)); err != nil {
			t.Fatal(err)
		}
	}
	t.Setenv("PATH", dir+string(os.PathListSeparator)+os.Getenv("PATH"))
	log = filepath.Join(dir, "fakevmm.log")
	t.Setenv(EnvLog, log)
	return dir, log
}

// ReadLog parses the fake's log.
func ReadLog(path string) ([]Record, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var out []Record
	for _, l := range strings.Split(strings.TrimSpace(string(b)), "\n") {
		if l == "" {
			continue
		}
		var r Record
		if err := json.Unmarshal([]byte(l), &r); err != nil {
			return nil, fmt.Errorf("fakevmm log line %q: %w", l, err)
		}
		out = append(out, r)
	}
	return out, nil
}

var logMu sync.Mutex

func record(r Record) {
	p := os.Getenv(EnvLog)
	if p == "" {
		return
	}
	b, _ := json.Marshal(r)
	logMu.Lock()
	defer logMu.Unlock()
	f, err := os.OpenFile(p, os.O_WRONLY|os.O_APPEND|os.O_CREATE, 0o600)
	if err != nil {
		return
	}
	f.Write(append(b, '\n'))
	f.Close()
}

func envInt(k string, def int) int {
	if v, err := strconv.Atoi(os.Getenv(k)); err == nil {
		return v
	}
	return def
}

func fatalf(format string, a ...any) int {
	fmt.Fprintf(os.Stderr, "fakevmm: "+format+"\n", a...)
	return 1
}

// ---- Firecracker ----

type fcState struct {
	mu        sync.Mutex
	boot      map[string]any
	machine   map[string]any
	vsockPath string
	started   bool
}

func firecracker(args []string) int {
	record(Record{VMM: Firecracker, Kind: "argv", Argv: args})
	var apiSock, logPath string
	for i := 0; i < len(args); i++ {
		a := args[i]
		val := func() string {
			if i+1 >= len(args) {
				return ""
			}
			i++
			return args[i]
		}
		switch a {
		case "--api-sock":
			apiSock = val()
		case "--log-path":
			logPath = val()
		case "--level":
			switch v := val(); strings.ToLower(v) {
			case "error", "warning", "info", "debug", "trace", "off":
			default:
				return fatalf("invalid --level %q", v)
			}
		default:
			return fatalf("firecracker: unknown argument %q", a)
		}
	}
	if apiSock == "" {
		return fatalf("firecracker: --api-sock is required")
	}
	if logPath != "" {
		// Firecracker opens the log path for writing; it must exist.
		f, err := os.OpenFile(logPath, os.O_WRONLY|os.O_APPEND, 0)
		if err != nil {
			return fatalf("firecracker: cannot open log path: %v", err)
		}
		fmt.Fprintf(f, "fake firecracker: logger configured\n")
		f.Close()
	}
	ln, err := net.Listen("unix", apiSock)
	if err != nil {
		return fatalf("firecracker: %v", err)
	}
	st := &fcState{}
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { fcHandle(st, w, r) })}
	go srv.Serve(ln)
	select {} // the guest goroutine exits the process
}

func fcFault(w http.ResponseWriter, code int, msg string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(map[string]string{"fault_message": msg})
}

func fcHandle(st *fcState, w http.ResponseWriter, r *http.Request) {
	body, _ := io.ReadAll(r.Body)
	rec := Record{VMM: Firecracker, Kind: "api", Method: r.Method, Path: r.URL.Path}
	if len(body) > 0 {
		rec.Body = json.RawMessage(body)
	}
	record(rec)
	if r.Method != http.MethodPut {
		fcFault(w, http.StatusBadRequest, "Invalid request method")
		return
	}
	if r.Header.Get("Content-Type") != "application/json" {
		fcFault(w, http.StatusBadRequest, "missing Content-Type: application/json")
		return
	}
	if fail := os.Getenv(EnvFail); fail != "" && fail == r.URL.Path {
		fcFault(w, http.StatusBadRequest, "fake failure injected for "+fail)
		return
	}
	var m map[string]any
	dec := json.NewDecoder(strings.NewReader(string(body)))
	dec.UseNumber()
	if err := dec.Decode(&m); err != nil {
		fcFault(w, http.StatusBadRequest, "An error occurred when deserializing the json body of a request: "+err.Error())
		return
	}
	st.mu.Lock()
	defer st.mu.Unlock()
	if st.started && r.URL.Path != "/actions" {
		fcFault(w, http.StatusBadRequest, "The requested operation is not supported after starting the microVM.")
		return
	}
	need := func(keys ...string) bool {
		for _, k := range keys {
			if _, ok := m[k]; !ok {
				fcFault(w, http.StatusBadRequest, "missing field `"+k+"`")
				return false
			}
		}
		return true
	}
	file := func(k string) bool {
		p, _ := m[k].(string)
		if _, err := os.Stat(p); err != nil {
			fcFault(w, http.StatusBadRequest, fmt.Sprintf("%s: %v", k, err))
			return false
		}
		return true
	}
	switch {
	case r.URL.Path == "/boot-source":
		if !need("kernel_image_path") || !file("kernel_image_path") {
			return
		}
		if _, ok := m["initrd_path"]; ok && !file("initrd_path") {
			return
		}
		st.boot = m
	case r.URL.Path == "/machine-config":
		if !need("vcpu_count", "mem_size_mib") {
			return
		}
		n, _ := m["vcpu_count"].(json.Number).Int64()
		if n < 1 || n > 32 || (n > 1 && n%2 != 0) {
			fcFault(w, http.StatusBadRequest, "The vCPU number is invalid! The vCPU number can only be 1 or an even number when SMT is enabled.")
			return
		}
		st.machine = m
	case r.URL.Path == "/vsock":
		if !need("guest_cid", "uds_path") {
			return
		}
		if c, _ := m["guest_cid"].(json.Number).Int64(); c < 3 {
			fcFault(w, http.StatusBadRequest, "invalid guest_cid")
			return
		}
		st.vsockPath, _ = m["uds_path"].(string)
	case strings.HasPrefix(r.URL.Path, "/network-interfaces/"):
		if !need("iface_id", "host_dev_name") {
			return
		}
		if m["iface_id"] != strings.TrimPrefix(r.URL.Path, "/network-interfaces/") {
			fcFault(w, http.StatusBadRequest, "The id from the path does not match the id from the body!")
			return
		}
	case r.URL.Path == "/entropy":
	case r.URL.Path == "/actions":
		if !need("action_type") {
			return
		}
		if m["action_type"] != "InstanceStart" {
			fcFault(w, http.StatusBadRequest, "unsupported action in fake: "+fmt.Sprint(m["action_type"]))
			return
		}
		if st.started {
			fcFault(w, http.StatusBadRequest, "The microVM is already running.")
			return
		}
		if st.boot == nil {
			fcFault(w, http.StatusBadRequest, "Cannot start microvm without kernel configuration.")
			return
		}
		st.started = true
		args, _ := st.boot["boot_args"].(string)
		vs := st.vsockPath
		var ln net.Listener
		if vs != "" {
			var err error
			// Firecracker creates uds_path when the device is built.
			if ln, err = net.Listen("unix", vs); err != nil {
				fcFault(w, http.StatusBadRequest, "vsock: "+err.Error())
				return
			}
		}
		go runGuest(Firecracker, args, ln)
	default:
		fcFault(w, http.StatusBadRequest, "Invalid request path")
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// ---- Cloud Hypervisor ----

// chOpts parses "k=v,k=v".
func chOpts(s string) map[string]string {
	m := map[string]string{}
	for _, kv := range strings.Split(s, ",") {
		k, v, _ := strings.Cut(kv, "=")
		m[k] = v
	}
	return m
}

func cloudHypervisor(args []string) int {
	record(Record{VMM: CloudHypervisor, Kind: "argv", Argv: args})
	flags := map[string]string{}
	known := map[string]bool{"--api-socket": true, "--kernel": true, "--initramfs": true, "--cmdline": true,
		"--cpus": true, "--memory": true, "--serial": true, "--console": true, "--vsock": true, "--rng": true, "--net": true}
	for i := 0; i < len(args); i++ {
		if !known[args[i]] || i+1 >= len(args) {
			return fatalf("cloud-hypervisor: unexpected argument %q", args[i])
		}
		flags[args[i]] = args[i+1]
		i++
	}
	for _, f := range []string{"--kernel", "--cmdline"} {
		if flags[f] == "" {
			return fatalf("cloud-hypervisor: %s is required by this fake", f)
		}
	}
	for _, f := range []string{"--kernel", "--initramfs"} {
		if p := flags[f]; p != "" {
			if _, err := os.Stat(p); err != nil {
				return fatalf("cloud-hypervisor: %s: %v", f, err)
			}
		}
	}
	if flags["--serial"] != "tty" {
		return fatalf("cloud-hypervisor: this fake only emulates --serial tty (got %q)", flags["--serial"])
	}
	if c := chOpts(flags["--cpus"])["boot"]; c == "" {
		return fatalf("cloud-hypervisor: --cpus boot= missing")
	}
	var apiLn net.Listener
	if a := flags["--api-socket"]; a != "" {
		p := chOpts(a)["path"]
		if p == "" {
			return fatalf("cloud-hypervisor: --api-socket wants path=<path>")
		}
		var err error
		if apiLn, err = net.Listen("unix", p); err != nil {
			return fatalf("cloud-hypervisor: %v", err)
		}
		mux := http.NewServeMux()
		mux.HandleFunc("/api/v1/", func(w http.ResponseWriter, r *http.Request) {
			body, _ := io.ReadAll(r.Body)
			rec := Record{VMM: CloudHypervisor, Kind: "api", Method: r.Method, Path: r.URL.Path}
			if len(body) > 0 {
				rec.Body = json.RawMessage(body)
			}
			record(rec)
			if r.Method == http.MethodGet && r.URL.Path == "/api/v1/vmm.ping" {
				w.Header().Set("Content-Type", "application/json")
				json.NewEncoder(w).Encode(map[string]any{"build_version": "fake", "version": "v54.0-fake", "pid": os.Getpid(), "features": []string{}})
				return
			}
			http.Error(w, "not emulated by fakevmm", http.StatusNotImplemented)
		})
		go (&http.Server{Handler: mux}).Serve(apiLn)
	}
	var vsLn net.Listener
	if v := flags["--vsock"]; v != "" {
		o := chOpts(v)
		if cid, _ := strconv.Atoi(o["cid"]); cid < 3 || o["socket"] == "" {
			return fatalf("cloud-hypervisor: --vsock wants cid=<n>=3+,socket=<path> (got %q)", v)
		}
		var err error
		if vsLn, err = net.Listen("unix", o["socket"]); err != nil {
			return fatalf("cloud-hypervisor: %v", err)
		}
	}
	runGuest(CloudHypervisor, flags["--cmdline"], vsLn)
	return 0
}

// ---- the fake guest ----

type guest struct {
	vmm      string
	scenario string
	code     int
	sentinel int
	refuse   int
	cmdline  string

	mu    sync.Mutex
	conn  net.Conn     // current control connection
	ln    net.Listener // vsock host socket
	ops   chan string
	conn1 chan struct{}
	once  sync.Once
}

// runGuest "boots" and, when the guest powers off, exits the process
// (status 0, as Firecracker and Cloud Hypervisor do on guest shutdown).
func runGuest(vmm, cmdline string, vsock net.Listener) {
	g := &guest{
		vmm:      vmm,
		scenario: os.Getenv(EnvScenario),
		code:     envInt(EnvCode, 0),
		refuse:   envInt(EnvRefuse, 0),
		cmdline:  cmdline,
		ops:      make(chan string, 16),
		conn1:    make(chan struct{}),
	}
	g.sentinel = envInt(EnvSentinel, g.code)
	if g.scenario == "" {
		g.scenario = "exit"
	}
	fmt.Printf("fake %s: booting with cmdline: %s\n", vmm, cmdline)
	listens := g.scenario == "exit" || g.scenario == "stop"
	if vsock != nil {
		g.ln = vsock
		go g.accept(vsock, listens)
	}
	deadline := time.After(60 * time.Second) // never outlive a broken test
	switch g.scenario {
	case "exit":
		select {
		case <-g.conn1:
		case <-deadline:
			fmt.Println("fake guest: control channel never connected")
			os.Exit(3)
		}
		g.send(`{"ev":"ready","go":"go-fake","cpus":1,"mem_mb":128}`)
		fmt.Println("hello from the fake guest")
		g.send(fmt.Sprintf(`{"ev":"exit","code":%d}`, g.code))
		g.poweroff(true)
	case "serial":
		fmt.Println("hello from the fake guest (no control channel)")
		g.poweroff(true)
	case "silent":
		fmt.Println("fake guest: powering off without an exit code")
		g.poweroff(false)
	case "mute":
		fmt.Println("fake guest: spinning")
		<-deadline
		os.Exit(3)
	case "stop":
		select {
		case <-g.conn1:
		case <-deadline:
			os.Exit(3)
		}
		g.send(`{"ev":"ready","go":"go-fake","cpus":1,"mem_mb":128}`)
		fmt.Println("fake guest: waiting for stop")
		for {
			select {
			case l := <-g.ops:
				var op struct {
					Op string  `json:"op"`
					ID *uint64 `json:"id"`
				}
				if json.Unmarshal([]byte(l), &op) != nil {
					continue
				}
				switch op.Op {
				case "ping":
					id := uint64(0)
					if op.ID != nil {
						id = *op.ID
					}
					g.send(fmt.Sprintf(`{"ev":"pong","id":%d,"uptime_ms":1}`, id))
				case "stop":
					fmt.Println("fake guest: got stop, exiting on SIGTERM")
					g.code, g.sentinel = 143, 143
					g.send(`{"ev":"exit","code":143,"signal":"SIGTERM"}`)
					g.poweroff(true)
				default:
					g.send(fmt.Sprintf(`{"ev":"error","msg":"unknown op %q"}`, op.Op))
				}
			case <-deadline:
				os.Exit(3)
			}
		}
	default:
		fmt.Fprintf(os.Stderr, "fakevmm: unknown scenario %q\n", g.scenario)
		os.Exit(1)
	}
}

func (g *guest) poweroff(sentinel bool) {
	if sentinel {
		fmt.Printf("[patina] exit code %d\n", g.sentinel)
	}
	g.mu.Lock()
	if g.ln != nil {
		g.ln.Close() // no new connections while "powering off"
	}
	if g.conn != nil {
		g.conn.Close()
	}
	g.mu.Unlock()
	os.Stdout.Sync()
	os.Exit(0)
}

func (g *guest) send(line string) {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.conn != nil {
		g.conn.Write([]byte(line + "\n"))
	}
}

// accept implements the host side of Firecracker's hybrid vsock: the host
// writes "CONNECT <port>\n"; if the guest listens on port the VMM answers
// "OK <host-port>\n", otherwise it closes the connection.
func (g *guest) accept(ln net.Listener, listens bool) {
	for hostPort := 1073741824; ; hostPort++ {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		r := bufio.NewReader(c)
		c.SetReadDeadline(time.Now().Add(5 * time.Second))
		line, err := r.ReadString('\n')
		c.SetReadDeadline(time.Time{})
		line = strings.TrimRight(line, "\n")
		if err != nil || !strings.HasPrefix(line, "CONNECT ") {
			record(Record{VMM: g.vmm, Kind: "refused", Line: line})
			c.Close()
			continue
		}
		port := strings.TrimPrefix(line, "CONNECT ")
		g.mu.Lock()
		refuse := !listens || port != "4242" || g.refuse > 0
		if g.refuse > 0 {
			g.refuse--
		}
		g.mu.Unlock()
		if refuse {
			record(Record{VMM: g.vmm, Kind: "refused", Line: line})
			c.Close()
			continue
		}
		record(Record{VMM: g.vmm, Kind: "connect", Line: line})
		g.mu.Lock()
		if g.conn != nil {
			g.conn.Close() // a new connection replaces the old one
		}
		g.conn = c
		fmt.Fprintf(c, "OK %d\n", hostPort)
		g.mu.Unlock()
		g.once.Do(func() { close(g.conn1) })
		go g.readOps(c, r)
	}
}

func (g *guest) readOps(c net.Conn, r *bufio.Reader) {
	for {
		l, err := r.ReadString('\n')
		if l = strings.TrimSpace(l); l != "" {
			record(Record{VMM: g.vmm, Kind: "op", Line: l})
			g.ops <- l
		}
		if err != nil {
			return
		}
	}
}
