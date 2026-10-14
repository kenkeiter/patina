package main

import (
	"encoding/binary"
	"fmt"
	"math/rand"
	"os"
	"syscall"
	"unsafe"
)

const (
	pageSize = 4096
	// The data arena: buffers the kernel reads from and writes to.
	junkSize   = 2 << 20 // shared scratch: any syscall may write here
	consSize   = 64 << 10
	maxWorkers = 32
	// A PROT_NONE page between the junk area and the cons areas: a kernel
	// write through a junk pointer with a huge length stops there (EFAULT
	// after a partial copy) instead of overwriting constructed inputs.
	consBase = junkSize + pageSize
	rwSize   = consBase + maxWorkers*consSize
	// The mm arena: the only range the fuzzer maps, unmaps, protects,
	// advises and remaps.
	mmArenaSize = 64 << 20

	// Addresses that are never valid user memory.
	kernHalf  = 0xffff_8000_0000_0000
	userEnd   = 0x0000_8000_0000_0000 // first non-user address (48-bit VA)
	allOnes   = ^uintptr(0)
	lowUnmapd = 0x1000 // below mmap_min_addr
)

// Arena addresses (set up once by setupArena).
var (
	arenaBase uintptr // guard page below the RW region
	rwBase    uintptr // RW region: junk area, then per-worker cons areas
	rwEnd     uintptr // first byte past the RW region: a PROT_NONE guard
	roPage    uintptr // PROT_READ page (zero-filled)
	holePage  uintptr // unmapped page
	mmBase    uintptr // mm arena (PROT_NONE, MAP_NORESERVE at start)
	guardPage uintptr // = rwEnd
)

func mmapRaw(addr, length, prot, flags uintptr) (uintptr, syscall.Errno) {
	return scr(SYS_MMAP, addr, length, prot, flags, allOnes, 0)
}

func setupArena() {
	total := uintptr(pageSize + rwSize + 4*pageSize)
	base, e := mmapRaw(0, total, syscall.PROT_NONE,
		syscall.MAP_PRIVATE|syscall.MAP_ANONYMOUS|syscall.MAP_NORESERVE)
	if e != 0 {
		fmt.Printf("FAIL sysfuzz: arena mmap: %v\n", e)
		os.Exit(1)
	}
	arenaBase = base
	rwBase = base + pageSize
	rwEnd = rwBase + rwSize
	guardPage = rwEnd
	roPage = rwEnd + pageSize
	holePage = roPage + pageSize
	// [guard][junk][guard][cons...][guard][RO][hole][guard]
	if _, e := scr(SYS_MPROTECT, rwBase, rwSize, syscall.PROT_READ|syscall.PROT_WRITE); e != 0 {
		fmt.Printf("FAIL sysfuzz: arena mprotect rw: %v\n", e)
		os.Exit(1)
	}
	if _, e := scr(SYS_MPROTECT, rwBase+junkSize, pageSize, syscall.PROT_NONE); e != 0 {
		fmt.Printf("FAIL sysfuzz: arena mprotect cons guard: %v\n", e)
		os.Exit(1)
	}
	if _, e := scr(SYS_MPROTECT, roPage, pageSize, syscall.PROT_READ); e != 0 {
		fmt.Printf("FAIL sysfuzz: arena mprotect ro: %v\n", e)
		os.Exit(1)
	}
	if _, e := scr(SYS_MUNMAP, holePage, pageSize); e != 0 {
		fmt.Printf("FAIL sysfuzz: arena munmap hole: %v\n", e)
		os.Exit(1)
	}
	mm, e := mmapRaw(0, mmArenaSize, syscall.PROT_NONE,
		syscall.MAP_PRIVATE|syscall.MAP_ANONYMOUS|syscall.MAP_NORESERVE)
	if e != 0 {
		fmt.Printf("FAIL sysfuzz: mm arena mmap: %v\n", e)
		os.Exit(1)
	}
	mmBase = mm
	fmt.Printf("sysfuzz: arena rw %#x-%#x (cons at +%#x) ro %#x hole %#x mm %#x-%#x\n",
		rwBase, rwEnd, consBase, roPage, holePage, mmBase, mmBase+mmArenaSize)
}

// mem views arena memory as a byte slice.
func mem(p uintptr, n int) []byte {
	return unsafe.Slice((*byte)(unsafe.Pointer(p)), n)
}

func put64(p uintptr, v uint64) { binary.LittleEndian.PutUint64(mem(p, 8), v) }
func put32(p uintptr, v uint32) { binary.LittleEndian.PutUint32(mem(p, 4), v) }
func put16(p uintptr, v uint16) { binary.LittleEndian.PutUint16(mem(p, 2), v) }
func get64(p uintptr) uint64    { return binary.LittleEndian.Uint64(mem(p, 8)) }
func get32(p uintptr) uint32    { return binary.LittleEndian.Uint32(mem(p, 4)) }

// cons is a per-worker bump allocator for constructed syscall inputs
// (timespecs, iovecs, msghdrs, paths...). Only its owner writes it, so a
// "1 ns" timeout cannot turn into "forever" behind its back; syscalls
// that write output are pointed at the shared junk area instead.
type cons struct {
	base, off uintptr
}

func newCons(worker int) *cons {
	return &cons{base: rwBase + consBase + uintptr(worker)*consSize}
}

func (c *cons) reset() { c.off = 0 }

// alloc returns n zeroed bytes, 16-aligned.
func (c *cons) alloc(n int) uintptr {
	n = (n + 15) &^ 15
	if c.off+uintptr(n) > consSize {
		c.off = 0
	}
	p := c.base + c.off
	c.off += uintptr(n)
	clear(mem(p, n))
	return p
}

func (c *cons) bytes(b []byte) uintptr {
	p := c.alloc(len(b))
	copy(mem(p, len(b)), b)
	return p
}

// cstr places a NUL-terminated string.
func (c *cons) cstr(s string) uintptr {
	p := c.alloc(len(s) + 1)
	copy(mem(p, len(s)), s)
	return p
}

func (c *cons) u64s(v ...uint64) uintptr {
	p := c.alloc(8 * len(v))
	for i, x := range v {
		put64(p+uintptr(8*i), x)
	}
	return p
}

// timespec places (sec, nsec) pairs: one struct timespec, or an array
// of them (utimensat, itimerspec).
func (c *cons) timespec(v ...int64) uintptr {
	u := make([]uint64, len(v))
	for i, x := range v {
		u[i] = uint64(x)
	}
	return c.u64s(u...)
}

// randFor is a fixed-seed source for deterministic cases.
func randFor(n int64) *rand.Rand { return rand.New(rand.NewSource(n)) }

// junk returns a pointer to n bytes somewhere in the shared junk area.
func junk(r *rand.Rand, n int) uintptr {
	if n > junkSize {
		n = junkSize
	}
	if n < 0 {
		n = 0
	}
	off := r.Intn(junkSize - n + 1)
	return rwBase + uintptr(off)
}

// badPtrs are never valid user memory for any access.
func badPtrs() []uintptr {
	return []uintptr{
		0,
		1,
		lowUnmapd,
		kernHalf,
		kernHalf + 0x1000,
		0xffff_ffff_ffff_f000,
		allOnes,
		allOnes - 7,
		userEnd,
		userEnd - 8, // straddles the end of user space (unmapped anyway)
		holePage,
		guardPage,
		arenaBase,
		0x0000_7fff_ffff_f000,
		0xdead_beef_0000,
	}
}

// edgeLens are interesting lengths/counts.
var edgeLens = []uint64{
	0, 1, 2, 7, 8, 15, 16, 4095, 4096, 4097, 65535, 65536, 65537,
	1 << 20, 0x7fff_f000, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 1 << 32,
	1 << 47, 1 << 48, 1 << 62, 1 << 63, ^uint64(0), ^uint64(0) - 4095,
	^uint64(0) - 0xfff - 0x1000, 1<<63 - 1,
}

// edgeInts are interesting int/fd/flag values (as 64-bit registers).
var edgeInts = []int64{
	0, 1, 2, 3, -1, -2, -100, 63, 64, 65, 127, 128, 255, 256, 1023, 1024,
	1 << 16, 1<<20 - 1, 1 << 20, 1<<31 - 1, -1 << 31, 1 << 32, 1<<32 + 3,
	-1 << 63, 1<<63 - 1,
}
