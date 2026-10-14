package main

// limits mode: a resource-limit regression test for the kernel's commit
// accounting (D78/D87). Each kernel-backed resource is created until the
// kernel refuses. The refusal must be one of the errnos Linux returns for
// that call, the kernel must keep answering syscalls throughout, and once
// everything is released the kernel's memory use must come back down.
//
// With -limits-strict the final MemFree must be within limitsSlackKB of the
// baseline; without it the delta is only reported (D87: heap frames are not
// yet returned to the frame allocator).

import (
	"bufio"
	"errors"
	"flag"
	"fmt"
	"os"
	"strconv"
	"strings"
	"syscall"
)

var limitsStrict = flag.Bool("limits-strict", false, "limits mode: fail unless MemFree returns to within 8 MiB of the baseline")

const (
	limitsSlackKB = 8 * 1024
	limitsFdCap   = 1 << 20 // the kernel's nr_open equivalent (prlimit refuses more)
	limitsMapCap  = 1 << 14 // 64 MiB chunks: 1 TiB of address space at most
)

func limFail(format string, args ...any) {
	fmt.Printf("FAIL sysfuzz limits: "+format+"\n", args...)
	os.Exit(1)
}

func limMemFreeKB() int {
	f, err := os.Open("/proc/meminfo")
	if err != nil {
		limFail("open /proc/meminfo: %v", err)
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		if len(fields) >= 2 && fields[0] == "MemFree:" {
			kb, err := strconv.Atoi(fields[1])
			if err != nil {
				limFail("MemFree %q: %v", fields[1], err)
			}
			return kb
		}
	}
	limFail("MemFree missing from /proc/meminfo")
	return 0
}

// limErrno unwraps the errno from a raw syscall error or an *os.PathError.
func limErrno(err error) syscall.Errno {
	var en syscall.Errno
	if errors.As(err, &en) {
		return en
	}
	limFail("error without an errno: %v", err)
	return 0
}

func limExpect(what string, err error, allowed ...syscall.Errno) {
	en := limErrno(err)
	for _, a := range allowed {
		if en == a {
			return
		}
	}
	limFail("%s refused with %v (%d); expected one of %v", what, en, int(en), allowed)
}

// limAlive checks that the kernel still answers a trivial syscall.
func limAlive(stage string) {
	if _, _, e := syscall.RawSyscall(syscall.SYS_GETPID, 0, 0, 0); e != 0 {
		limFail("getpid after %s: %v", stage, e)
	}
}

func limRaiseNofile() uint64 {
	var rl syscall.Rlimit
	if err := syscall.Getrlimit(syscall.RLIMIT_NOFILE, &rl); err != nil {
		limFail("getrlimit: %v", err)
	}
	rl.Cur = rl.Max
	if err := syscall.Setrlimit(syscall.RLIMIT_NOFILE, &rl); err != nil {
		limFail("setrlimit NOFILE %d: %v", rl.Max, err)
	}
	return rl.Cur
}

// limPipes creates pipes (writing one byte into each so the ring is
// allocated) until the kernel refuses, then closes them all.
func limPipes() {
	fds := make([]int, 0, 4096)
	var refusal error
	for len(fds)/2 < limitsFdCap {
		var p [2]int
		if err := syscall.Pipe2(p[:], syscall.O_CLOEXEC); err != nil {
			refusal = err
			break
		}
		fds = append(fds, p[0], p[1])
		if _, err := syscall.Write(p[1], []byte{1}); err != nil {
			refusal = err
			break
		}
	}
	if refusal != nil {
		limExpect("pipe2/write", refusal, syscall.EMFILE, syscall.ENFILE, syscall.ENOMEM)
	}
	limAlive("pipes")
	for _, fd := range fds {
		syscall.Close(fd)
	}
	fmt.Printf("ok pipes: %d created, refused with %v\n", len(fds)/2, refusal)
}

// limSockets creates UDP sockets until the kernel refuses, then closes them.
func limSockets() {
	fds := make([]int, 0, 4096)
	var refusal error
	for len(fds) < limitsFdCap {
		fd, err := syscall.Socket(syscall.AF_INET, syscall.SOCK_DGRAM|syscall.SOCK_CLOEXEC, 0)
		if err != nil {
			refusal = err
			break
		}
		fds = append(fds, fd)
	}
	if refusal != nil {
		limExpect("socket", refusal, syscall.EMFILE, syscall.ENFILE, syscall.ENOBUFS, syscall.ENOMEM)
	}
	limAlive("sockets")
	for _, fd := range fds {
		syscall.Close(fd)
	}
	fmt.Printf("ok udp sockets: %d created, refused with %v\n", len(fds), refusal)
}

// limEpolls creates epoll instances until the kernel refuses, then closes them.
func limEpolls() {
	fds := make([]int, 0, 4096)
	var refusal error
	for len(fds) < limitsFdCap {
		fd, err := syscall.EpollCreate1(syscall.EPOLL_CLOEXEC)
		if err != nil {
			refusal = err
			break
		}
		fds = append(fds, fd)
	}
	if refusal != nil {
		limExpect("epoll_create1", refusal, syscall.EMFILE, syscall.ENFILE, syscall.ENOMEM)
	}
	limAlive("epolls")
	for _, fd := range fds {
		syscall.Close(fd)
	}
	fmt.Printf("ok epolls: %d created, refused with %v\n", len(fds), refusal)
}

// limMappings reserves writable anonymous memory in 64 MiB chunks until the
// commit limit refuses, then unmaps everything. A single oversized request
// must also be refused cleanly.
func limMappings() {
	const chunk = 64 << 20
	if _, err := syscall.Mmap(-1, 0, 1<<40, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_ANON|syscall.MAP_PRIVATE); err == nil {
		limFail("1 TiB writable mmap succeeded")
	} else {
		limExpect("mmap 1 TiB", err, syscall.ENOMEM)
	}
	maps := make([][]byte, 0, 64)
	var refusal error
	for len(maps) < limitsMapCap {
		b, err := syscall.Mmap(-1, 0, chunk, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_ANON|syscall.MAP_PRIVATE)
		if err != nil {
			refusal = err
			break
		}
		maps = append(maps, b)
	}
	if refusal == nil {
		limFail("%d chunks of 64 MiB mapped without refusal", len(maps))
	}
	limExpect("mmap 64 MiB", refusal, syscall.ENOMEM)
	limAlive("mappings")
	for _, b := range maps {
		if err := syscall.Munmap(b); err != nil {
			limFail("munmap: %v", err)
		}
	}
	fmt.Printf("ok mappings: %d x 64 MiB committed, refused with %v\n", len(maps), refusal)
}

// limTmpfs writes to /tmp in 4 MiB chunks until the filesystem refuses,
// then removes the file.
func limTmpfs() {
	const path = "/tmp/sysfuzz-limits.bin"
	f, err := os.OpenFile(path, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o644)
	if err != nil {
		limFail("create %s: %v", path, err)
	}
	buf := make([]byte, 4<<20)
	for i := range buf {
		buf[i] = byte(i)
	}
	var written int64
	var refusal error
	for written < 1<<40 {
		n, err := f.Write(buf)
		written += int64(n)
		if err != nil {
			refusal = err
			break
		}
	}
	if refusal == nil {
		limFail("tmpfs accepted %d bytes without refusal", written)
	}
	limExpect("tmpfs write", refusal, syscall.ENOSPC, syscall.ENOMEM)
	limAlive("tmpfs")
	f.Close()
	if err := os.Remove(path); err != nil {
		limFail("remove %s: %v", path, err)
	}
	fmt.Printf("ok tmpfs: %d MiB written, refused with %v\n", written>>20, refusal)
}

func runLimits() {
	nofile := limRaiseNofile()
	base := limMemFreeKB()
	fmt.Printf("sysfuzz limits: RLIMIT_NOFILE=%d MemFree=%d kB\n", nofile, base)

	limPipes()
	limSockets()
	limEpolls()
	limMappings()
	limTmpfs()

	after := limMemFreeKB()
	delta := base - after
	fmt.Printf("sysfuzz limits: MemFree after release %d kB (delta %d kB)\n", after, delta)
	if *limitsStrict && delta > limitsSlackKB {
		limFail("MemFree did not return to baseline: %d kB still held (slack %d kB)", delta, limitsSlackKB)
	}
	fmt.Println("PASS sysfuzz limits")
}
