//! Adjustable `CLOCK_REALTIME` offset: wall time = `timer::now_ns()` +
//! [`epoch_ns`]. Starts as `patina.epoch` (set by the loader through
//! `Process.epoch_ns`, read lazily on first use) and is stepped by the
//! control channel's `time` op ([`set_epoch_ns`], design §10.2).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static EPOCH_NS: AtomicU64 = AtomicU64::new(0);
static SET: AtomicBool = AtomicBool::new(false);

/// Wall-clock time at boot (ns since the Unix epoch).
pub fn epoch_ns() -> u64 {
    if SET.load(Ordering::Acquire) {
        return EPOCH_NS.load(Ordering::Relaxed);
    }
    crate::task::try_process().map_or(0, |p| p.epoch_ns)
}

/// Step the wall clock so that "now" is `now_epoch_ns` (ns since the Unix
/// epoch; values before 1970 clamp to 0).
pub fn set_now(now_epoch_ns: i64) {
    let now = crate::arch::timer::now_ns();
    let e = (now_epoch_ns.max(0) as u64).saturating_sub(now);
    set_epoch_ns(e);
}

/// Set the boot-time offset directly.
pub fn set_epoch_ns(e: u64) {
    EPOCH_NS.store(e, Ordering::Relaxed);
    SET.store(true, Ordering::Release);
}

/// Current wall-clock time in ns since the Unix epoch.
pub fn now_ns() -> u64 {
    crate::arch::timer::now_ns() + epoch_ns()
}
