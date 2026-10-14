// vdsobench checks and times the clock and getrandom paths Go takes
// through the vDSO (decision D75), against the real syscalls:
//
//   - 1e6 time.Now() calls are monotonic; ns/call is printed;
//   - runtime.nanotime agrees with clock_gettime(CLOCK_MONOTONIC) (syscall)
//     within a few µs, time.Now() with clock_gettime(CLOCK_REALTIME) within
//     1 ms;
//   - time.Since over a 100 ms sleep is 100-150 ms;
//   - 1e5 crypto/rand reads of 32 bytes are timed; output is not all zero
//     and two reads differ;
//   - /proc/self/maps has a [vdso] line, or none when the environment says
//     VDSOBENCH_EXPECT=novdso (run with patina.novdso=1).
//
// The timed loops are bracketed by "BEGIN <loop>" / "END <loop>" lines so a
// patina.strace=1 log can be checked for syscalls inside them.
//
// Prints "PASS vdsobench" and exits 0 on success.
package main

import (
	"bytes"
	"crypto/rand"
	"fmt"
	"os"
	"runtime"
	"strings"
	"syscall"
	"time"
	"unsafe"
)

//go:linkname nanotime runtime.nanotime
func nanotime() int64

const (
	clockRealtime  = 0
	clockMonotonic = 1
)

func sysClock(id int) int64 {
	var ts syscall.Timespec
	_, _, e := syscall.RawSyscall(syscall.SYS_CLOCK_GETTIME, uintptr(id), uintptr(unsafe.Pointer(&ts)), 0)
	if e != 0 {
		fail("clock_gettime(%d): %v", id, e)
	}
	return ts.Nano()
}

// getrandom's syscall number (the syscall package has it only on arm64).
func sysGetrandom() uintptr {
	if runtime.GOARCH == "amd64" {
		return 318
	}
	return 278
}

func fail(f string, a ...any) {
	fmt.Printf("FAIL vdsobench: "+f+"\n", a...)
	os.Exit(1)
}

func main() {
	expectVDSO := os.Getenv("VDSOBENCH_EXPECT") != "novdso"

	// /proc/self/maps.
	maps, err := os.ReadFile("/proc/self/maps")
	if err != nil {
		fail("read /proc/self/maps: %v", err)
	}
	hasVDSO := false
	for _, l := range strings.Split(string(maps), "\n") {
		if strings.HasSuffix(l, " [vdso]") || strings.HasSuffix(l, " [vvar]") {
			fmt.Println("maps:", l)
		}
		if strings.HasSuffix(l, " [vdso]") {
			hasVDSO = true
		}
	}
	if hasVDSO != expectVDSO {
		fail("[vdso] in /proc/self/maps: %v, want %v", hasVDSO, expectVDSO)
	}

	// Monotonic clock vs the syscall.
	var worst int64
	for i := 0; i < 1000; i++ {
		s1 := sysClock(clockMonotonic)
		v := nanotime()
		s2 := sysClock(clockMonotonic)
		const slack = 5000 // 5 µs: TSC/CNTVCT skew between CPUs
		if v < s1-slack || v > s2+slack {
			fail("nanotime %d not within syscall [%d, %d]", v, s1, s2)
		}
		if d := s2 - s1; d > worst {
			worst = d
		}
	}
	fmt.Printf("monotonic: nanotime within syscall bracket (1000 checks, widest bracket %d ns)\n", worst)

	// Wall clock vs the syscall.
	for i := 0; i < 1000; i++ {
		s1 := sysClock(clockRealtime)
		v := time.Now().UnixNano()
		s2 := sysClock(clockRealtime)
		if v < s1-1_000_000 || v > s2+1_000_000 {
			fail("time.Now %d not within 1 ms of syscall [%d, %d]", v, s1, s2)
		}
	}
	fmt.Println("realtime: time.Now within 1 ms of clock_gettime(CLOCK_REALTIME) (1000 checks)")

	// 1e6 time.Now(): monotonic, timed.
	const nNow = 1_000_000
	fmt.Println("BEGIN now-loop")
	start := time.Now()
	prev := start
	for i := 0; i < nNow; i++ {
		t := time.Now()
		if t.Sub(prev) < 0 {
			fail("time.Now went backwards at %d: %v < %v", i, t, prev)
		}
		prev = t
	}
	el := time.Since(start)
	fmt.Println("END now-loop")
	nowNs := float64(el.Nanoseconds()) / nNow

	// The syscall for comparison.
	const nSys = 100_000
	t0 := time.Now()
	for i := 0; i < nSys; i++ {
		sysClock(clockMonotonic)
	}
	sysNs := float64(time.Since(t0).Nanoseconds()) / nSys

	// Sleep.
	t1 := time.Now()
	time.Sleep(100 * time.Millisecond)
	slept := time.Since(t1)
	if slept < 100*time.Millisecond || slept > 150*time.Millisecond {
		fail("time.Since over a 100 ms sleep = %v", slept)
	}
	fmt.Printf("sleep: 100 ms sleep measured %v\n", slept)

	// crypto/rand.
	a, b := make([]byte, 32), make([]byte, 32)
	if _, err := rand.Read(a); err != nil {
		fail("rand.Read: %v", err)
	}
	if _, err := rand.Read(b); err != nil {
		fail("rand.Read: %v", err)
	}
	if bytes.Equal(a, make([]byte, 32)) || bytes.Equal(a, b) {
		fail("rand.Read output: %x then %x", a, b)
	}
	big := make([]byte, 1000)
	if _, err := rand.Read(big); err != nil {
		fail("rand.Read(1000): %v", err)
	}
	if bytes.Equal(big[900:], make([]byte, 100)) {
		fail("rand.Read(1000) tail is zero")
	}
	const nRand = 100_000
	buf := make([]byte, 32)
	fmt.Println("BEGIN rand-loop")
	t2 := time.Now()
	var acc byte
	for i := 0; i < nRand; i++ {
		if _, err := rand.Read(buf); err != nil {
			fail("rand.Read: %v", err)
		}
		acc |= buf[0]
	}
	randEl := time.Since(t2)
	fmt.Println("END rand-loop")
	if acc == 0 {
		fail("rand.Read: first byte zero 1e5 times")
	}

	// Make the kernel reseed (it does after 1 MiB of syscall output, when
	// virtio-rng is present), then read again: the vDSO state must fetch a
	// new key (one 32-byte getrandom syscall in a patina.strace=1 log
	// between these markers) and keep producing fresh output.
	fmt.Println("BEGIN reseed")
	chunk := make([]byte, 64<<10)
	for n := 0; n < 1100<<10; n += len(chunk) {
		r, _, e := syscall.Syscall(sysGetrandom(), uintptr(unsafe.Pointer(&chunk[0])), uintptr(len(chunk)), 0)
		if e != 0 || int(r) != len(chunk) {
			fail("getrandom syscall: %d %v", r, e)
		}
	}
	time.Sleep(50 * time.Millisecond)
	for i := 0; i < 3; i++ {
		prevA := append([]byte(nil), a...)
		if _, err := rand.Read(a); err != nil || bytes.Equal(a, prevA) {
			fail("rand.Read after reseed: %v %x", err, a)
		}
	}
	fmt.Println("END reseed")

	mode := "vdso"
	if !hasVDSO {
		mode = "novdso"
	}
	fmt.Printf("RESULT %s time.Now: %.1f ns/call (1e6 calls, %v)\n", mode, nowNs, el)
	fmt.Printf("RESULT %s clock_gettime syscall: %.1f ns/call (1e5 calls)\n", mode, sysNs)
	fmt.Printf("RESULT %s crypto/rand.Read(32 B): %.1f ns/call (1e5 calls, %v)\n", mode,
		float64(randEl.Nanoseconds())/nRand, randEl)
	fmt.Println("PASS vdsobench")
}
