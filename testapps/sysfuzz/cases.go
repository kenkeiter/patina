package main

// Deterministic nasty cases. Each expectation is the errno Linux returns;
// the comment names the Linux function (v6.x+ source) that produces it.
// There is no Linux reference machine in the test harness, so these
// citations are the reference: where Linux's answer depends on
// configuration or timing the case is not asserted here (the fuzzer still
// covers it for "kernel survives").

import (
	"fmt"
	"os"
	"strings"
	"syscall"
)

type tcase struct {
	c      *cons
	failed []string
	n      int
}

// want checks errno (0 = success expected).
func (t *tcase) want(name string, r uintptr, e, want syscall.Errno) {
	t.n++
	if e == want {
		fmt.Printf("ok %s\n", name)
		return
	}
	msg := fmt.Sprintf("%s: got r=%#x errno=%d (%v), want errno=%d (%v)",
		name, r, int(e), e, int(want), want)
	fmt.Printf("FAIL %s\n", msg)
	t.failed = append(t.failed, msg)
}

// wantRet checks a successful call's return value.
func (t *tcase) wantRet(name string, r uintptr, e syscall.Errno, want uintptr) {
	t.n++
	if e == 0 && r == want {
		fmt.Printf("ok %s\n", name)
		return
	}
	msg := fmt.Sprintf("%s: got r=%#x errno=%d (%v), want r=%#x", name, r, int(e), e, want)
	fmt.Printf("FAIL %s\n", msg)
	t.failed = append(t.failed, msg)
}

// wantOneOf accepts any of several errnos (used only where Linux itself
// differs by architecture or configuration; the comment says why).
func (t *tcase) wantOneOf(name string, r uintptr, e syscall.Errno, want ...syscall.Errno) {
	for _, w := range want {
		if e == w {
			t.want(name, r, e, w)
			return
		}
	}
	t.want(name, r, e, want[0])
}

func (t *tcase) check(name string, ok bool, detail string) {
	t.n++
	if ok {
		fmt.Printf("ok %s\n", name)
		return
	}
	msg := name + ": " + detail
	fmt.Printf("FAIL %s\n", msg)
	t.failed = append(t.failed, msg)
}

// must("what")(scr(...)) exits the program when a setup call fails.
func must(name string) func(uintptr, syscall.Errno) uintptr {
	return func(r uintptr, e syscall.Errno) uintptr {
		if e != 0 {
			fmt.Printf("FAIL sysfuzz setup %s: %v\n", name, e)
			os.Exit(1)
		}
		return r
	}
}

func closeFd(fd uintptr) { scr(SYS_CLOSE, fd) }

// pipe returns a non-blocking pipe.
func pipe(t *tcase) (uintptr, uintptr) {
	p := t.c.alloc(8)
	must("pipe2")(scr(SYS_PIPE2, p, syscall.O_NONBLOCK|syscall.O_CLOEXEC))
	return uintptr(get32(p)), uintptr(get32(p + 4))
}

func openPath(t *tcase, path string, flags int) uintptr {
	return must("open " + path)(scr(SYS_OPENAT, u(-100), t.c.cstr(path), uintptr(flags|syscall.O_CLOEXEC), 0o644))
}

func runCases() {
	t := &tcase{c: newCons(maxWorkers - 1)}
	scr(SYS_MKDIRAT, u(-100), t.c.cstr("/tmp"), 0o755)
	scr(SYS_MKDIRAT, u(-100), t.c.cstr("/tmp/sfc"), 0o755)
	casesMM(t)
	casesFd(t)
	casesRW(t)
	casesPath(t)
	casesPoll(t)
	casesNet(t)
	casesSignal(t)
	casesTime(t)
	casesProc(t)
	casesArch(t)
	if len(t.failed) > 0 {
		fmt.Printf("sysfuzz cases: %d of %d FAILED:\n", len(t.failed), t.n)
		for _, f := range t.failed {
			fmt.Printf("  %s\n", f)
		}
		os.Exit(1)
	}
	fmt.Printf("PASS sysfuzz cases (%d)\n", t.n)
}

// Generic ioctl numbers (the same on arm64 and x86_64; the syscall
// package defines them for neither).
const (
	FIONREAD = 0x541b
	FIONBIO  = 0x5421
)

const (
	anon     = syscall.MAP_PRIVATE | syscall.MAP_ANONYMOUS
	rw       = syscall.PROT_READ | syscall.PROT_WRITE
	mapFixed = syscall.MAP_FIXED
	noRes    = syscall.MAP_NORESERVE
)

func casesMM(t *tcase) {
	E := syscall.Errno(0)
	// mm/mmap.c do_mmap: "if (!len) return -EINVAL".
	r, e := scr(SYS_MMAP, 0, 0, rw, anon, allOnes, 0)
	t.want("mmap len 0", r, e, syscall.EINVAL)
	// do_mmap: len = PAGE_ALIGN(len) wraps to 0 → "if (!len) return -ENOMEM".
	r, e = scr(SYS_MMAP, 0, allOnes-100, rw, anon, allOnes, 0)
	t.want("mmap len wraps", r, e, syscall.ENOMEM)
	// __get_unmapped_area: "if (len > TASK_SIZE) return -ENOMEM".
	r, e = scr(SYS_MMAP, 0, 1<<62, syscall.PROT_NONE, anon|noRes, allOnes, 0)
	t.want("mmap len > TASK_SIZE", r, e, syscall.ENOMEM)
	// arch sys_mmap: "if (offset_in_page(off) != 0) return -EINVAL".
	r, e = scr(SYS_MMAP, 0, 4096, rw, anon, allOnes, 1)
	t.want("mmap unaligned offset", r, e, syscall.EINVAL)
	// do_mmap: MAP_TYPE neither SHARED nor PRIVATE → "default: return -EINVAL".
	r, e = scr(SYS_MMAP, 0, 4096, rw, syscall.MAP_ANONYMOUS, allOnes, 0)
	t.want("mmap no map type", r, e, syscall.EINVAL)
	// MAP_FIXED unaligned: __get_unmapped_area "if (offset_in_page(addr)) return -EINVAL".
	r, e = scr(SYS_MMAP, mmBase+1, 4096, rw, anon|mapFixed, allOnes, 0)
	t.want("mmap fixed unaligned", r, e, syscall.EINVAL)
	// MAP_FIXED in the kernel half: __get_unmapped_area "if (addr > TASK_SIZE - len) return -ENOMEM".
	r, e = scr(SYS_MMAP, kernHalf, 4096, rw, anon|mapFixed, allOnes, 0)
	t.want("mmap fixed kernel", r, e, syscall.ENOMEM)
	// MAP_FIXED range wrapping: same check.
	r, e = scr(SYS_MMAP, 0x7fff_0000_0000, 1<<47, syscall.PROT_NONE, anon|mapFixed|noRes, allOnes, 0)
	t.want("mmap fixed past end", r, e, syscall.ENOMEM)
	// do_mmap ignores unknown prot bits (only mprotect validates them).
	r, e = scr(SYS_MMAP, 0, 4096, rw|0x1000, anon, allOnes, 0)
	t.want("mmap unknown prot bit", r, e, 0)
	if e == 0 {
		scr(SYS_MUNMAP, r, 4096)
	}

	// mm/vma.c do_vmi_munmap / __vm_munmap: "if ((offset_in_page(start)) ||
	// start > TASK_SIZE || len > TASK_SIZE-start) return -EINVAL"; len 0
	// is EINVAL too ("if (!len) return -EINVAL" after PAGE_ALIGN).
	r, e = scr(SYS_MUNMAP, mmBase, 0)
	t.want("munmap len 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_MUNMAP, mmBase+1, 4096)
	t.want("munmap unaligned", r, e, syscall.EINVAL)
	r, e = scr(SYS_MUNMAP, kernHalf, 4096)
	t.want("munmap kernel", r, e, syscall.EINVAL)
	r, e = scr(SYS_MUNMAP, mmBase, allOnes-4095)
	t.want("munmap len wraps", r, e, syscall.EINVAL)

	// mm/mprotect.c do_mprotect_pkey.
	r, e = scr(SYS_MPROTECT, mmBase+1, 4096, syscall.PROT_READ)
	t.want("mprotect unaligned", r, e, syscall.EINVAL)
	// "if (!len) return 0" comes before any range or prot check.
	r, e = scr(SYS_MPROTECT, kernHalf, 0, 0x1000)
	t.want("mprotect len 0 kernel addr", r, e, 0)
	// "end <= start → -ENOMEM".
	r, e = scr(SYS_MPROTECT, mmBase, allOnes-4095, syscall.PROT_READ)
	t.want("mprotect len wraps", r, e, syscall.ENOMEM)
	// no VMA → -ENOMEM.
	r, e = scr(SYS_MPROTECT, kernHalf, 4096, syscall.PROT_READ)
	t.want("mprotect kernel", r, e, syscall.ENOMEM)
	r, e = scr(SYS_MPROTECT, holePage, 4096, syscall.PROT_READ)
	t.want("mprotect hole", r, e, syscall.ENOMEM)
	// arch_validate_prot: unknown bits → -EINVAL.
	r, e = scr(SYS_MPROTECT, mmBase, 4096, 0x1000)
	t.want("mprotect unknown prot bit", r, e, syscall.EINVAL)
	// "if (grows == (PROT_GROWSDOWN|PROT_GROWSUP)) return -EINVAL".
	r, e = scr(SYS_MPROTECT, mmBase, 4096, syscall.PROT_READ|0x01000000|0x02000000)
	t.want("mprotect growsdown|growsup", r, e, syscall.EINVAL)
	// PROT_GROWSDOWN on a VMA without VM_GROWSDOWN → -EINVAL.
	r, e = scr(SYS_MPROTECT, mmBase, 4096, syscall.PROT_READ|0x01000000)
	t.want("mprotect growsdown on plain vma", r, e, syscall.EINVAL)

	// mm/madvise.c do_madvise: behaviour first, then check_input_range.
	r, e = scr(SYS_MADVISE, mmBase+1, 4096, 12345)
	t.want("madvise bad advice", r, e, syscall.EINVAL)
	r, e = scr(SYS_MADVISE, mmBase+1, 4096, syscall.MADV_DONTNEED)
	t.want("madvise unaligned", r, e, syscall.EINVAL)
	// "if (len_in && !len) return -EINVAL" (PAGE_ALIGN wrapped).
	r, e = scr(SYS_MADVISE, mmBase, allOnes-100, syscall.MADV_DONTNEED)
	t.want("madvise len wraps", r, e, syscall.EINVAL)
	// "if (start + len < start) return -EINVAL".
	r, e = scr(SYS_MADVISE, kernHalf, 1<<63, syscall.MADV_DONTNEED)
	t.want("madvise range wraps", r, e, syscall.EINVAL)
	// no VMA → madvise_walk_vmas returns -ENOMEM.
	r, e = scr(SYS_MADVISE, kernHalf, 4096, syscall.MADV_DONTNEED)
	t.want("madvise kernel", r, e, syscall.ENOMEM)
	r, e = scr(SYS_MADVISE, holePage, 4096, syscall.MADV_NORMAL)
	t.want("madvise hole", r, e, syscall.ENOMEM)
	r, e = scr(SYS_MADVISE, kernHalf, 0, syscall.MADV_DONTNEED)
	t.want("madvise len 0", r, e, 0)

	// mm/mincore.c sys_mincore.
	vec := junk(randFor(1), 64)
	r, e = scr(SYS_MINCORE, mmBase+1, 4096, vec)
	t.want("mincore unaligned", r, e, syscall.EINVAL)
	// "if (!access_ok(start, len)) return -ENOMEM".
	r, e = scr(SYS_MINCORE, kernHalf, 4096, vec)
	t.want("mincore kernel", r, e, syscall.ENOMEM)
	// "if (!access_ok(vec, pages)) return -EFAULT".
	r, e = scr(SYS_MINCORE, mmBase, 4096, kernHalf)
	t.want("mincore vec kernel", r, e, syscall.EFAULT)
	// do_mincore: no VMA → -ENOMEM.
	r, e = scr(SYS_MINCORE, holePage, 4096, vec)
	t.want("mincore hole", r, e, syscall.ENOMEM)
	r, e = scr(SYS_MINCORE, mmBase, 4096, holePage)
	t.want("mincore vec hole", r, e, syscall.EFAULT)
	// A 16 TiB PROT_NONE reservation: mincore walks it 4096 pages per
	// step and copies each step out; the junk area (2 MiB, then a guard)
	// runs out first → -EFAULT. The kernel must not try to build the
	// whole 4 Gi-entry vector at once.
	big, e := scr(SYS_MMAP, 0, 16<<40, syscall.PROT_NONE, anon|noRes, allOnes, 0)
	if e == 0 {
		if *skipFlag != "mincore-huge" { // -skip mincore-huge: a kernel that panics here
			r, e = scr(SYS_MINCORE, big, 16<<40, rwBase)
			t.want("mincore 16 TiB into 2 MiB", r, e, syscall.EFAULT)
		}
		r, e = scr(SYS_MADVISE, big, 16<<40, syscall.MADV_DONTNEED)
		t.want("madvise 16 TiB reservation", r, e, 0)
		r, e = scr(SYS_MPROTECT, big, 16<<40, syscall.PROT_NONE)
		t.want("mprotect 16 TiB reservation", r, e, 0)
		r, e = scr(SYS_MUNMAP, big, 16<<40)
		t.want("munmap 16 TiB reservation", r, e, 0)
	} else {
		t.check("mmap 16 TiB reservation", false, e.Error())
	}

	// mm/mremap.c check_mremap_params (after PAGE_ALIGN of both lengths).
	// Two RW pages between PROT_NONE neighbours (no VMA merging).
	m := must("mmap mremap src")(scr(SYS_MMAP, mmBase+16*4096, 2*4096, rw, anon|mapFixed, allOnes, 0))
	r, e = scr(SYS_MREMAP, m, 4096, 4096, 0x80)
	t.want("mremap bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_MREMAP, m+1, 4096, 4096, 0)
	t.want("mremap unaligned", r, e, syscall.EINVAL)
	r, e = scr(SYS_MREMAP, m, 4096, 0, 0)
	t.want("mremap new_len 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_MREMAP, m, 4096, allOnes-10, 0) // PAGE_ALIGN → 0
	t.want("mremap new_len wraps", r, e, syscall.EINVAL)
	r, e = scr(SYS_MREMAP, m, 4096, 1<<62, 1) // "new_len > TASK_SIZE"
	t.want("mremap new_len > TASK_SIZE", r, e, syscall.EINVAL)
	// MREMAP_FIXED without MREMAP_MAYMOVE → -EINVAL.
	r, e = scr(SYS_MREMAP, m, 4096, 4096, 2, mmBase)
	t.want("mremap fixed without maymove", r, e, syscall.EINVAL)
	// do_mremap: vma_lookup fails → -EFAULT.
	r, e = scr(SYS_MREMAP, holePage, 4096, 8192, 1)
	t.want("mremap hole", r, e, syscall.EFAULT)
	// old range runs past the VMA → -EFAULT ("if (old_len > vma->vm_end - addr) return -EFAULT").
	r, e = scr(SYS_MREMAP, m, 16*4096, 32*4096, 1)
	t.want("mremap old range past vma", r, e, syscall.EFAULT)
	// Restore the arena's PROT_NONE reservation.
	scr(SYS_MMAP, m, 2*4096, syscall.PROT_NONE, anon|mapFixed|noRes, allOnes, 0)

	// mm/msync.c: unknown flags / MS_ASYNC|MS_SYNC / unaligned → -EINVAL;
	// a hole in the range → -ENOMEM.
	r, e = scr(SYS_MSYNC, rwBase, 4096, 0x100)
	t.want("msync bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_MSYNC, rwBase, 4096, syscall.MS_ASYNC|syscall.MS_SYNC)
	t.want("msync async|sync", r, e, syscall.EINVAL)
	r, e = scr(SYS_MSYNC, rwBase+1, 4096, syscall.MS_SYNC)
	t.want("msync unaligned", r, e, syscall.EINVAL)
	r, e = scr(SYS_MSYNC, holePage, 4096, syscall.MS_SYNC)
	t.want("msync hole", r, e, syscall.ENOMEM)
	r, e = scr(SYS_MSYNC, rwBase, 4096, syscall.MS_SYNC)
	t.want("msync ok", r, e, E)
	// mm/mlock.c: a hole → -ENOMEM; mlockall(0) / unknown flags → -EINVAL.
	r, e = scr(SYS_MLOCK, holePage, 4096)
	t.want("mlock hole", r, e, syscall.ENOMEM)
	r, e = scr(SYS_MLOCKALL, 0)
	t.want("mlockall 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_MLOCK2, rwBase, 4096, 0x80)
	t.want("mlock2 bad flags", r, e, syscall.EINVAL)
}

func casesFd(t *tcase) {
	// fs/file.c close_fd: no file → -EBADF.
	for _, fd := range []int64{-1, -100, 1 << 20, 1<<31 - 1, 1<<32 + 1000} {
		r, e := scr(SYS_CLOSE, u(fd))
		t.want(fmt.Sprintf("close %d", fd), r, e, syscall.EBADF)
	}
	// fs/file.c ksys_dup3: "if ((flags & ~O_CLOEXEC) != 0) return -EINVAL;
	// if (unlikely(oldfd == newfd)) return -EINVAL; if (newfd >= rlimit(RLIMIT_NOFILE)) return -EBADF".
	r, e := scr(SYS_DUP3, 0, 0, 0)
	t.want("dup3 old == new", r, e, syscall.EINVAL)
	r, e = scr(SYS_DUP3, 0, 50, 0x1)
	t.want("dup3 bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_DUP3, 0, 1<<20, 0)
	t.want("dup3 new >= NOFILE", r, e, syscall.EBADF)
	r, e = scr(SYS_DUP3, 0, u(-5), 0)
	t.want("dup3 new negative", r, e, syscall.EBADF)
	r, e = scr(SYS_DUP3, u(-1), 50, 0)
	t.want("dup3 bad old", r, e, syscall.EBADF)
	r, e = scr(SYS_DUP, 999)
	t.want("dup closed", r, e, syscall.EBADF)
	// fs/fcntl.c: F_DUPFD with arg >= NOFILE → -EINVAL (f_dupfd); bad fd → -EBADF.
	r, e = scr(SYS_FCNTL, 0, syscall.F_DUPFD, 1<<20)
	t.want("fcntl F_DUPFD >= NOFILE", r, e, syscall.EINVAL)
	r, e = scr(SYS_FCNTL, u(-1), syscall.F_GETFD, 0)
	t.want("fcntl bad fd", r, e, syscall.EBADF)
	r, e = scr(SYS_FCNTL, 0, 9999, 0)
	t.want("fcntl bad cmd", r, e, syscall.EINVAL)
	// F_SETLK with a bad struct flock pointer → -EFAULT (copy_from_user).
	fd := openPath(t, "/tmp/sfc/lock", syscall.O_RDWR|syscall.O_CREAT)
	r, e = scr(SYS_FCNTL, fd, syscall.F_SETLK, kernHalf)
	t.want("fcntl F_SETLK bad ptr", r, e, syscall.EFAULT)
	// fs/open.c close_range: "if (flags & ~(CLOSE_RANGE_UNSHARE|CLOSE_RANGE_CLOEXEC)) return -EINVAL; if (fd > max_fd) return -EINVAL".
	r, e = scr(SYS_CLOSE_RANGE, 10, 5, 0)
	t.want("close_range first > last", r, e, syscall.EINVAL)
	r, e = scr(SYS_CLOSE_RANGE, 1000, 2000, 0x80)
	t.want("close_range bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_CLOSE_RANGE, 1<<20, allOnes, 0)
	t.want("close_range above every fd", r, e, 0)

	// fs/read_write.c lseek: "if (whence > SEEK_MAX) → -EINVAL"; on a
	// pipe → -ESPIPE (no_llseek); a negative result → -EINVAL.
	r, e = scr(SYS_LSEEK, fd, 0, 99)
	t.want("lseek bad whence", r, e, syscall.EINVAL)
	r, e = scr(SYS_LSEEK, fd, u(-10), 0)
	t.want("lseek negative", r, e, syscall.EINVAL)
	pr, pw := pipe(t)
	r, e = scr(SYS_LSEEK, pr, 0, 0)
	t.want("lseek pipe", r, e, syscall.ESPIPE)
	// ftruncate: "if (length < 0) return -EINVAL"; non-regular or not
	// open for writing → -EINVAL (do_ftruncate).
	r, e = scr(SYS_FTRUNCATE, fd, u(-1))
	t.want("ftruncate negative", r, e, syscall.EINVAL)
	r, e = scr(SYS_FTRUNCATE, pr, 0)
	t.want("ftruncate pipe", r, e, syscall.EINVAL)
	// vfs_fallocate: "offset < 0 || len <= 0 → -EINVAL"; unknown mode →
	// -EOPNOTSUPP; "offset + len > s_maxbytes || < 0 → -EFBIG".
	r, e = scr(SYS_FALLOCATE, fd, 0, 0, 0)
	t.want("fallocate len 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_FALLOCATE, fd, 0, 1<<62, 1<<62)
	t.want("fallocate overflow", r, e, syscall.EFBIG)
	// fs/pipe.c do_pipe2: unknown flags → -EINVAL; a bad fds pointer → -EFAULT
	// and both fds are closed again.
	r, e = scr(SYS_PIPE2, t.c.alloc(8), 0x1)
	t.want("pipe2 bad flags", r, e, syscall.EINVAL)
	before := countFds()
	r, e = scr(SYS_PIPE2, kernHalf, 0)
	t.want("pipe2 bad pointer", r, e, syscall.EFAULT)
	t.check("pipe2 bad pointer leaks no fd", countFds() == before,
		fmt.Sprintf("fds %d → %d", before, countFds()))
	r, e = scr(SYS_PIPE2, roPage, 0)
	t.want("pipe2 read-only pointer", r, e, syscall.EFAULT)
	// fs/eventfd.c: unknown flags → -EINVAL; read/write < 8 bytes →
	// -EINVAL; writing ULLONG_MAX → -EINVAL.
	r, e = scr(SYS_EVENTFD2, 0, 0x1)
	t.want("eventfd2 bad flags", r, e, syscall.EINVAL)
	efd := must("eventfd2")(scr(SYS_EVENTFD2, 0, syscall.O_NONBLOCK|syscall.O_CLOEXEC))
	r, e = scr(SYS_WRITE, efd, t.c.u64s(1), 4)
	t.want("eventfd write 4 bytes", r, e, syscall.EINVAL)
	r, e = scr(SYS_WRITE, efd, t.c.u64s(^uint64(0)), 8)
	t.want("eventfd write ULLONG_MAX", r, e, syscall.EINVAL)
	r, e = scr(SYS_READ, efd, junk(randFor(2), 8), 7)
	t.want("eventfd read 7 bytes", r, e, syscall.EINVAL)
	r, e = scr(SYS_READ, efd, junk(randFor(2), 8), 8)
	t.want("eventfd read empty nonblocking", r, e, syscall.EAGAIN)
	r, e = scr(SYS_WRITE, efd, kernHalf, 8)
	t.want("eventfd write bad pointer", r, e, syscall.EFAULT)
	// fs/readdir.c getdents64: on a non-directory → -ENOTDIR; a buffer too
	// small for one entry → -EINVAL; a bad buffer → -EFAULT (access_ok).
	r, e = scr(SYS_GETDENTS64, fd, junk(randFor(3), 4096), 4096)
	t.want("getdents64 on file", r, e, syscall.ENOTDIR)
	dfd := openPath(t, "/", syscall.O_RDONLY|syscall.O_DIRECTORY)
	r, e = scr(SYS_GETDENTS64, dfd, junk(randFor(3), 4096), 8)
	t.want("getdents64 tiny buffer", r, e, syscall.EINVAL)
	r, e = scr(SYS_GETDENTS64, dfd, kernHalf, 4096)
	t.want("getdents64 kernel buffer", r, e, syscall.EFAULT)
	r, e = scr(SYS_GETDENTS64, u(-1), junk(randFor(3), 4096), 4096)
	t.want("getdents64 bad fd", r, e, syscall.EBADF)
	// fs/ioctl.c: FIONBIO with a bad pointer → -EFAULT; a bad fd → -EBADF.
	r, e = scr(SYS_IOCTL, pr, FIONBIO, kernHalf)
	t.want("ioctl FIONBIO bad ptr", r, e, syscall.EFAULT)
	r, e = scr(SYS_IOCTL, u(-1), FIONREAD, junk(randFor(4), 4))
	t.want("ioctl bad fd", r, e, syscall.EBADF)
	r, e = scr(SYS_IOCTL, pr, FIONREAD, kernHalf)
	t.want("ioctl FIONREAD bad ptr", r, e, syscall.EFAULT)
	r, e = scr(SYS_FSTAT, fd, kernHalf)
	t.want("fstat bad ptr", r, e, syscall.EFAULT)
	r, e = scr(SYS_FSTAT, fd, roPage)
	t.want("fstat read-only ptr", r, e, syscall.EFAULT)
	for _, f := range []uintptr{fd, pr, pw, efd, dfd} {
		closeFd(f)
	}
}

// countFds counts open fds below 4096.
func countFds() int {
	n := 0
	for fd := 0; fd < 4096; fd++ {
		if _, e := scr(SYS_FCNTL, uintptr(fd), syscall.F_GETFD, 0); e == 0 {
			n++
		}
	}
	return n
}

func casesRW(t *tcase) {
	pr, pw := pipe(t)
	buf := junk(randFor(5), 4096)
	// fs/read_write.c vfs_read/vfs_write: "if (unlikely(!access_ok(buf,
	// count))) return -EFAULT" before the file is touched.
	r, e := scr(SYS_READ, u(-1), buf, 1)
	t.want("read bad fd", r, e, syscall.EBADF)
	r, e = scr(SYS_WRITE, pr, buf, 1)
	t.want("write to read end", r, e, syscall.EBADF)
	r, e = scr(SYS_READ, pw, buf, 1)
	t.want("read from write end", r, e, syscall.EBADF)
	r, e = scr(SYS_WRITE, pw, kernHalf, 1)
	t.want("write kernel buffer", r, e, syscall.EFAULT)
	r, e = scr(SYS_WRITE, pw, buf, 1<<62)
	t.want("write huge count", r, e, syscall.EFAULT)
	// Drain first: a kernel that wrote before checking the huge count
	// above has filled the ring (the case fails; the setup must not).
	for {
		if _, e := scr(SYS_READ, pr, junk(randFor(3), 4096), 4096); e != 0 {
			break
		}
	}
	must("write x")(scr(SYS_WRITE, pw, t.c.cstr("xy"), 2))
	r, e = scr(SYS_READ, pr, kernHalf, 1)
	t.want("read kernel buffer", r, e, syscall.EFAULT)
	r, e = scr(SYS_READ, pr, buf, 1<<62)
	t.want("read huge count", r, e, syscall.EFAULT)
	// fs/pipe.c pipe_read: a copy that faults on its first byte returns
	// -EFAULT and leaves the data in the pipe.
	r, e = scr(SYS_READ, pr, holePage, 1)
	t.want("read into hole", r, e, syscall.EFAULT)
	r, e = scr(SYS_READ, pr, roPage, 1)
	t.want("read into read-only page", r, e, syscall.EFAULT)
	r, e = scr(SYS_READ, pr, buf, 16)
	t.wantRet("read after faults keeps data", r, e, 2)
	// A zero-length read with a bad pointer: access_ok(p, 0) passes for a
	// user address, nothing is copied.
	r, e = scr(SYS_READ, pr, holePage, 0)
	t.want("read 0 bytes into hole", r, e, 0)

	// /dev/zero (drivers/char/mem.c read_iter_zero): a buffer that runs
	// into the guard page gets the bytes before it.
	zfd := openPath(t, "/dev/zero", syscall.O_RDONLY)
	r, e = scr(SYS_READ, zfd, rwEnd-100, 4096)
	t.wantRet("read /dev/zero up to the guard page", r, e, 100)
	r, e = scr(SYS_READ, zfd, guardPage, 4096)
	t.want("read /dev/zero into guard page", r, e, syscall.EFAULT)
	// pread64: "if (pos < 0) return -EINVAL" before fdget; a pipe → -ESPIPE.
	r, e = scr(SYS_PREAD64, u(-1), buf, 1, u(-1))
	t.want("pread64 negative offset bad fd", r, e, syscall.EINVAL)
	r, e = scr(SYS_PREAD64, pr, buf, 1, 0)
	t.want("pread64 pipe", r, e, syscall.ESPIPE)
	r, e = scr(SYS_PWRITE64, pw, buf, 1, u(-5))
	t.want("pwrite64 negative offset", r, e, syscall.EINVAL)

	// readv/writev (lib/iov_iter.c __import_iovec): nr_segs > UIO_MAXIOV →
	// -EINVAL; a negative iov_len → -EINVAL; a segment failing access_ok →
	// -EFAULT; nr_segs 0 → 0.
	iov := t.c.u64s(uint64(buf), 8, uint64(buf+8), 8)
	r, e = scr(SYS_READV, pr, iov, 1025)
	t.want("readv 1025 segments", r, e, syscall.EINVAL)
	r, e = scr(SYS_READV, pr, iov, 0)
	t.want("readv 0 segments", r, e, 0)
	r, e = scr(SYS_READV, pr, kernHalf, 2)
	t.want("readv iov in kernel", r, e, syscall.EFAULT)
	r, e = scr(SYS_WRITEV, pw, t.c.u64s(uint64(buf), 1<<63), 1)
	t.want("writev negative iov_len", r, e, syscall.EINVAL)
	r, e = scr(SYS_WRITEV, pw, t.c.u64s(uint64(buf), 8, kernHalf, 8), 2)
	t.want("writev kernel segment", r, e, syscall.EFAULT)
	r, e = scr(SYS_WRITEV, pw, t.c.u64s(uint64(buf), 8, uint64(buf), 1<<62), 2)
	t.want("writev huge segment", r, e, syscall.EFAULT)
	r, e = scr(SYS_READ, pr, buf, 16)
	t.want("pipe empty after failed writev", r, e, syscall.EAGAIN)
	// Two segments whose lengths add past MAX_RW_COUNT are clamped, not
	// overflowed: writing 2 GiB of zeros from /dev/zero-backed... the
	// pipe takes 64 KiB and the call returns that.
	big := t.c.u64s(uint64(rwBase), 1<<30, uint64(rwBase), 1<<30)
	_ = big // (covered by the fuzzer; a full write here would need 2 GiB mapped)

	// sendfile (fs/read_write.c do_sendfile): an offset pointer that faults
	// → -EFAULT; a negative *offset → -EINVAL.
	ffd := openPath(t, "/tmp/sfc/sf", syscall.O_RDWR|syscall.O_CREAT|syscall.O_TRUNC)
	must("write sf")(scr(SYS_WRITE, ffd, t.c.cstr("hello"), 5))
	r, e = scr(SYS_SENDFILE, pw, ffd, kernHalf, 5)
	t.want("sendfile bad offset ptr", r, e, syscall.EFAULT)
	r, e = scr(SYS_SENDFILE, pw, ffd, t.c.u64s(^uint64(0)), 5)
	t.want("sendfile negative offset", r, e, syscall.EINVAL)
	r, e = scr(SYS_SENDFILE, u(-1), ffd, 0, 5)
	t.want("sendfile bad out fd", r, e, syscall.EBADF)
	// A regular file read up to an unmapped page: generic filemap_read
	// returns the bytes copied before the fault.
	r, e = scr(SYS_PREAD64, ffd, rwEnd-3, 5, 0)
	t.wantRet("pread64 up to the guard page", r, e, 3)
	// ... and the write side (generic_perform_write) the bytes before it.
	r, e = scr(SYS_PWRITE64, ffd, rwEnd-2, 5, 0)
	t.wantRet("pwrite64 up to the guard page", r, e, 2)
	for _, f := range []uintptr{pr, pw, zfd, ffd} {
		closeFd(f)
	}
}

func casesPath(t *tcase) {
	c := t.c
	// fs/namei.c getname: NULL / kernel pointers → -EFAULT; an empty name →
	// -ENOENT; no NUL within PATH_MAX → -ENAMETOOLONG.
	r, e := scr(SYS_OPENAT, u(-100), 0, syscall.O_RDONLY, 0)
	t.want("openat NULL path", r, e, syscall.EFAULT)
	r, e = scr(SYS_OPENAT, u(-100), kernHalf, syscall.O_RDONLY, 0)
	t.want("openat kernel path", r, e, syscall.EFAULT)
	r, e = scr(SYS_OPENAT, u(-100), c.cstr(""), syscall.O_RDONLY, 0)
	t.want("openat empty path", r, e, syscall.ENOENT)
	long := c.alloc(8192)
	for i := 0; i < 8192; i++ {
		mem(long, 8192)[i] = '/'
	}
	r, e = scr(SYS_OPENAT, u(-100), long, syscall.O_RDONLY, 0)
	t.want("openat no NUL in PATH_MAX", r, e, syscall.ENAMETOOLONG)
	mem(long, 8192)[4095] = 0 // 4095 slashes + NUL: fits, resolves to "/"
	r, e = scr(SYS_OPENAT, u(-100), long, syscall.O_RDONLY|syscall.O_CLOEXEC, 0)
	t.want("openat 4095-byte path", r, e, 0)
	if e == 0 {
		closeFd(r)
	}
	mem(long, 8192)[4095] = '/'
	mem(long, 8192)[4096] = 0 // 4096 + NUL: one too many
	r, e = scr(SYS_OPENAT, u(-100), long, syscall.O_RDONLY, 0)
	t.want("openat 4096-byte path", r, e, syscall.ENAMETOOLONG)
	// A string running into the guard page without a NUL → -EFAULT.
	copy(mem(rwEnd-4, 4), "abcd")
	r, e = scr(SYS_OPENAT, u(-100), rwEnd-4, syscall.O_RDONLY, 0)
	t.want("openat path runs into guard", r, e, syscall.EFAULT)
	// A 256-byte component → -ENAMETOOLONG (NAME_MAX, tmpfs).
	r, e = scr(SYS_OPENAT, u(-100), c.cstr("/tmp/sfc/"+strings.Repeat("n", 256)), syscall.O_RDONLY|syscall.O_CREAT, 0o644)
	t.want("openat 256-byte component", r, e, syscall.ENAMETOOLONG)
	// A bad dirfd with a relative path → -EBADF; with an absolute path the
	// dirfd is ignored; a non-directory dirfd → -ENOTDIR.
	r, e = scr(SYS_OPENAT, 9999, c.cstr("x"), syscall.O_RDONLY, 0)
	t.want("openat bad dirfd relative", r, e, syscall.EBADF)
	r, e = scr(SYS_OPENAT, 9999, c.cstr("/"), syscall.O_RDONLY|syscall.O_CLOEXEC, 0)
	t.want("openat bad dirfd absolute", r, e, 0)
	if e == 0 {
		closeFd(r)
	}
	ffd := openPath(t, "/tmp/sfc/f", syscall.O_RDWR|syscall.O_CREAT)
	r, e = scr(SYS_OPENAT, ffd, c.cstr("x"), syscall.O_RDONLY, 0)
	t.want("openat file as dirfd", r, e, syscall.ENOTDIR)
	// A symlink loop → -ELOOP.
	scr(SYS_SYMLINKAT, c.cstr("/tmp/sfc/loopb"), u(-100), c.cstr("/tmp/sfc/loopa"))
	scr(SYS_SYMLINKAT, c.cstr("/tmp/sfc/loopa"), u(-100), c.cstr("/tmp/sfc/loopb"))
	r, e = scr(SYS_OPENAT, u(-100), c.cstr("/tmp/sfc/loopa"), syscall.O_RDONLY, 0)
	t.want("openat symlink loop", r, e, syscall.ELOOP)
	// fs/stat.c do_readlinkat: "if (bufsiz <= 0) return -EINVAL"; not a
	// link → -EINVAL.
	r, e = scr(SYS_READLINKAT, u(-100), c.cstr("/tmp/sfc/loopa"), junk(randFor(6), 64), 0)
	t.want("readlinkat bufsiz 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_READLINKAT, u(-100), c.cstr("/tmp/sfc/loopa"), junk(randFor(6), 64), u(-1))
	t.want("readlinkat bufsiz negative", r, e, syscall.EINVAL)
	r, e = scr(SYS_READLINKAT, u(-100), c.cstr("/tmp/sfc/f"), junk(randFor(6), 64), 64)
	t.want("readlinkat not a link", r, e, syscall.EINVAL)
	r, e = scr(SYS_READLINKAT, u(-100), c.cstr("/tmp/sfc/loopa"), kernHalf, 64)
	t.want("readlinkat kernel buffer", r, e, syscall.EFAULT)
	// fs/d_path.c getcwd: buffer too small → -ERANGE; bad buffer → -EFAULT.
	r, e = scr(SYS_GETCWD, junk(randFor(7), 8), 1)
	t.want("getcwd size 1", r, e, syscall.ERANGE)
	r, e = scr(SYS_GETCWD, junk(randFor(7), 8), 0)
	t.want("getcwd size 0", r, e, syscall.ERANGE)
	r, e = scr(SYS_GETCWD, kernHalf, 4096)
	t.want("getcwd kernel buffer", r, e, syscall.EFAULT)
	// fs/namei.c do_unlinkat / do_renameat2 / do_linkat flag checks.
	r, e = scr(SYS_UNLINKAT, u(-100), c.cstr("/tmp/sfc/f"), 0x1)
	t.want("unlinkat bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_RENAMEAT2, u(-100), c.cstr("/tmp/sfc/f"), u(-100), c.cstr("/tmp/sfc/g"), 0x80)
	t.want("renameat2 bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_RENAMEAT2, u(-100), c.cstr("/tmp/sfc/f"), u(-100), c.cstr("/tmp/sfc/g"), 1|2)
	t.want("renameat2 noreplace|exchange", r, e, syscall.EINVAL)
	r, e = scr(SYS_LINKAT, u(-100), c.cstr("/tmp/sfc/f"), u(-100), c.cstr("/tmp/sfc/g"), 0x1)
	t.want("linkat bad flags", r, e, syscall.EINVAL)
	// fs/open.c do_faccessat: "if (mode & ~S_IRWXO) return -EINVAL"; bad flags → -EINVAL.
	r, e = scr(SYS_FACCESSAT2, u(-100), c.cstr("/"), 8, 0)
	t.want("faccessat2 bad mode", r, e, syscall.EINVAL)
	r, e = scr(SYS_FACCESSAT2, u(-100), c.cstr("/"), 0, 0x1)
	t.want("faccessat2 bad flags", r, e, syscall.EINVAL)
	// fs/stat.c: newfstatat / statx with unknown flags → -EINVAL; a bad
	// stat buffer → -EFAULT.
	r, e = scr(SYS_NEWFSTATAT, u(-100), c.cstr("/"), junk(randFor(8), 256), 0x1)
	t.want("newfstatat bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_NEWFSTATAT, u(-100), c.cstr("/"), kernHalf, 0)
	t.want("newfstatat kernel buffer", r, e, syscall.EFAULT)
	r, e = scr(SYS_STATX, u(-100), c.cstr("/"), 0x1, 0x7ff, junk(randFor(8), 256))
	t.want("statx bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_STATX, u(-100), c.cstr("/"), 0, 0x7ff, kernHalf)
	t.want("statx kernel buffer", r, e, syscall.EFAULT)
	// fs/utimes.c: tv_nsec outside [0, 1e9) and not UTIME_NOW/OMIT → -EINVAL;
	// utimensat(AT_FDCWD, NULL, ...) → getname(NULL) → -EFAULT.
	r, e = scr(SYS_UTIMENSAT, u(-100), c.cstr("/tmp/sfc/f"), c.timespec(0, 1e9, 0, 0), 0)
	t.want("utimensat bad nsec", r, e, syscall.EINVAL)
	r, e = scr(SYS_UTIMENSAT, u(-100), c.cstr("/tmp/sfc/f"), c.timespec(0, -1, 0, 0), 0)
	t.want("utimensat negative nsec", r, e, syscall.EINVAL)
	r, e = scr(SYS_UTIMENSAT, u(-100), 0, 0, 0)
	t.want("utimensat AT_FDCWD NULL path", r, e, syscall.EFAULT)
	r, e = scr(SYS_UTIMENSAT, ffd, 0, 0, 0x100)
	t.want("utimensat fd NULL path with flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_UTIMENSAT, u(-100), c.cstr("/tmp/sfc/f"), kernHalf, 0)
	t.want("utimensat bad times ptr", r, e, syscall.EFAULT)
	// fs/open.c do_sys_truncate: "if (length < 0) return -EINVAL".
	r, e = scr(SYS_TRUNCATE, c.cstr("/tmp/sfc/f"), u(-1))
	t.want("truncate negative", r, e, syscall.EINVAL)
	r, e = scr(SYS_MKDIRAT, u(-100), c.cstr("/tmp/sfc"), 0o755)
	t.want("mkdirat existing", r, e, syscall.EEXIST)
	closeFd(ffd)
}

func casesPoll(t *tcase) {
	c := t.c
	ep := must("epoll_create1")(scr(SYS_EPOLL_CREATE1, syscall.EPOLL_CLOEXEC))
	pr, pw := pipe(t)
	evs := junk(randFor(9), 4096)
	// fs/eventpoll.c do_epoll_wait: "maxevents <= 0 || > EP_MAX_EVENTS →
	// -EINVAL", then "!access_ok(events, ...) → -EFAULT", then fdget → -EBADF,
	// then not an epoll file → -EINVAL.
	r, e := scr(SYS_EPOLL_PWAIT, ep, evs, 0, 0, 0, 8)
	t.want("epoll_pwait maxevents 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_EPOLL_PWAIT, ep, evs, u(-1), 0, 0, 8)
	t.want("epoll_pwait maxevents -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_EPOLL_PWAIT, ep, evs, 1<<30, 0, 0, 8)
	t.want("epoll_pwait maxevents > EP_MAX_EVENTS", r, e, syscall.EINVAL)
	r, e = scr(SYS_EPOLL_PWAIT, u(-1), kernHalf, 1, 0, 0, 8)
	t.want("epoll_pwait kernel events before fd", r, e, syscall.EFAULT)
	r, e = scr(SYS_EPOLL_PWAIT, u(-1), evs, 1, 0, 0, 8)
	t.want("epoll_pwait bad epfd", r, e, syscall.EBADF)
	r, e = scr(SYS_EPOLL_PWAIT, pr, evs, 1, 0, 0, 8)
	t.want("epoll_pwait not epoll", r, e, syscall.EINVAL)
	// epoll_pwait with a sigmask of the wrong size → -EINVAL (set_user_sigmask).
	r, e = scr(SYS_EPOLL_PWAIT, ep, evs, 1, 0, c.u64s(0), 4)
	t.want("epoll_pwait sigsetsize 4", r, e, syscall.EINVAL)
	// epoll_pwait2: an invalid timespec → -EINVAL.
	r, e = scr(SYS_EPOLL_PWAIT2, ep, evs, 1, c.timespec(0, -1), 0, 8)
	t.want("epoll_pwait2 bad timeout", r, e, syscall.EINVAL)
	r, e = scr(SYS_EPOLL_PWAIT2, ep, evs, 1, kernHalf, 0, 8)
	t.want("epoll_pwait2 kernel timeout", r, e, syscall.EFAULT)
	// epoll_ctl: the event is copied first ("ep_op_has_event(op) &&
	// copy_from_user → -EFAULT"); then fdget → -EBADF; unpollable file →
	// -EPERM; epfd == fd or epfd not epoll → -EINVAL; bad op → -EINVAL.
	ev := c.alloc(16)
	put32(ev, uint32(syscall.EPOLLIN))
	r, e = scr(SYS_EPOLL_CTL, u(-1), syscall.EPOLL_CTL_ADD, pr, kernHalf)
	t.want("epoll_ctl bad event before fd", r, e, syscall.EFAULT)
	r, e = scr(SYS_EPOLL_CTL, ep, syscall.EPOLL_CTL_ADD, u(-1), ev)
	t.want("epoll_ctl bad fd", r, e, syscall.EBADF)
	r, e = scr(SYS_EPOLL_CTL, ep, syscall.EPOLL_CTL_ADD, ep, ev)
	t.want("epoll_ctl add self", r, e, syscall.EINVAL)
	r, e = scr(SYS_EPOLL_CTL, pw, syscall.EPOLL_CTL_ADD, pr, ev)
	t.want("epoll_ctl on non-epoll", r, e, syscall.EINVAL)
	r, e = scr(SYS_EPOLL_CTL, ep, 99, pr, ev)
	t.want("epoll_ctl bad op", r, e, syscall.EINVAL)
	reg := openPath(t, "/tmp/sfc/f", syscall.O_RDWR|syscall.O_CREAT)
	r, e = scr(SYS_EPOLL_CTL, ep, syscall.EPOLL_CTL_ADD, reg, ev)
	t.want("epoll_ctl regular file", r, e, syscall.EPERM)
	r, e = scr(SYS_EPOLL_CTL, ep, syscall.EPOLL_CTL_DEL, pr, 0)
	t.want("epoll_ctl del not added", r, e, syscall.ENOENT)
	r, e = scr(SYS_EPOLL_CTL, ep, syscall.EPOLL_CTL_ADD, pr, ev)
	t.want("epoll_ctl add", r, e, 0)
	r, e = scr(SYS_EPOLL_CTL, ep, syscall.EPOLL_CTL_ADD, pr, ev)
	t.want("epoll_ctl add twice", r, e, syscall.EEXIST)
	r, e = scr(SYS_EPOLL_CREATE1, 0x1)
	t.want("epoll_create1 bad flags", r, e, syscall.EINVAL)
	// An event is ready, but the events buffer faults: Linux re-queues the
	// event and returns -EFAULT; a later wait still sees it.
	must("write pipe")(scr(SYS_WRITE, pw, c.cstr("z"), 1))
	r, e = scr(SYS_EPOLL_PWAIT, ep, holePage, 4, 0, 0, 8)
	t.want("epoll_pwait events into hole", r, e, syscall.EFAULT)
	r, e = scr(SYS_EPOLL_PWAIT, ep, evs, 4, 0, 0, 8)
	t.wantRet("epoll_pwait after fault still reports", r, e, 1)

	// fs/select.c: ppoll timespec → -EFAULT / -EINVAL; a bad sigsetsize →
	// -EINVAL (set_user_sigmask) before the fds are read; nfds >
	// RLIMIT_NOFILE → -EINVAL; a bad fds pointer → -EFAULT; a closed fd →
	// POLLNVAL.
	pfd := c.alloc(16)
	put32(pfd, uint32(pr))
	put16(pfd+4, uint16(syscall.EPOLLIN))
	r, e = scr(SYS_PPOLL, pfd, 1, c.timespec(0, 1e9), 0, 8)
	t.want("ppoll bad timeout", r, e, syscall.EINVAL)
	r, e = scr(SYS_PPOLL, pfd, 1, kernHalf, 0, 8)
	t.want("ppoll kernel timeout", r, e, syscall.EFAULT)
	r, e = scr(SYS_PPOLL, kernHalf, 1, c.timespec(0, 0), c.u64s(0), 4)
	t.want("ppoll bad sigsetsize first", r, e, syscall.EINVAL)
	r, e = scr(SYS_PPOLL, pfd, 1<<20+1, c.timespec(0, 0), 0, 8)
	t.want("ppoll nfds > NOFILE", r, e, syscall.EINVAL)
	r, e = scr(SYS_PPOLL, kernHalf, 1, c.timespec(0, 0), 0, 8)
	t.want("ppoll kernel fds", r, e, syscall.EFAULT)
	put32(pfd+8, 777)
	put16(pfd+12, uint16(syscall.EPOLLIN))
	r, e = scr(SYS_PPOLL, pfd, 2, c.timespec(0, 0), 0, 8)
	t.check("ppoll closed fd → POLLNVAL", e == 0 && r == 2 && mem(pfd+14, 2)[0] == 0x20,
		fmt.Sprintf("r=%d e=%v revents=%#x", r, e, mem(pfd+14, 2)[0]))
	put32(pfd, ^uint32(0)) // fd -1: ignored, revents 0
	r, e = scr(SYS_PPOLL, pfd, 1, c.timespec(0, 0), 0, 8)
	t.wantRet("ppoll negative fd ignored", r, e, 0)
	// pselect6: nfds < 0 → -EINVAL; a set bit for a closed fd → -EBADF.
	set := c.u64s(1 << 50)
	r, e = scr(SYS_PSELECT6, u(-1), set, 0, 0, c.timespec(0, 0), 0)
	t.want("pselect6 nfds negative", r, e, syscall.EINVAL)
	r, e = scr(SYS_PSELECT6, 64, set, 0, 0, c.timespec(0, 0), 0)
	t.want("pselect6 closed fd", r, e, syscall.EBADF)
	r, e = scr(SYS_PSELECT6, 64, kernHalf, 0, 0, c.timespec(0, 0), 0)
	t.want("pselect6 kernel set", r, e, syscall.EFAULT)
	for _, f := range []uintptr{ep, pr, pw, reg} {
		closeFd(f)
	}
}

func sockaddrIn(c *cons, port uint16, ip [4]byte) uintptr {
	p := c.alloc(16)
	put16(p, syscall.AF_INET)
	mem(p+2, 2)[0], mem(p+2, 2)[1] = byte(port>>8), byte(port)
	copy(mem(p+4, 4), ip[:])
	return p
}

func casesNet(t *tcase) {
	c := t.c
	nb := uintptr(syscall.SOCK_NONBLOCK | syscall.SOCK_CLOEXEC)
	// net/socket.c __sys_socket: unknown type flags → -EINVAL;
	// __sock_create: family >= NPROTO → -EAFNOSUPPORT.
	r, e := scr(SYS_SOCKET, syscall.AF_INET, syscall.SOCK_STREAM|0x100000, 0)
	t.want("socket bad type flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_SOCKET, 9999, syscall.SOCK_STREAM, 0)
	t.want("socket bad family", r, e, syscall.EAFNOSUPPORT)
	r, e = scr(SYS_SOCKET, u(-1), syscall.SOCK_STREAM, 0)
	t.want("socket family -1", r, e, syscall.EAFNOSUPPORT)
	s := must("socket udp")(scr(SYS_SOCKET, syscall.AF_INET, syscall.SOCK_DGRAM|nb, 0))
	ts := must("socket tcp")(scr(SYS_SOCKET, syscall.AF_INET, syscall.SOCK_STREAM|nb, 0))
	// move_addr_to_kernel: "ulen < 0 || ulen > sizeof(sockaddr_storage) →
	// -EINVAL"; a bad pointer → -EFAULT. sockfd_lookup first: -EBADF / -ENOTSOCK.
	r, e = scr(SYS_BIND, s, sockaddrIn(c, 0, [4]byte{127, 0, 0, 1}), 129)
	t.want("bind addrlen 129", r, e, syscall.EINVAL)
	r, e = scr(SYS_BIND, s, sockaddrIn(c, 0, [4]byte{127, 0, 0, 1}), u(-1))
	t.want("bind addrlen -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_BIND, s, kernHalf, 16)
	t.want("bind kernel addr", r, e, syscall.EFAULT)
	r, e = scr(SYS_BIND, u(-1), kernHalf, 16)
	t.want("bind bad fd first", r, e, syscall.EBADF)
	r, e = scr(SYS_BIND, 0, sockaddrIn(c, 0, [4]byte{127, 0, 0, 1}), 16)
	t.want("bind not a socket", r, e, syscall.ENOTSOCK)
	// inet_bind: addr_len < sizeof(struct sockaddr_in) → -EINVAL.
	r, e = scr(SYS_BIND, s, sockaddrIn(c, 0, [4]byte{127, 0, 0, 1}), 8)
	t.want("bind short sockaddr_in", r, e, syscall.EINVAL)
	r, e = scr(SYS_CONNECT, s, kernHalf, 16)
	t.want("connect kernel addr", r, e, syscall.EFAULT)
	// __sys_accept4: "if (flags & ~(SOCK_CLOEXEC | SOCK_NONBLOCK)) return -EINVAL" first.
	r, e = scr(SYS_ACCEPT4, u(-1), 0, 0, 0x1)
	t.want("accept4 bad flags", r, e, syscall.EINVAL)
	// getsockopt: "if (optlen < 0) return -EINVAL" (sock_getsockopt
	// len < 0); a bad optlen pointer → -EFAULT.
	r, e = scr(SYS_GETSOCKOPT, s, syscall.SOL_SOCKET, syscall.SO_TYPE, junk(randFor(10), 8), kernHalf)
	t.want("getsockopt kernel optlen", r, e, syscall.EFAULT)
	r, e = scr(SYS_GETSOCKOPT, s, syscall.SOL_SOCKET, syscall.SO_TYPE, junk(randFor(10), 8), c.u64s(uint64(0xffffffff)))
	t.want("getsockopt optlen negative", r, e, syscall.EINVAL)
	// __sys_setsockopt: "if (optlen < 0) return -EINVAL".
	r, e = scr(SYS_SETSOCKOPT, s, syscall.SOL_SOCKET, syscall.SO_REUSEADDR, c.u64s(1), u(-1))
	t.want("setsockopt optlen negative", r, e, syscall.EINVAL)
	r, e = scr(SYS_SETSOCKOPT, s, syscall.SOL_SOCKET, syscall.SO_REUSEADDR, kernHalf, 4)
	t.want("setsockopt kernel optval", r, e, syscall.EFAULT)
	// sendto with addrlen > sizeof(sockaddr_storage) → -EINVAL.
	r, e = scr(SYS_SENDTO, s, junk(randFor(10), 8), 8, 0, sockaddrIn(c, 9, [4]byte{127, 0, 0, 1}), 1000)
	t.want("sendto addrlen 1000", r, e, syscall.EINVAL)
	// __sys_shutdown → inet_shutdown: how outside SHUT_RD..SHUT_RDWR → -EINVAL.
	r, e = scr(SYS_SHUTDOWN, ts, 3)
	t.want("shutdown how 3", r, e, syscall.EINVAL)
	// recvmsg/sendmsg: a bad msghdr → -EFAULT; msg_iovlen > UIO_MAXIOV →
	// -EMSGSIZE (__copy_msghdr); msg_namelen < 0 → -EINVAL.
	r, e = scr(SYS_RECVMSG, s, kernHalf, syscall.MSG_DONTWAIT)
	t.want("recvmsg kernel msghdr", r, e, syscall.EFAULT)
	msg := c.alloc(56)
	put64(msg+16, uint64(c.u64s(uint64(junk(randFor(10), 8)), 8)))
	put64(msg+24, 1025)
	r, e = scr(SYS_SENDMSG, s, msg, syscall.MSG_DONTWAIT)
	t.want("sendmsg iovlen 1025", r, e, syscall.EMSGSIZE)
	put64(msg+24, 1)
	put64(msg, uint64(sockaddrIn(c, 9, [4]byte{127, 0, 0, 1})))
	put32(msg+8, 0xffffffff)
	r, e = scr(SYS_RECVMSG, s, msg, syscall.MSG_DONTWAIT)
	t.want("recvmsg namelen negative", r, e, syscall.EINVAL)

	// AF_UNIX control messages (net/core/scm.c __scm_send / scm_fp_copy):
	// cmsg_len < sizeof(cmsghdr) or past msg_controllen → -EINVAL; more
	// than SCM_MAX_FD (253) fds → -EINVAL; a bad fd → -EBADF.
	sv := c.alloc(8)
	must("socketpair")(scr(SYS_SOCKETPAIR, syscall.AF_UNIX, syscall.SOCK_DGRAM|nb, 0, sv))
	ua, ub := uintptr(get32(sv)), uintptr(get32(sv+4))
	sendCtl := func(ctl []byte) (uintptr, syscall.Errno) {
		m := c.alloc(56)
		put64(m+16, uint64(c.u64s(uint64(junk(randFor(11), 4)), 4)))
		put64(m+24, 1)
		cp := c.bytes(ctl)
		put64(m+32, uint64(cp))
		put64(m+40, uint64(len(ctl)))
		return scr(SYS_SENDMSG, ua, m, syscall.MSG_DONTWAIT)
	}
	cmsg := func(clen uint64, level, ty int32, fds ...int32) []byte {
		b := make([]byte, 16+4*len(fds))
		putLE64(b[0:], clen)
		putLE32(b[8:], uint32(level))
		putLE32(b[12:], uint32(ty))
		for i, fd := range fds {
			putLE32(b[16+4*i:], uint32(fd))
		}
		return b
	}
	r, e = sendCtl(cmsg(8, syscall.SOL_SOCKET, syscall.SCM_RIGHTS))
	t.want("sendmsg cmsg_len < header", r, e, syscall.EINVAL)
	r, e = sendCtl(cmsg(100, syscall.SOL_SOCKET, syscall.SCM_RIGHTS, 0))
	t.want("sendmsg cmsg_len past control", r, e, syscall.EINVAL)
	many := make([]int32, 254)
	r, e = sendCtl(cmsg(uint64(16+4*254), syscall.SOL_SOCKET, syscall.SCM_RIGHTS, many...))
	t.want("sendmsg 254 fds", r, e, syscall.EINVAL)
	r, e = sendCtl(cmsg(20, syscall.SOL_SOCKET, syscall.SCM_RIGHTS, 9999))
	t.want("sendmsg SCM_RIGHTS bad fd", r, e, syscall.EBADF)
	r, e = sendCtl(cmsg(20, syscall.SOL_SOCKET, syscall.SCM_RIGHTS, -1))
	t.want("sendmsg SCM_RIGHTS fd -1", r, e, syscall.EBADF)
	r, e = sendCtl(cmsg(16, syscall.SOL_SOCKET, 77))
	t.want("sendmsg unknown SOL_SOCKET type", r, e, syscall.EINVAL)
	// socketpair: AF_INET → -EOPNOTSUPP; a bad sv pointer → -EFAULT
	// (and no fd stays open).
	r, e = scr(SYS_SOCKETPAIR, syscall.AF_INET, syscall.SOCK_STREAM, 0, sv)
	t.want("socketpair AF_INET", r, e, syscall.EOPNOTSUPP)
	before := countFds()
	r, e = scr(SYS_SOCKETPAIR, syscall.AF_UNIX, syscall.SOCK_STREAM, 0, kernHalf)
	t.want("socketpair kernel sv", r, e, syscall.EFAULT)
	t.check("socketpair kernel sv leaks no fd", countFds() == before,
		fmt.Sprintf("fds %d → %d", before, countFds()))
	// AF_UNIX bind: sun_path longer than the address → -EINVAL.
	r, e = scr(SYS_BIND, ua, c.cstr("\x01\x00"), 1)
	t.want("bind unix addrlen 1", r, e, syscall.EINVAL)
	for _, f := range []uintptr{s, ts, ua, ub} {
		closeFd(f)
	}
}

func putLE64(b []byte, v uint64) {
	for i := 0; i < 8; i++ {
		b[i] = byte(v >> (8 * i))
	}
}

func putLE32(b []byte, v uint32) {
	for i := 0; i < 4; i++ {
		b[i] = byte(v >> (8 * i))
	}
}

func casesSignal(t *tcase) {
	c := t.c
	oact := junk(randFor(12), 64)
	// kernel/signal.c rt_sigaction: "if (sigsetsize != sizeof(sigset_t))
	// return -EINVAL"; do_sigaction: "!valid_signal(sig) || sig < 1 ||
	// (act && sig_kernel_only(sig)) → -EINVAL".
	r, e := scr(SYS_RT_SIGACTION, uintptr(syscall.SIGUSR1), 0, oact, 4)
	t.want("rt_sigaction sigsetsize 4", r, e, syscall.EINVAL)
	for _, sig := range []int64{0, 65, -1, 1 << 32} {
		r, e = scr(SYS_RT_SIGACTION, u(sig), 0, oact, 8)
		t.want(fmt.Sprintf("rt_sigaction sig %d", sig), r, e, syscall.EINVAL)
	}
	r, e = scr(SYS_RT_SIGACTION, uintptr(syscall.SIGKILL), c.alloc(32), 0, 8)
	t.want("rt_sigaction SIGKILL", r, e, syscall.EINVAL)
	r, e = scr(SYS_RT_SIGACTION, uintptr(syscall.SIGSTOP), c.alloc(32), 0, 8)
	t.want("rt_sigaction SIGSTOP", r, e, syscall.EINVAL)
	r, e = scr(SYS_RT_SIGACTION, uintptr(syscall.SIGKILL), 0, oact, 8)
	t.want("rt_sigaction SIGKILL query", r, e, 0)
	r, e = scr(SYS_RT_SIGACTION, uintptr(syscall.SIGUSR1), kernHalf, 0, 8)
	t.want("rt_sigaction kernel act", r, e, syscall.EFAULT)
	r, e = scr(SYS_RT_SIGACTION, uintptr(syscall.SIGUSR1), 0, kernHalf, 8)
	t.want("rt_sigaction kernel oact", r, e, syscall.EFAULT)
	// rt_sigprocmask: sigsetsize → -EINVAL; bad how with a set → -EINVAL;
	// bad how without a set → 0.
	r, e = scr(SYS_RT_SIGPROCMASK, 99, c.u64s(0), 0, 8)
	t.want("rt_sigprocmask bad how", r, e, syscall.EINVAL)
	r, e = scr(SYS_RT_SIGPROCMASK, 99, 0, oact, 8)
	t.want("rt_sigprocmask bad how no set", r, e, 0)
	r, e = scr(SYS_RT_SIGPROCMASK, 0, 0, oact, 16)
	t.want("rt_sigprocmask sigsetsize 16", r, e, syscall.EINVAL)
	r, e = scr(SYS_RT_SIGPROCMASK, 0, kernHalf, 0, 8)
	t.want("rt_sigprocmask kernel set", r, e, syscall.EFAULT)
	// sigaltstack (do_sigaltstack): bad pointer → -EFAULT; bad flags →
	// -EINVAL; ss_size < MINSIGSTKSZ → -ENOMEM.
	r, e = scr(SYS_SIGALTSTACK, kernHalf, 0)
	t.want("sigaltstack kernel ss", r, e, syscall.EFAULT)
	r, e = scr(SYS_SIGALTSTACK, c.u64s(uint64(rwBase), 0x77, 1<<20), 0)
	t.want("sigaltstack bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_SIGALTSTACK, c.u64s(uint64(rwBase), 0, 100), 0)
	t.want("sigaltstack tiny", r, e, syscall.ENOMEM)
	r, e = scr(SYS_SIGALTSTACK, 0, kernHalf)
	t.want("sigaltstack kernel oss", r, e, syscall.EFAULT)
	// kill: an unknown pid → -ESRCH; a bad signal → -EINVAL.
	pid := uintptr(os.Getpid())
	r, e = scr(SYS_KILL, 99999, 0)
	t.want("kill unknown pid", r, e, syscall.ESRCH)
	r, e = scr(SYS_KILL, pid, 65)
	t.want("kill sig 65", r, e, syscall.EINVAL)
	r, e = scr(SYS_KILL, pid, u(-1))
	t.want("kill sig -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_KILL, pid, 0)
	t.want("kill sig 0", r, e, 0)
	// tgkill: "if (pid <= 0 || tgid <= 0) return -EINVAL"; unknown tid → -ESRCH.
	tid, _ := scr(SYS_GETTID)
	r, e = scr(SYS_TGKILL, 0, tid, 0)
	t.want("tgkill tgid 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_TGKILL, pid, u(-1), 0)
	t.want("tgkill tid -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_TGKILL, pid, 99999, 0)
	t.want("tgkill unknown tid", r, e, syscall.ESRCH)
	r, e = scr(SYS_TGKILL, pid, tid, 65)
	t.want("tgkill sig 65", r, e, syscall.EINVAL)
	r, e = scr(SYS_TKILL, u(-5), 0)
	t.want("tkill tid -5", r, e, syscall.EINVAL)
	// rt_sigpending: "if (sigsetsize > sizeof(sigset_t)) return -EINVAL".
	r, e = scr(SYS_RT_SIGPENDING, oact, 9)
	t.want("rt_sigpending size 9", r, e, syscall.EINVAL)
	r, e = scr(SYS_RT_SIGPENDING, kernHalf, 8)
	t.want("rt_sigpending kernel set", r, e, syscall.EFAULT)
	// rt_sigsuspend: sigsetsize → -EINVAL; bad mask → -EFAULT.
	r, e = scr(SYS_RT_SIGSUSPEND, c.u64s(0), 4)
	t.want("rt_sigsuspend sigsetsize 4", r, e, syscall.EINVAL)
	r, e = scr(SYS_RT_SIGSUSPEND, kernHalf, 8)
	t.want("rt_sigsuspend kernel mask", r, e, syscall.EFAULT)
}

func casesTime(t *tcase) {
	c := t.c
	// kernel/time/hrtimer.c nanosleep: get_timespec64 → -EFAULT;
	// "!timespec64_valid → -EINVAL" (negative sec, nsec outside [0, 1e9)).
	r, e := scr(SYS_NANOSLEEP, 0, 0)
	t.want("nanosleep NULL", r, e, syscall.EFAULT)
	r, e = scr(SYS_NANOSLEEP, kernHalf, 0)
	t.want("nanosleep kernel req", r, e, syscall.EFAULT)
	r, e = scr(SYS_NANOSLEEP, c.timespec(0, -1), 0)
	t.want("nanosleep nsec -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_NANOSLEEP, c.timespec(0, 1e9), 0)
	t.want("nanosleep nsec 1e9", r, e, syscall.EINVAL)
	r, e = scr(SYS_NANOSLEEP, c.timespec(-1, 0), 0)
	t.want("nanosleep sec -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_NANOSLEEP, c.timespec(0, 1), kernHalf)
	t.want("nanosleep 1ns bad rem", r, e, 0)
	// kernel/time/posix-timers.c clock_nanosleep: unknown clock → -EINVAL;
	// thread CPU clock → -EINVAL (posix_cpu_nsleep).
	r, e = scr(SYS_CLOCK_NANOSLEEP, 100, 0, c.timespec(0, 1), 0)
	t.want("clock_nanosleep bad clock", r, e, syscall.EINVAL)
	r, e = scr(SYS_CLOCK_NANOSLEEP, 3, 0, c.timespec(0, 1), 0)
	t.want("clock_nanosleep thread cputime", r, e, syscall.EINVAL)
	r, e = scr(SYS_CLOCK_NANOSLEEP, 1, 0, c.timespec(0, 1e9), 0)
	t.want("clock_nanosleep bad nsec", r, e, syscall.EINVAL)
	r, e = scr(SYS_CLOCK_NANOSLEEP, 1, 1, c.timespec(0, 1), 0)
	t.want("clock_nanosleep abstime in the past", r, e, 0)
	// clock_gettime / clock_getres: unknown clock → -EINVAL; bad pointer → -EFAULT.
	r, e = scr(SYS_CLOCK_GETTIME, 100, junk(randFor(13), 16))
	t.want("clock_gettime bad clock", r, e, syscall.EINVAL)
	r, e = scr(SYS_CLOCK_GETTIME, 1, kernHalf)
	t.want("clock_gettime kernel tp", r, e, syscall.EFAULT)
	r, e = scr(SYS_CLOCK_GETTIME, 1, roPage)
	t.want("clock_gettime read-only tp", r, e, syscall.EFAULT)
	r, e = scr(SYS_CLOCK_GETRES, 100, 0)
	t.want("clock_getres bad clock", r, e, syscall.EINVAL)
	r, e = scr(SYS_CLOCK_GETRES, 1, 0)
	t.want("clock_getres NULL", r, e, 0)
	// A per-thread CPU clock of a thread that does not exist: -EINVAL.
	r, e = scr(SYS_CLOCK_GETTIME, u((^int64(99999))<<3|6), junk(randFor(13), 16))
	t.want("clock_gettime cpuclock of unknown tid", r, e, syscall.EINVAL)
	// timer_create: unknown clock → -EINVAL; bad sigev_notify / signo →
	// -EINVAL (good_sigevent); bad timerid pointer → -EFAULT.
	idp := junk(randFor(13), 8)
	r, e = scr(SYS_TIMER_CREATE, 100, 0, idp)
	t.want("timer_create bad clock", r, e, syscall.EINVAL)
	sev := func(notify, signo int32) uintptr {
		p := c.alloc(64)
		put32(p+8, uint32(signo))
		put32(p+12, uint32(notify))
		return p
	}
	r, e = scr(SYS_TIMER_CREATE, 1, sev(77, 10), idp)
	t.want("timer_create bad notify", r, e, syscall.EINVAL)
	r, e = scr(SYS_TIMER_CREATE, 1, sev(0, 0), idp)
	t.want("timer_create signo 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_TIMER_CREATE, 1, sev(0, 65), idp)
	t.want("timer_create signo 65", r, e, syscall.EINVAL)
	r, e = scr(SYS_TIMER_CREATE, 1, kernHalf, idp)
	t.want("timer_create kernel sevp", r, e, syscall.EFAULT)
	r, e = scr(SYS_TIMER_CREATE, 1, sev(1, 0), kernHalf) // SIGEV_NONE
	t.want("timer_create kernel timerid", r, e, syscall.EFAULT)
	// timer_settime: "if (!new_setting) return -EINVAL"; invalid
	// itimerspec → -EINVAL; unknown timer → -EINVAL.
	r, e = scr(SYS_TIMER_SETTIME, 12345, 0, 0, 0)
	t.want("timer_settime NULL new", r, e, syscall.EINVAL)
	r, e = scr(SYS_TIMER_SETTIME, 12345, 0, c.timespec(0, 0, 0, 1), 0)
	t.want("timer_settime unknown timer", r, e, syscall.EINVAL)
	r, e = scr(SYS_TIMER_SETTIME, 12345, 0, kernHalf, 0)
	t.want("timer_settime kernel new", r, e, syscall.EFAULT)
	r, e = scr(SYS_TIMER_DELETE, 12345)
	t.want("timer_delete unknown", r, e, syscall.EINVAL)
	r, e = scr(SYS_TIMER_GETOVERRUN, u(-1))
	t.want("timer_getoverrun -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_TIMER_GETTIME, 12345, junk(randFor(13), 32))
	t.want("timer_gettime unknown", r, e, syscall.EINVAL)
	// itimers (kernel/time/itimer.c): unknown which → -EINVAL; tv_usec
	// outside [0, 1e6) → -EINVAL.
	r, e = scr(SYS_GETITIMER, 5, junk(randFor(13), 32))
	t.want("getitimer which 5", r, e, syscall.EINVAL)
	r, e = scr(SYS_SETITIMER, 5, c.alloc(32), 0)
	t.want("setitimer which 5", r, e, syscall.EINVAL)
	r, e = scr(SYS_SETITIMER, 0, c.u64s(0, 0, 0, 1e6), 0)
	t.want("setitimer bad usec", r, e, syscall.EINVAL)
	r, e = scr(SYS_SETITIMER, 0, kernHalf, 0)
	t.want("setitimer kernel value", r, e, syscall.EFAULT)
	// getrusage: unknown who → -EINVAL.
	r, e = scr(SYS_GETRUSAGE, 5, junk(randFor(13), 256))
	t.want("getrusage who 5", r, e, syscall.EINVAL)
	r, e = scr(SYS_GETRUSAGE, 0, kernHalf)
	t.want("getrusage kernel buf", r, e, syscall.EFAULT)
	r, e = scr(SYS_GETTIMEOFDAY, kernHalf, 0)
	t.want("gettimeofday kernel tv", r, e, syscall.EFAULT)
}

func casesProc(t *tcase) {
	c := t.c
	// kernel/sys.c prlimit64: copy the new limit first (-EFAULT), find the
	// task (-ESRCH), then do_prlimit: resource → -EINVAL, cur > max →
	// -EINVAL, NOFILE max > nr_open (1048576) → -EPERM.
	r, e := scr(SYS_PRLIMIT64, 0, 99, kernHalf, 0)
	t.want("prlimit64 kernel new first", r, e, syscall.EFAULT)
	r, e = scr(SYS_PRLIMIT64, 99999, 0, 0, 0)
	t.want("prlimit64 unknown pid", r, e, syscall.ESRCH)
	r, e = scr(SYS_PRLIMIT64, 0, 16, 0, junk(randFor(14), 16))
	t.want("prlimit64 resource 16", r, e, syscall.EINVAL)
	r, e = scr(SYS_PRLIMIT64, 0, syscall.RLIMIT_NOFILE, c.u64s(10, 5), 0)
	t.want("prlimit64 cur > max", r, e, syscall.EINVAL)
	r, e = scr(SYS_PRLIMIT64, 0, syscall.RLIMIT_NOFILE, c.u64s(1024, 1<<21), 0)
	t.want("prlimit64 NOFILE above nr_open", r, e, syscall.EPERM)
	r, e = scr(SYS_PRLIMIT64, 0, syscall.RLIMIT_NOFILE, 0, kernHalf)
	t.want("prlimit64 kernel old", r, e, syscall.EFAULT)
	// getrandom (drivers/char/random.c): unknown flags or RANDOM|INSECURE →
	// -EINVAL; a bad buffer → -EFAULT; len 0 → 0.
	r, e = scr(SYS_GETRANDOM, junk(randFor(14), 16), 16, 0x80)
	t.want("getrandom bad flags", r, e, syscall.EINVAL)
	r, e = scr(SYS_GETRANDOM, junk(randFor(14), 16), 16, 0x2|0x4)
	t.want("getrandom random|insecure", r, e, syscall.EINVAL)
	r, e = scr(SYS_GETRANDOM, kernHalf, 16, 0)
	t.want("getrandom kernel buf", r, e, syscall.EFAULT)
	r, e = scr(SYS_GETRANDOM, 0, 0, 0)
	t.want("getrandom len 0", r, e, 0)
	r, e = scr(SYS_GETRANDOM, rwEnd-10, 100, 0)
	t.wantRet("getrandom up to the guard page", r, e, 10)
	// sched_getaffinity: "if ((len * BITS_PER_BYTE) < nr_cpu_ids) → -EINVAL;
	// if (len & (sizeof(unsigned long)-1)) → -EINVAL"; unknown pid → -ESRCH.
	r, e = scr(SYS_SCHED_GETAFFINITY, 0, 4, junk(randFor(14), 8))
	t.want("sched_getaffinity len 4", r, e, syscall.EINVAL)
	r, e = scr(SYS_SCHED_GETAFFINITY, 0, 12, junk(randFor(14), 16))
	t.want("sched_getaffinity len 12", r, e, syscall.EINVAL)
	r, e = scr(SYS_SCHED_GETAFFINITY, 99999, 8, junk(randFor(14), 8))
	t.want("sched_getaffinity unknown pid", r, e, syscall.ESRCH)
	r, e = scr(SYS_SCHED_GETAFFINITY, 0, 8, kernHalf)
	t.want("sched_getaffinity kernel mask", r, e, syscall.EFAULT)
	r, e = scr(SYS_SCHED_SETAFFINITY, 0, 8, kernHalf)
	t.want("sched_setaffinity kernel mask", r, e, syscall.EFAULT)
	r, e = scr(SYS_UNAME, kernHalf)
	t.want("uname kernel buf", r, e, syscall.EFAULT)
	r, e = scr(SYS_UNAME, roPage)
	t.want("uname read-only buf", r, e, syscall.EFAULT)
	r, e = scr(SYS_SYSINFO, kernHalf)
	t.want("sysinfo kernel buf", r, e, syscall.EFAULT)
	// prctl: unknown option → -EINVAL; PR_SET_NAME with a bad pointer → -EFAULT.
	r, e = scr(SYS_PRCTL, 0x7fff_0000)
	t.want("prctl bad option", r, e, syscall.EINVAL)
	r, e = scr(SYS_PRCTL, syscall.PR_SET_NAME, kernHalf)
	t.want("prctl PR_SET_NAME kernel", r, e, syscall.EFAULT)
	r, e = scr(SYS_PRCTL, syscall.PR_GET_NAME, roPage)
	t.want("prctl PR_GET_NAME read-only", r, e, syscall.EFAULT)
	// capget: a bad header → -EFAULT.
	r, e = scr(SYS_CAPGET, kernHalf, 0)
	t.want("capget kernel header", r, e, syscall.EFAULT)
	// getgroups: "if (gidsetsize < 0) return -EINVAL"; setgroups:
	// "if ((unsigned)gidsetsize > NGROUPS_MAX) return -EINVAL".
	r, e = scr(SYS_GETGROUPS, u(-1), 0)
	t.want("getgroups -1", r, e, syscall.EINVAL)
	r, e = scr(SYS_SETGROUPS, 65537, junk(randFor(14), 8))
	t.want("setgroups 65537", r, e, syscall.EINVAL)
	r, e = scr(SYS_SETGROUPS, 4, kernHalf)
	t.want("setgroups kernel list", r, e, syscall.EFAULT)
	// futex (kernel/futex/syscalls.c): the timeout is read and validated
	// before anything else (-EFAULT / -EINVAL); an unaligned uaddr →
	// -EINVAL (get_futex_key); an unreadable one → -EFAULT; a mismatched
	// value → -EAGAIN; bitset 0 → -EINVAL; unknown op → -ENOSYS;
	// FUTEX_CLOCK_REALTIME on FUTEX_WAKE → -ENOSYS; a kernel-half uaddr →
	// -EFAULT even for FUTEX_WAKE (access_ok in get_futex_key).
	fw := junk(randFor(14), 8) &^ 7
	put32(fw, 5)
	r, e = scr(SYS_FUTEX, fw+1, 0, 5, kernHalf)
	t.want("futex wait bad timeout before alignment", r, e, syscall.EFAULT)
	r, e = scr(SYS_FUTEX, fw, 0, 5, c.timespec(0, -1))
	t.want("futex wait bad timeout", r, e, syscall.EINVAL)
	r, e = scr(SYS_FUTEX, fw+1, 0, 5, c.timespec(0, 1000))
	t.want("futex wait unaligned", r, e, syscall.EINVAL)
	r, e = scr(SYS_FUTEX, holePage, 0, 5, c.timespec(0, 1000))
	t.want("futex wait hole", r, e, syscall.EFAULT)
	r, e = scr(SYS_FUTEX, kernHalf, 0, 5, c.timespec(0, 1000))
	t.want("futex wait kernel", r, e, syscall.EFAULT)
	r, e = scr(SYS_FUTEX, fw, 0, 6, c.timespec(0, 1000))
	t.want("futex wait mismatch", r, e, syscall.EAGAIN)
	r, e = scr(SYS_FUTEX, fw, 0, 5, c.timespec(0, 1000))
	t.want("futex wait times out", r, e, syscall.ETIMEDOUT)
	r, e = scr(SYS_FUTEX, fw, 9, 5, c.timespec(0, 1000), 0, 0) // WAIT_BITSET, bitset 0
	t.want("futex wait_bitset bitset 0", r, e, syscall.EINVAL)
	r, e = scr(SYS_FUTEX, fw, 99, 0, 0)
	t.want("futex bad op", r, e, syscall.ENOSYS)
	r, e = scr(SYS_FUTEX, fw, 1|256, 1, 0) // FUTEX_WAKE|FUTEX_CLOCK_REALTIME
	t.want("futex wake clock_realtime", r, e, syscall.ENOSYS)
	r, e = scr(SYS_FUTEX, kernHalf, 1, 1, 0)
	t.want("futex wake kernel", r, e, syscall.EFAULT)
	r, e = scr(SYS_FUTEX, fw, 1|128, 1, 0)
	t.want("futex wake private", r, e, 0)
}
