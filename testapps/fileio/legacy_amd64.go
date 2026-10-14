package main

import (
	"os"
	"syscall"
	"time"
	"unsafe"
)

// x86_64-only legacy syscalls that Go's syscall package issues directly.
func legacyChecks(q string) {
	check("utime (x86_64 SYS_UTIME)", func() error {
		if err := syscall.Utime(q, &syscall.Utimbuf{Actime: 3000000, Modtime: 4000000}); err != nil {
			return err
		}
		fi, err := os.Stat(q)
		if err != nil {
			return err
		}
		st := fi.Sys().(*syscall.Stat_t)
		if fi.ModTime().Unix() != 4000000 || fi.ModTime().Nanosecond() != 0 || st.Atim.Sec != 3000000 {
			return errorf("mtime %v atime %d", fi.ModTime(), st.Atim.Sec)
		}
		before := time.Now().Unix()
		if err := syscall.Utime(q, nil); err != nil {
			return err
		}
		if fi, err = os.Stat(q); err != nil {
			return err
		}
		if fi.ModTime().Unix() < before {
			return errorf("utime(NULL): mtime %v, before %d", fi.ModTime(), before)
		}
		return nil
	}())
	check("time (x86_64 SYS_TIME)", func() error {
		var t int64
		r, _, e := syscall.RawSyscall(syscall.SYS_TIME, uintptr(unsafe.Pointer(&t)), 0, 0)
		now := time.Now().Unix()
		if e != 0 || int64(r) != t || t < now-2 || t > now+2 {
			return errorf("time = %d (stored %d), errno %v, now %d", int64(r), t, e, now)
		}
		return nil
	}())
}
