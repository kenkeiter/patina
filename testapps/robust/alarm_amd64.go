package main

import (
	"syscall"
	"time"
)

// checkAlarm (amd64 only: arm64 has no alarm syscall): alarm(2) arms
// ITIMER_REAL, alarm(0) cancels it and returns the seconds left, rounded
// as Linux (~1.4 s left → 1).
func checkAlarm() {
	r, _, e := syscall.RawSyscall(syscall.SYS_ALARM, 2, 0, 0)
	if e != 0 || r != 0 {
		fail("alarm(2) = %d, %v", r, e)
	}
	time.Sleep(600 * time.Millisecond)
	r, _, e = syscall.RawSyscall(syscall.SYS_ALARM, 0, 0, 0)
	if e != 0 || r != 1 {
		fail("alarm(0) after 0.6 s = %d, %v; want 1", r, e)
	}
	ok("alarm(2) then alarm(0) after 0.6s returns 1")
}
