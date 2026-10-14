// httphello serves "hello from patina" on :8080 and shuts down gracefully on
// SIGTERM (Milestone A). With -selftest it requests itself over loopback,
// resolves names through Go's resolver, prints PASS/FAIL and exits, which
// needs no host networking (except -dns).
//
// Handlers: / (the greeting), /big (a deterministic 4 MiB body, see bigByte),
// /echo (the request body back).
//
// Flags: -nohandler installs no signal handler, so SIGTERM kills the
// process (exit status 143 by SIG_DFL); -dns <name> adds a lookup through
// /etc/resolv.conf's nameserver to -selftest.
package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"
)

const body = "hello from patina\n"

// bigSize is the length of /big.
const bigSize = 4 << 20

// bigByte is byte i of /big. scripts/test-net.sh computes the same.
func bigByte(i int) byte { return byte(i*31 + i>>12) }

func bigBody() []byte {
	b := make([]byte, bigSize)
	for i := range b {
		b[i] = bigByte(i)
	}
	return b
}

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	selftest := flag.Bool("selftest", false, "GET / over loopback, resolve names, then exit")
	nohandler := flag.Bool("nohandler", false, "no SIGTERM handler (SIGTERM kills: exit 143)")
	dns := flag.String("dns", "", "with -selftest: also resolve this name via resolv.conf's nameserver")
	flag.Parse()

	big := bigBody()
	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, body)
	})
	mux.HandleFunc("/big", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/octet-stream")
		w.Write(big)
	})
	mux.HandleFunc("/echo", func(w http.ResponseWriter, r *http.Request) {
		io.Copy(w, r.Body)
	})
	srv := &http.Server{Handler: mux, ReadHeaderTimeout: 10 * time.Second}

	ln, err := net.Listen("tcp", *addr)
	if err != nil {
		fmt.Printf("FAIL httphello: listen: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("httphello: listening on %s\n", ln.Addr())

	errc := make(chan error, 1)
	go func() { errc <- srv.Serve(ln) }()

	if *selftest {
		_, port, _ := net.SplitHostPort(ln.Addr().String())
		if err := runSelftest("http://127.0.0.1:"+port, big, *dns); err != nil {
			fmt.Printf("FAIL httphello: %v\n", err)
			os.Exit(1)
		}
		srv.Close()
		fmt.Println("PASS httphello (selftest)")
		return
	}

	if *nohandler {
		fmt.Println("httphello: no signal handler")
		err := <-errc
		fmt.Printf("FAIL httphello: serve: %v\n", err)
		os.Exit(1)
	}

	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, syscall.SIGTERM, syscall.SIGINT)
	select {
	case s := <-sigs:
		fmt.Printf("httphello: got %v, shutting down\n", s)
	case err := <-errc:
		fmt.Printf("FAIL httphello: serve: %v\n", err)
		os.Exit(1)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := srv.Shutdown(ctx); err != nil {
		fmt.Printf("FAIL httphello: shutdown: %v\n", err)
		os.Exit(1)
	}
	if err := <-errc; !errors.Is(err, http.ErrServerClosed) {
		fmt.Printf("FAIL httphello: serve returned %v\n", err)
		os.Exit(1)
	}
	fmt.Println("PASS httphello (graceful shutdown)")
}

func runSelftest(base string, big []byte, dns string) error {
	c := &http.Client{Timeout: 10 * time.Second}
	get := func(path string) ([]byte, error) {
		resp, err := c.Get(base + path)
		if err != nil {
			return nil, fmt.Errorf("GET %s: %v", path, err)
		}
		defer resp.Body.Close()
		b, err := io.ReadAll(resp.Body)
		if err != nil {
			return nil, fmt.Errorf("GET %s: read: %v", path, err)
		}
		if resp.StatusCode != 200 {
			return nil, fmt.Errorf("GET %s: status %d", path, resp.StatusCode)
		}
		return b, nil
	}
	b, err := get("/")
	if err != nil {
		return err
	}
	if string(b) != body {
		return fmt.Errorf("GET /: body %q", b)
	}
	fmt.Println("ok GET /")
	b, err = get("/big")
	if err != nil {
		return err
	}
	if !bytes.Equal(b, big) {
		return fmt.Errorf("GET /big: %d bytes, mismatch", len(b))
	}
	fmt.Printf("ok GET /big %d bytes sha256 %x\n", len(b), sha256.Sum256(b))
	resp, err := c.Post(base+"/echo", "text/plain", strings.NewReader("ping patina"))
	if err != nil {
		return fmt.Errorf("POST /echo: %v", err)
	}
	b, _ = io.ReadAll(resp.Body)
	resp.Body.Close()
	if string(b) != "ping patina" {
		return fmt.Errorf("POST /echo: body %q", b)
	}
	fmt.Println("ok POST /echo")

	lookup := func(name string, want func([]string) bool) error {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		addrs, err := net.DefaultResolver.LookupHost(ctx, name)
		if err != nil {
			return fmt.Errorf("lookup %s: %v", name, err)
		}
		if want != nil && !want(addrs) {
			return fmt.Errorf("lookup %s: unexpected %v", name, addrs)
		}
		fmt.Printf("ok dns %s -> %s\n", name, strings.Join(addrs, " "))
		return nil
	}
	if err := lookup("localhost", func(a []string) bool {
		for _, s := range a {
			if s == "127.0.0.1" {
				return true
			}
		}
		return false
	}); err != nil {
		return err
	}
	host, err := os.Hostname()
	if err != nil {
		return fmt.Errorf("hostname: %v", err)
	}
	if err := lookup(host, nil); err != nil {
		return err
	}
	if dns != "" {
		if err := lookup(dns, nil); err != nil {
			return err
		}
	}
	return nil
}
