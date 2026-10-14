package main

// Fuzz mode: a robustness regression test of the kernel's syscall argument
// validation, in the tradition of trinity and syzkaller's smoke runs. Each
// worker issues random syscalls from the arch table with random and
// edge-case arguments (NULL, unmapped, guard and kernel-half addresses,
// huge lengths, negative fds, random flag words), mixed with "reasonable"
// calls (a valid fd and a bad buffer, a valid buffer and a bad length) so
// the validation paths behind the early EBADF/ENOSYS exits are reached.
// The only pass criterion is that the kernel keeps running and the program
// exits 0: nothing the program can issue may crash, hang or corrupt the
// kernel.
//
// Exclusions (fuzzSkip). A syscall is excluded only when its only possible
// effect is to end or wedge the test program itself, not the kernel:
//
//	exit, exit_group               end the program
//	kill, tkill, tgkill,           the only target is this process; a
//	rt_sigqueueinfo,               random signal number kills it
//	rt_tgsigqueueinfo,
//	pidfd_send_signal
//	rt_sigreturn                   the crash modes cover it (crash.go); from
//	                               a random stack it kills the program
//	rt_sigaction, rt_sigprocmask,  clobber Go's own handlers / masks / the
//	sigaltstack, rt_sigsuspend,    signal stack; suspend and pause block
//	pause                          until a signal that may never come
//	execve, execveat, fork, vfork, replace or duplicate the process (ENOSYS
//	clone, clone3                  under patina, D57; Go's threads need
//	                               clone with exact arguments)
//	setrlimit, prlimit64           a random NOFILE/STACK/CPU/FSIZE limit
//	                               ends the run later (exhaust mode raises
//	                               NOFILE deliberately)
//	arch_prctl                     ARCH_SET_FS: random TLS base = Go dies
//	clock_settime, settimeofday,   step the VM's wall clock (test-ctl.sh
//	adjtimex, clock_adjtime        covers the pointer checks)
//	reboot, vhangup, kexec_load    the VM / console
//	process_vm_readv/writev        pid 1 is this program: on Linux they
//	                               scribble on its own memory
//	restart_syscall                internal (D45)
//
// Everything else is called. Arguments that could hit the program's own
// state are confined instead of excluded: memory syscalls (mmap, munmap,
// mprotect, mremap, madvise, mincore, msync, mlock*, munlock*) only get
// addresses inside the mm arena or never-valid addresses; the fd
// generator never yields fds 0-2 or the Go runtime's own fds (the
// netpoller's epoll and eventfd), only the fuzzer's own non-blocking fds,
// junk fds and the dup target range [dupLo, dupHi); close, dup2, dup3 and
// close_range only act on that range or on junk; F_SETFL keeps O_NONBLOCK,
// FIONBIO sets it, F_SETLKW becomes F_SETLK and flock gets LOCK_NB; every
// timeout points at a timespec of a few milliseconds and never at NULL;
// timer_create gets no sigevent (SIGALRM, which main ignores); splice/tee/
// vmsplice get SPLICE_F_NONBLOCK; iovec arrays and msghdrs are built with
// bases from the junk area or the never-valid set so a random iovec base
// cannot point into Go's heap. Leaked fds (dup, fcntl(F_DUPFD), socket,
// pipe2, openat...) are closed by a periodic sweep of (fuzzMaxOwned, 1024).

import (
	"fmt"
	"math/rand"
	"os"
	"runtime"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

type sysEnt struct {
	nr   uintptr
	name string
}

var fuzzSkip = map[string]string{
	"exit": "ends the program", "exit_group": "ends the program",
	"kill": "self is the only target", "tkill": "self is the only target",
	"tgkill": "self is the only target", "rt_sigqueueinfo": "self is the only target",
	"rt_tgsigqueueinfo": "self is the only target", "pidfd_send_signal": "self is the only target",
	"rt_sigreturn": "crash modes", "rt_sigaction": "Go's handlers",
	"rt_sigprocmask": "Go's masks", "sigaltstack": "Go's signal stack",
	"rt_sigsuspend": "blocks until a signal", "pause": "blocks until a signal",
	"execve": "replaces the process", "execveat": "replaces the process",
	"fork": "duplicates the process", "vfork": "duplicates the process",
	"clone": "Go's threads", "clone3": "Go's threads",
	"setrlimit": "limits end the run later", "prlimit64": "limits end the run later",
	"arch_prctl": "random TLS base", "clock_settime": "wall clock",
	"settimeofday": "wall clock", "adjtimex": "wall clock", "clock_adjtime": "wall clock",
	"reboot": "the VM", "vhangup": "the console", "kexec_load": "the VM",
	"process_vm_readv": "self memory", "process_vm_writev": "self memory",
	"restart_syscall": "internal",
}

const (
	dupLo, dupHi = 900, 1000
	sweepEvery   = 256
	maxTimeoutNs = 3_000_000 // 3 ms
)

// Fuzzer-owned fds, all O_NONBLOCK.
var (
	fuzzFds      []uintptr
	fuzzMaxOwned uintptr
	fuzzSweepMu  sync.Mutex
	fuzzCounts   sync.Map // name -> *int64 (per-syscall call count)
)

func fuzzSetup() {
	c := newCons(maxWorkers - 1)
	scr(SYS_MKDIRAT, u(-100), c.cstr("/tmp"), 0o755)
	scr(SYS_MKDIRAT, u(-100), c.cstr("/tmp/sfz"), 0o755)
	nb := uintptr(syscall.SOCK_NONBLOCK | syscall.SOCK_CLOEXEC)
	p := c.alloc(8)
	must("pipe2")(scr(SYS_PIPE2, p, syscall.O_NONBLOCK|syscall.O_CLOEXEC))
	fuzzFds = append(fuzzFds, uintptr(get32(p)), uintptr(get32(p+4)))
	must("socketpair")(scr(SYS_SOCKETPAIR, syscall.AF_UNIX, uintptr(syscall.SOCK_STREAM)|nb, 0, p))
	fuzzFds = append(fuzzFds, uintptr(get32(p)), uintptr(get32(p+4)))
	tcp := must("socket tcp")(scr(SYS_SOCKET, syscall.AF_INET, uintptr(syscall.SOCK_STREAM)|nb, 0))
	must("bind tcp")(scr(SYS_BIND, tcp, sockaddrIn(c, 0, [4]byte{127, 0, 0, 1}), 16))
	must("listen tcp")(scr(SYS_LISTEN, tcp, 4))
	udp := must("socket udp")(scr(SYS_SOCKET, syscall.AF_INET, uintptr(syscall.SOCK_DGRAM)|nb, 0))
	must("bind udp")(scr(SYS_BIND, udp, sockaddrIn(c, 0, [4]byte{127, 0, 0, 1}), 16))
	ev := must("eventfd2")(scr(SYS_EVENTFD2, 0, syscall.O_NONBLOCK|syscall.O_CLOEXEC /* EFD_NONBLOCK|EFD_CLOEXEC */))
	ep := must("epoll_create1")(scr(SYS_EPOLL_CREATE1, syscall.EPOLL_CLOEXEC))
	f := must("open file")(scr(SYS_OPENAT, u(-100), c.cstr("/tmp/sfz/data"),
		syscall.O_RDWR|syscall.O_CREAT|syscall.O_NONBLOCK|syscall.O_CLOEXEC, 0o644))
	d := must("open dir")(scr(SYS_OPENAT, u(-100), c.cstr("/tmp"), syscall.O_RDONLY|syscall.O_DIRECTORY|syscall.O_CLOEXEC, 0))
	st := must("open status")(scr(SYS_OPENAT, u(-100), c.cstr("/proc/self/status"), syscall.O_RDONLY|syscall.O_CLOEXEC, 0))
	fuzzFds = append(fuzzFds, tcp, udp, ev, ep, f, d, st)
	for _, fd := range fuzzFds {
		if fd > fuzzMaxOwned {
			fuzzMaxOwned = fd
		}
	}
	fmt.Printf("sysfuzz fuzz: owned fds %v (pipe r/w, unix pair, tcp listener, udp, eventfd, epoll, file, dir, status)\n", fuzzFds)
}

// sweepFds closes fds the fuzzer leaked (anything above the owned set
// that it did not open deliberately). Go's own fds are below the owned
// set, which was opened after the runtime started.
func sweepFds() {
	fuzzSweepMu.Lock()
	defer fuzzSweepMu.Unlock()
	for fd := fuzzMaxOwned + 1; fd < 1024; fd++ {
		scr(SYS_CLOSE, fd)
	}
}

type fuzzer struct {
	id      int
	r       *rand.Rand
	c       *cons
	log     bool
	lastLen uintptr // length argument of the last mmap/mremap (for unmapOutside)
	last    atomic.Pointer[string]
	calls   atomic.Int64
}

// --- argument generators -------------------------------------------------

func (f *fuzzer) fd() uintptr {
	var v uintptr
	switch f.r.Intn(10) {
	case 0:
		v = allOnes
	case 1:
		v = u(int64(-f.r.Intn(1 << 20)))
	case 2:
		v = uintptr(1024 + f.r.Intn(1<<20))
	case 3:
		v = uintptr(dupLo + f.r.Intn(dupHi-dupLo))
	case 4:
		v = uintptr(edgeInts[f.r.Intn(len(edgeInts))])
	default:
		return fuzzFds[f.r.Intn(len(fuzzFds))]
	}
	// The kernel takes an fd as a 32-bit int (close(1<<32 | 3) closes fd
	// 3): never let the low 32 bits name fds 0-2 or the Go runtime's.
	if low := uint32(v); low <= uint32(fuzzMaxOwned) {
		return 1024 + uintptr(low)
	}
	return v
}

// dupTarget is an fd close/dup2/dup3 may replace: the dup range or junk,
// never an owned fd (its number would be reused by a leaked, possibly
// blocking description).
func (f *fuzzer) dupTarget() uintptr {
	if f.r.Intn(8) == 0 {
		v := f.fd()
		for _, o := range fuzzFds {
			if uint32(v) == uint32(o) {
				return 1024 + uintptr(uint32(v))
			}
		}
		return v
	}
	return uintptr(dupLo + f.r.Intn(dupHi-dupLo))
}

func (f *fuzzer) length() uintptr {
	switch f.r.Intn(4) {
	case 0:
		return uintptr(edgeLens[f.r.Intn(len(edgeLens))])
	case 1:
		return uintptr(f.r.Intn(64))
	case 2:
		return uintptr(f.r.Intn(junkSize))
	default:
		return uintptr(f.r.Uint64())
	}
}

func (f *fuzzer) flags() uintptr {
	switch f.r.Intn(4) {
	case 0:
		return 0
	case 1:
		return uintptr(f.r.Intn(1 << 6))
	case 2:
		return uintptr(f.r.Uint32())
	default:
		return 1 << uint(f.r.Intn(32))
	}
}

func (f *fuzzer) smallInt() uintptr {
	switch f.r.Intn(3) {
	case 0:
		return uintptr(f.r.Intn(16))
	case 1:
		return uintptr(edgeInts[f.r.Intn(len(edgeInts))])
	default:
		return allOnes - uintptr(f.r.Intn(4))
	}
}

// ptr is a pointer for the kernel to read or write: usually valid junk,
// sometimes never valid, sometimes a random user or kernel address.
func (f *fuzzer) ptr() uintptr {
	switch f.r.Intn(8) {
	case 0, 1:
		b := badPtrs()
		return b[f.r.Intn(len(b))]
	case 2:
		return rwEnd - uintptr(f.r.Intn(64)) // ends in the guard page
	case 3:
		return roPage + uintptr(f.r.Intn(pageSize))
	case 4:
		return uintptr(f.r.Uint64())
	default:
		return junk(f.r, 1)
	}
}

// path is a NUL-terminated path in the cons area, or a bad pointer.
func (f *fuzzer) path() uintptr {
	if f.r.Intn(6) == 0 {
		return f.ptr()
	}
	paths := []string{
		"/", "/tmp", "/tmp/sfz", "/tmp/sfz/data", "/proc", "/proc/self",
		"/proc/self/status", "/proc/meminfo", "/dev", "/dev/null",
		"/dev/zero", "/dev/urandom", "/nonexistent", "", ".", "..",
		"a/../../../..", "/tmp/sfz/../sfz/./data", "/tmp/sfz/sub",
	}
	switch f.r.Intn(12) {
	case 0:
		return f.c.cstr(strings.Repeat("a", 4100)) // ENAMETOOLONG
	case 1:
		return f.c.cstr("/tmp/sfz/" + strings.Repeat("b", 256)) // component too long
	case 2:
		b := make([]byte, 1+f.r.Intn(40))
		f.r.Read(b)
		return f.c.bytes(append(b, 0))
	case 3:
		return f.c.cstr(fmt.Sprintf("/tmp/sfz/f%d", f.r.Intn(8)))
	case 4:
		return f.c.cstr(strings.Repeat("/tmp/sfz/..", 64) + "/tmp")
	default:
		return f.c.cstr(paths[f.r.Intn(len(paths))])
	}
}

// ts is a timespec of at most a few milliseconds, or a bad pointer that
// is never NULL (NULL means "wait forever" to most timed waits).
func (f *fuzzer) ts() uintptr {
	switch f.r.Intn(8) {
	case 0:
		// Unreadable only: readable junk holds random seconds.
		bad := []uintptr{kernHalf, allOnes - 7, userEnd - 8, holePage, guardPage, lowUnmapd, rwEnd - 8}
		return bad[f.r.Intn(len(bad))]
	case 1:
		return f.c.timespec(0, -1) // EINVAL
	case 2:
		return f.c.timespec(-1, 0) // EINVAL
	case 3:
		return f.c.timespec(0, 1_000_000_000) // EINVAL
	default:
		return f.c.timespec(0, int64(f.r.Intn(maxTimeoutNs)))
	}
}

// timeoutMs is an int timeout for poll/epoll_wait: 0-3 ms (a negative
// value means forever).
func (f *fuzzer) timeoutMs() uintptr {
	return uintptr(f.r.Intn(4))
}

// mmAddr is an address inside the mm arena (half of them page-aligned)
// or an address that no memory syscall may touch anything with.
func (f *fuzzer) mmAddr() uintptr {
	switch f.r.Intn(8) {
	case 0:
		bad := []uintptr{kernHalf, kernHalf + 0x1000, allOnes, allOnes - 4095, userEnd, userEnd - 8, 0xffff_ffff_ffff_f000}
		return bad[f.r.Intn(len(bad))]
	case 1:
		return mmBase + uintptr(f.r.Intn(mmArenaSize))
	default:
		return mmBase + uintptr(f.r.Intn(mmArenaSize/pageSize))*pageSize
	}
}

// mmLen never reaches past the end of the arena unless it wraps around
// the address space (which every memory syscall must reject).
func (f *fuzzer) mmLen(addr uintptr) uintptr {
	var remain uintptr
	if addr >= mmBase && addr < mmBase+mmArenaSize {
		remain = mmBase + mmArenaSize - addr
	}
	switch f.r.Intn(6) {
	case 0:
		return 0
	case 1:
		return allOnes - uintptr(f.r.Intn(1<<20))
	case 2:
		return allOnes - addr + 1 + uintptr(f.r.Intn(4096)) // wraps to 0..4096
	case 3:
		if remain > 0 {
			return remain
		}
		return pageSize
	default:
		if remain == 0 {
			return uintptr(f.r.Intn(1 << 20))
		}
		return uintptr(f.r.Int63n(int64(remain) + 1))
	}
}

func (f *fuzzer) inArena(addr, n uintptr) bool {
	return addr >= mmBase && n <= mmArenaSize && addr+n <= mmBase+mmArenaSize
}

// iov builds an iovec array (bases are junk, bad pointers or NULL) and
// returns (pointer, count); sometimes the pointer itself is bad.
func (f *fuzzer) iov() (uintptr, uintptr) {
	n := f.r.Intn(8)
	switch f.r.Intn(8) {
	case 0:
		n = 1024 + f.r.Intn(4) // UIO_MAXIOV boundary
	case 1:
		return f.ptr(), uintptr(f.r.Intn(1030))
	case 2:
		return f.c.alloc(16), f.length()
	}
	p := f.c.alloc(16 * n)
	for i := 0; i < n; i++ {
		var base uintptr
		switch f.r.Intn(4) {
		case 0:
			base = f.ptr()
		default:
			base = junk(f.r, 1)
		}
		var l uintptr
		switch f.r.Intn(3) {
		case 0:
			l = uintptr(edgeLens[f.r.Intn(len(edgeLens))])
		default:
			l = uintptr(f.r.Intn(8192))
		}
		put64(p+uintptr(16*i), uint64(base))
		put64(p+uintptr(16*i+8), uint64(l))
	}
	return p, uintptr(n)
}

// msghdr builds a struct msghdr with iovecs from iov(); name/control
// point at junk or bad memory.
func (f *fuzzer) msghdr() uintptr {
	if f.r.Intn(8) == 0 {
		return f.ptr()
	}
	return f.msghdrCons()
}

// msghdrCons is msghdr always built in the cons area (safe to copy).
func (f *fuzzer) msghdrCons() uintptr {
	p := f.c.alloc(56)
	iov, n := f.iov()
	var name uintptr
	var namelen uint32
	switch f.r.Intn(4) {
	case 0:
		name, namelen = 0, 0
	case 1:
		name, namelen = sockaddrIn(f.c, uint16(f.r.Intn(65536)), [4]byte{127, 0, 0, 1}), 16
	case 2:
		name, namelen = f.ptr(), uint32(f.r.Intn(300))
	default:
		name, namelen = junk(f.r, 1), uint32(edgeInts[f.r.Intn(len(edgeInts))])
	}
	put64(p, uint64(name))
	put32(p+8, namelen)
	put64(p+16, uint64(iov))
	put64(p+24, uint64(n))
	switch f.r.Intn(3) {
	case 0:
		put64(p+32, 0)
		put64(p+40, 0)
	case 1:
		put64(p+32, uint64(junk(f.r, 64)))
		put64(p+40, uint64(f.r.Intn(128)))
	default:
		put64(p+32, uint64(f.ptr()))
		put64(p+40, uint64(f.length()))
	}
	put32(p+48, uint32(f.flags()))
	return p
}

// generic is a random argument with no knowledge of its meaning.
func (f *fuzzer) generic() uintptr {
	switch f.r.Intn(6) {
	case 0:
		return f.ptr()
	case 1:
		return f.length()
	case 2:
		return f.flags()
	case 3:
		return f.smallInt()
	case 4:
		return f.fd()
	default:
		return uintptr(f.r.Uint64())
	}
}

// --- shaping ------------------------------------------------------------

// shape fills the six arguments for a syscall: generic values, then
// per-syscall knowledge that keeps the call from blocking, from touching
// the program's own fds and mappings, or from being rejected before the
// interesting checks. Returns a cleanup for mappings that landed outside
// the arena.
func (f *fuzzer) shape(name string, a *[6]uintptr) func(r uintptr, e syscall.Errno) {
	for i := range a {
		a[i] = f.generic()
	}
	// Half the calls get a plausible first fd.
	if f.r.Intn(2) == 0 {
		a[0] = fuzzFds[f.r.Intn(len(fuzzFds))]
	}
	switch name {
	case "read", "write", "pread64", "pwrite64", "getrandom", "getdents64", "getcwd":
		a[0], a[1], a[2] = f.fd(), f.ptr(), f.length()
		if name == "getrandom" || name == "getcwd" {
			a[0], a[1] = f.ptr(), f.length()
		}
	case "readv", "writev", "preadv", "pwritev", "preadv2", "pwritev2":
		a[0] = f.fd()
		a[1], a[2] = f.iov()
	case "recvmsg", "sendmsg":
		a[0], a[1], a[2] = f.fd(), f.msghdr(), f.flags()|syscall.MSG_DONTWAIT
	case "recvmmsg", "sendmmsg":
		n := f.r.Intn(4)
		p := f.c.alloc(64 * (n + 1))
		for i := 0; i <= n; i++ {
			copy(mem(p+uintptr(64*i), 56), mem(f.msghdrCons(), 56))
		}
		a[0], a[1], a[2], a[3] = f.fd(), p, uintptr(n+1), f.flags()|syscall.MSG_DONTWAIT
		if f.r.Intn(4) == 0 {
			a[1] = f.ptr()
		}
		a[4] = f.ts() // recvmmsg timeout
	case "recvfrom", "sendto":
		a[0], a[1], a[2], a[3] = f.fd(), f.ptr(), f.length(), f.flags()|syscall.MSG_DONTWAIT
		a[4], a[5] = f.ptr(), f.smallInt()
		if f.r.Intn(2) == 0 {
			a[4], a[5] = sockaddrIn(f.c, uint16(f.r.Intn(65536)), [4]byte{127, 0, 0, 1}), 16
		}
	case "connect", "bind", "accept", "accept4", "getsockname", "getpeername":
		a[0] = f.fd()
		if f.r.Intn(2) == 0 {
			a[1], a[2] = sockaddrIn(f.c, uint16(f.r.Intn(65536)), [4]byte{127, 0, 0, 1}), 16
			if name == "accept" || name == "accept4" || name == "getsockname" || name == "getpeername" {
				l := f.c.alloc(4)
				put32(l, uint32(f.smallInt()))
				a[2] = l
			}
		}
		if name == "accept4" {
			a[3] = f.flags() | syscall.SOCK_NONBLOCK
		}
	case "setsockopt", "getsockopt":
		a[0], a[1], a[2] = f.fd(), f.smallInt(), f.smallInt()
		if f.r.Intn(2) == 0 {
			a[1] = []uintptr{syscall.SOL_SOCKET, syscall.IPPROTO_TCP, syscall.IPPROTO_IP, syscall.IPPROTO_IPV6}[f.r.Intn(4)]
		}
		if name == "getsockopt" {
			l := f.c.alloc(4)
			put32(l, uint32(f.smallInt()))
			if f.r.Intn(3) == 0 {
				a[3], a[4] = f.ptr(), l
			} else {
				a[3], a[4] = junk(f.r, 256), l
			}
		} else {
			a[3], a[4] = f.ptr(), f.smallInt()
		}
	case "socket":
		a[0], a[1], a[2] = f.smallInt(), f.smallInt()|syscall.SOCK_NONBLOCK, f.smallInt()
		if f.r.Intn(2) == 0 {
			a[0] = []uintptr{syscall.AF_INET, syscall.AF_INET6, syscall.AF_UNIX, syscall.AF_NETLINK, 40}[f.r.Intn(5)]
		}
	case "socketpair":
		a[0], a[1], a[2], a[3] = syscall.AF_UNIX, f.smallInt()|syscall.SOCK_NONBLOCK, f.smallInt(), f.ptr()
	case "pipe2", "pipe":
		a[0], a[1] = f.ptr(), f.flags()|syscall.O_NONBLOCK
	case "eventfd2", "timerfd_create", "signalfd4", "inotify_init1":
		// A leaked blocking fd must not exist: a later read would wait.
		a[1] |= syscall.O_NONBLOCK // EFD_/TFD_/SFD_/IN_NONBLOCK
		if name == "signalfd4" {
			a[3] |= syscall.O_NONBLOCK
		}
	case "openat2":
		how := f.c.alloc(24)
		put64(how, uint64(f.flags()|syscall.O_NONBLOCK))
		put64(how+8, uint64(f.smallInt()))
		put64(how+16, uint64(f.r.Intn(64)))
		a[0], a[1], a[2], a[3] = u(-100), f.path(), how, 24
		if f.r.Intn(4) == 0 {
			a[2], a[3] = f.ptr(), f.length()
		}
	case "mknodat":
		// Only node types a read cannot block on forever (no devices).
		a[0], a[1] = u(-100), f.path()
		a[2] = uintptr(f.r.Intn(0o7777)) | []uintptr{0, syscall.S_IFREG, syscall.S_IFIFO, syscall.S_IFSOCK}[f.r.Intn(4)]
		a[3] = f.smallInt()
	case "close":
		a[0] = f.dupTarget()
	case "dup2", "dup3":
		a[0], a[1] = f.fd(), f.dupTarget()
	case "close_range":
		lo := uintptr(dupLo + f.r.Intn(dupHi-dupLo))
		a[0], a[1], a[2] = lo, lo+uintptr(f.r.Intn(1<<20)), f.flags()
		if f.r.Intn(4) == 0 {
			a[1] = allOnes
		}
	case "fcntl":
		a[0], a[1] = f.fd(), f.smallInt()
		switch a[1] {
		case syscall.F_SETFL:
			a[2] |= syscall.O_NONBLOCK
		case syscall.F_SETLKW:
			a[1] = syscall.F_SETLK
		case 38: // F_OFD_SETLKW
			a[1] = 37
		}
	case "ioctl":
		a[0], a[1], a[2] = f.fd(), f.smallInt(), f.ptr()
		if f.r.Intn(2) == 0 {
			a[1] = []uintptr{FIONREAD, FIONBIO, syscall.TCGETS, syscall.TIOCGWINSZ, syscall.SIOCGIFCONF, 0x5452}[f.r.Intn(6)]
		}
		if a[1] == FIONBIO {
			p := f.c.alloc(4)
			put32(p, 1)
			a[2] = p
		}
	case "flock":
		a[0], a[1] = f.fd(), f.smallInt()|syscall.LOCK_NB
	case "epoll_ctl":
		a[0], a[1], a[2] = f.fd(), f.smallInt(), f.fd()
		if f.r.Intn(2) == 0 {
			a[0] = fuzzFds[len(fuzzFds)-4] // the epoll fd
			a[1] = uintptr([]int{syscall.EPOLL_CTL_ADD, syscall.EPOLL_CTL_MOD, syscall.EPOLL_CTL_DEL}[f.r.Intn(3)])
		}
		if f.r.Intn(2) == 0 {
			p := f.c.alloc(16)
			put32(p, uint32(f.flags()))
			a[3] = p
		} else {
			a[3] = f.ptr()
		}
	case "epoll_wait", "epoll_pwait":
		a[0], a[1], a[2], a[3] = f.fd(), f.ptr(), f.smallInt(), f.timeoutMs()
		a[4], a[5] = f.ptr(), f.smallInt() // sigmask, sigsetsize
		if f.r.Intn(2) == 0 {
			a[4], a[5] = 0, 8
		}
	case "epoll_pwait2":
		a[0], a[1], a[2], a[3] = f.fd(), f.ptr(), f.smallInt(), f.ts()
		a[4], a[5] = 0, 8
	case "poll":
		a[0], a[1], a[2] = f.ptr(), f.smallInt(), f.timeoutMs()
	case "ppoll":
		a[0], a[1], a[2], a[3], a[4] = f.ptr(), f.smallInt(), f.ts(), f.ptr(), f.smallInt()
		if f.r.Intn(2) == 0 {
			a[3], a[4] = 0, 8
		}
	case "select":
		a[0], a[1], a[2], a[3] = f.smallInt(), f.ptr(), f.ptr(), f.ptr()
		a[4] = f.c.timespec(0, int64(f.r.Intn(maxTimeoutNs/1000))) // timeval
	case "pselect6":
		a[0], a[1], a[2], a[3], a[4], a[5] = f.smallInt(), f.ptr(), f.ptr(), f.ptr(), f.ts(), 0
	case "nanosleep":
		a[0], a[1] = f.ts(), f.ptr()
	case "clock_nanosleep":
		a[0], a[1], a[2], a[3] = f.smallInt(), 0, f.ts(), f.ptr()
	case "futex":
		// FUTEX_WAKE, WAIT, REQUEUE, CMP_REQUEUE, WAKE_OP, WAIT_BITSET,
		// WAKE_BITSET, random; half of them FUTEX_PRIVATE_FLAG.
		ops := []uintptr{1, 0, 3, 4, 5, 9, 10, uintptr(f.r.Intn(32))}
		op := ops[f.r.Intn(len(ops))]
		if f.r.Intn(2) == 0 {
			op |= 128
		}
		var addr uintptr
		if f.r.Intn(3) == 0 {
			addr = f.ptr()
		} else {
			addr = junk(f.r, 4) &^ uintptr(f.r.Intn(2)*3)
		}
		a[0], a[1], a[2], a[3], a[4], a[5] = addr, op, f.smallInt(), f.ts(), f.ptr(), f.smallInt()
	case "rt_sigtimedwait":
		a[0], a[1], a[2], a[3] = f.ptr(), f.ptr(), f.ts(), 8
	case "futex_waitv", "semtimedop":
		a[3] = f.ts()
	case "mq_timedsend", "mq_timedreceive", "io_getevents", "io_pgetevents":
		a[4] = f.ts()
	case "timer_create":
		a[0], a[1], a[2] = f.smallInt(), 0, f.ptr()
	case "timer_settime", "timerfd_settime":
		a[0], a[1], a[2], a[3] = f.smallInt(), f.flags(), f.ptr(), f.ptr()
		if f.r.Intn(2) == 0 {
			a[2] = f.c.timespec(0, int64(f.r.Intn(maxTimeoutNs)), 0, int64(f.r.Intn(maxTimeoutNs)))
		}
	case "setitimer":
		a[0], a[1], a[2] = uintptr(f.r.Intn(4)), f.ptr(), f.ptr()
		if f.r.Intn(2) == 0 {
			a[1] = f.c.timespec(0, int64(f.r.Intn(maxTimeoutNs/1000)), 0, int64(f.r.Intn(maxTimeoutNs/1000)))
		}
	case "splice", "tee", "vmsplice":
		a[0], a[2] = f.fd(), f.fd()
		a[5] |= 2 // SPLICE_F_NONBLOCK
		if name == "vmsplice" {
			a[1], a[2] = f.iov()
			a[3] |= 2
		}
		if name == "tee" {
			a[1], a[3] = f.fd(), f.flags()|2
		}
	case "sendfile", "copy_file_range":
		a[0], a[1], a[2], a[3], a[4] = f.fd(), f.fd(), f.ptr(), f.ptr(), f.length()
		if name == "sendfile" {
			a[3] = f.length()
		}
	case "mmap":
		var addr uintptr
		switch f.r.Intn(3) {
		case 0:
			addr = 0
		default:
			addr = f.mmAddr()
		}
		n := f.mmLen(addr)
		if addr == 0 {
			n = uintptr(f.r.Intn(1 << 20))
		}
		fl := f.flags()
		if addr == 0 || !f.inArena(addr, n) {
			fl &^= syscall.MAP_FIXED
		}
		fl |= syscall.MAP_PRIVATE | syscall.MAP_ANONYMOUS | syscall.MAP_NORESERVE
		a[0], a[1], a[2], a[3], a[4], a[5] = addr, n, f.flags(), fl, f.fd(), f.length()
		if f.r.Intn(2) == 0 {
			a[4], a[5] = allOnes, 0
		}
		return f.unmapOutside
	case "mremap":
		// Never a move or a shrink: either leaves a hole in the arena that
		// the kernel may hand to Go's next heap mapping (see munmap).
		old := f.mmAddr()
		oldLen := f.mmLen(old)
		newLen := oldLen + uintptr(f.r.Intn(1<<20))
		if f.r.Intn(4) == 0 {
			newLen = f.length()
			if newLen < oldLen {
				newLen = oldLen
			}
		}
		a[0], a[1], a[2], a[3], a[4] = old, oldLen, newLen, f.flags()&^(1|2|4), f.mmAddr()
		return f.unmapOutside
	case "munmap":
		// Only addresses the call must reject: an unmapped arena range is
		// free space the kernel would place Go's next mapping in, and the
		// next arena-wide call would destroy it. Valid unmaps are covered
		// by the cases.
		addr := f.mmAddr()
		if addr&(pageSize-1) == 0 {
			addr += 1 + uintptr(f.r.Intn(pageSize-1))
		}
		a[0], a[1] = addr, f.mmLen(addr)
	case "mprotect", "madvise", "mincore", "msync", "mlock", "mlock2", "munlock", "remap_file_pages":
		addr := f.mmAddr()
		a[0], a[1] = addr, f.mmLen(addr)
		switch name {
		case "mprotect":
			a[2] = f.flags()
		case "mincore":
			a[2] = f.ptr()
		}
	case "process_madvise":
		a[0] = f.fd()
		a[1], a[2] = f.iov()
	case "brk":
		a[0] = f.ptr()
	case "set_tid_address", "set_robust_list", "rseq":
		a[0] = f.ptr()
	case "openat", "mkdirat", "unlinkat", "renameat", "renameat2", "linkat", "symlinkat",
		"readlinkat", "newfstatat", "fchmodat", "fchownat", "faccessat", "faccessat2", "utimensat",
		"statx", "name_to_handle_at", "fchmodat2":
		a[0], a[1] = u(-100), f.path()
		if f.r.Intn(4) == 0 {
			a[0] = f.fd()
		}
		switch name {
		case "openat":
			a[2], a[3] = f.flags()|syscall.O_NONBLOCK, f.smallInt()
			if f.r.Intn(2) == 0 {
				a[2] = []uintptr{syscall.O_RDONLY, syscall.O_RDWR | syscall.O_CREAT, syscall.O_WRONLY | syscall.O_TRUNC,
					syscall.O_RDONLY | syscall.O_DIRECTORY, 0x200000 /* O_PATH */, syscall.O_RDWR | syscall.O_CREAT | syscall.O_EXCL,
					syscall.O_RDONLY | syscall.O_NOFOLLOW}[f.r.Intn(7)] | syscall.O_NONBLOCK
			}
		case "renameat", "linkat":
			a[2], a[3] = u(-100), f.path()
		case "renameat2":
			a[2], a[3], a[4] = u(-100), f.path(), f.flags()
		case "symlinkat":
			a[1], a[2], a[3] = f.path(), u(-100), f.path()
		case "readlinkat":
			a[2], a[3] = f.ptr(), f.length()
		case "newfstatat", "statx":
			a[2], a[3] = f.ptr(), f.flags()
			if name == "statx" {
				a[2], a[3], a[4] = f.flags(), f.flags(), f.ptr()
			}
		case "utimensat":
			a[2], a[3] = f.c.timespec(int64(f.r.Intn(1<<31)), int64(f.r.Intn(1_000_000_000)), 0, int64(f.r.Intn(1<<31))), f.flags()
			if f.r.Intn(3) == 0 {
				a[2] = f.ptr()
			}
		}
	case "chdir", "chroot", "truncate", "umount2", "swapon", "swapoff", "acct", "quotactl":
		a[0] = f.path()
	case "mount", "pivot_root":
		a[0], a[1], a[2] = f.path(), f.path(), f.path()
	case "sched_setaffinity", "sched_getaffinity":
		a[0], a[1], a[2] = 0, f.smallInt(), f.ptr()
		if f.r.Intn(2) == 0 {
			p := f.c.alloc(128)
			put64(p, uint64(1+f.r.Intn(255)))
			a[1], a[2] = 8+uintptr(f.r.Intn(3))*8, p
		}
	case "prctl":
		a[0] = uintptr(f.r.Intn(80))
		if a[0] == syscall.PR_SET_NAME || a[0] == syscall.PR_GET_NAME {
			a[1] = f.ptr()
		}
	case "exit", "exit_group":
		// never reached: skipped
	}
	return nil
}

// unmapOutside releases a mapping the kernel placed outside the arena
// (an mmap without MAP_FIXED, or an mremap that moved).
func (f *fuzzer) unmapOutside(r uintptr, e syscall.Errno) {
	if e != 0 || r < 0x10000 {
		return
	}
	if r >= mmBase && r < mmBase+mmArenaSize {
		return
	}
	// The length is the one the kernel rounded up from; unmapping one
	// page too many beyond our own mapping is still only our mapping
	// (the kernel places new ones in free space), so use what we asked.
	scr(SYS_MUNMAP, r, f.lastLen)
}

func (f *fuzzer) run(iters int, wg *sync.WaitGroup) {
	defer wg.Done()
	runtime.LockOSThread()
	// Candidate syscalls: everything the table names minus the skip list.
	cands := make([]sysEnt, 0, len(sysTable))
	for _, s := range sysTable {
		if _, skip := fuzzSkip[s.name]; !skip {
			cands = append(cands, s)
		}
	}
	for i := 0; i < iters; i++ {
		if i%sweepEvery == sweepEvery-1 {
			sweepFds()
		}
		if i%64 == 0 {
			f.c.reset()
		}
		s := cands[f.r.Intn(len(cands))]
		var a [6]uintptr
		cleanup := f.shape(s.name, &a)
		f.lastLen = a[1]
		if s.name == "mremap" {
			f.lastLen = a[2]
		}
		desc := fmt.Sprintf("%s(%#x, %#x, %#x, %#x, %#x, %#x)", s.name, a[0], a[1], a[2], a[3], a[4], a[5])
		f.last.Store(&desc)
		if f.log {
			fmt.Printf("[%d] %s\n", f.id, desc)
		}
		r, _, e := syscall.Syscall6(s.nr, a[0], a[1], a[2], a[3], a[4], a[5])
		f.calls.Add(1)
		if f.log {
			fmt.Printf("[%d]   = %#x errno=%d\n", f.id, r, int(e))
		}
		if cleanup != nil {
			cleanup(r, e)
		}
		v, _ := fuzzCounts.LoadOrStore(s.name, new(int64))
		atomic.AddInt64(v.(*int64), 1)
	}
}

func runFuzz() {
	fuzzSetup()
	procs := *procsFlag
	if procs <= 0 {
		procs = runtime.GOMAXPROCS(0)
	}
	if procs > maxWorkers-1 {
		procs = maxWorkers - 1
	}
	fmt.Printf("sysfuzz fuzz: seed=%d iters=%d procs=%d syscalls=%d skipped=%d\n",
		*seedFlag, *itersFlag, procs, len(sysTable), len(fuzzSkip))
	start := time.Now()
	var wg sync.WaitGroup
	workers := make([]*fuzzer, procs)
	for w := 0; w < procs; w++ {
		f := &fuzzer{id: w, r: rand.New(rand.NewSource(*seedFlag*1000 + int64(w))), c: newCons(w), log: *vFlag}
		workers[w] = f
		wg.Add(1)
		go f.run(*itersFlag/procs, &wg)
	}
	// Watchdog: progress every 5 s; past the deadline, print what every
	// worker is stuck in (a kernel that is alive still gets here) and
	// fail. A kernel hang shows no progress line at all.
	done := make(chan struct{})
	go func() {
		t := time.NewTicker(5 * time.Second)
		defer t.Stop()
		for {
			select {
			case <-done:
				return
			case <-t.C:
				var n int64
				for _, f := range workers {
					n += f.calls.Load()
				}
				fmt.Printf("sysfuzz fuzz: progress %d calls after %v\n", n, time.Since(start).Round(time.Second))
				if time.Since(start) > *deadlineFlag {
					for _, f := range workers {
						if p := f.last.Load(); p != nil && f.calls.Load() < int64(*itersFlag/procs) {
							fmt.Printf("FAIL sysfuzz fuzz: worker %d stuck in %s\n", f.id, *p)
						}
					}
					os.Exit(3)
				}
			}
		}
	}()
	wg.Wait()
	close(done)
	sweepFds()
	// Summary: per-syscall call counts, sorted by name.
	var names []string
	fuzzCounts.Range(func(k, _ any) bool { names = append(names, k.(string)); return true })
	sort.Strings(names)
	var sb strings.Builder
	total := 0
	for i, n := range names {
		v, _ := fuzzCounts.Load(n)
		c := atomic.LoadInt64(v.(*int64))
		total += int(c)
		fmt.Fprintf(&sb, "%s=%d ", n, c)
		if i%8 == 7 {
			sb.WriteByte('\n')
		}
	}
	fmt.Printf("sysfuzz fuzz: %d calls over %d syscalls in %v\n%s\n", total, len(names), time.Since(start).Round(time.Millisecond), sb.String())
	// The kernel still answers, and the arena is still ours.
	if _, e := scr(SYS_MPROTECT, mmBase, mmArenaSize, syscall.PROT_NONE); e != 0 && e != syscall.ENOMEM {
		fmt.Printf("FAIL sysfuzz fuzz: mprotect arena after fuzz: %v\n", e)
		os.Exit(1)
	}
	fmt.Println("PASS sysfuzz fuzz")
}
