// httphello serves "hello from patina" on :8080 and shuts down gracefully on
// SIGTERM (Milestone A). With -selftest it requests itself over loopback,
// prints PASS/FAIL and exits, which needs no host networking.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"
)

const body = "hello from patina\n"

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	selftest := flag.Bool("selftest", false, "GET / over loopback, then exit")
	flag.Parse()

	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, body)
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
		resp, err := (&http.Client{Timeout: 10 * time.Second}).Get("http://127.0.0.1:" + port + "/")
		if err != nil {
			fmt.Printf("FAIL httphello: GET: %v\n", err)
			os.Exit(1)
		}
		b, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if resp.StatusCode != 200 || string(b) != body {
			fmt.Printf("FAIL httphello: status %d body %q\n", resp.StatusCode, b)
			os.Exit(1)
		}
		srv.Close()
		fmt.Println("PASS httphello (selftest)")
		return
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
