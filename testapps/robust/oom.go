package main

import (
	"fmt"
	"syscall"
)

// runOOMFault touches a MAP_NORESERVE mapping larger than RAM (exempt
// from the commit limit) page by page: the page fault that finds no free
// frame kills the program (D78 backstop: "[patina] out of memory: ...",
// SIGKILL, exit 137).
func runOOMFault() {
	const size = 1 << 30
	b, err := syscall.Mmap(-1, 0, size, syscall.PROT_READ|syscall.PROT_WRITE,
		syscall.MAP_PRIVATE|syscall.MAP_ANON|syscall.MAP_NORESERVE)
	if err != nil {
		fail("mmap 1 GiB MAP_NORESERVE: %v", err)
	}
	ok("oomfault: touching 1 GiB MAP_NORESERVE")
	for off := 0; off < size; off += 4096 {
		b[off] = 1
	}
	fail("touched 1 GiB without running out of memory")
}

// runOOM allocates (and touches) 64 MiB chunks, keeping every one, until
// the runtime fails. With commit accounting (D78) Go's own
// "fatal error: runtime: out of memory" ends the program (exit 2).
func runOOM() {
	total := procKB("/proc/meminfo", "MemTotal:") * 4 / 1024
	ok("oom: allocating 64 MiB chunks on a %d MiB VM", total)
	var keep [][]byte
	for i := 1; ; i++ {
		b := make([]byte, 64<<20)
		for off := 0; off < len(b); off += 4096 {
			b[off] = byte(i)
		}
		keep = append(keep, b)
		fmt.Printf("robust: oom: %d MiB allocated and touched\n", i*64)
		if i > 1024 {
			fail("allocated 64 GiB without running out of memory")
		}
	}
}
