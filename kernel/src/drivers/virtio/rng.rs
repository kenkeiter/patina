//! virtio-rng (entropy device, virtio 1.2 §5.4): one request queue of
//! device-writable buffers. Polled: requests are rare (boot seeding and
//! periodic reseeds, see `random.rs`), so no interrupt is attached.

use super::queue::{Buf, DmaBuf, VirtQueue};
use super::{DEVICE_RNG, Device, driver_ok, negotiate};
use crate::sync::SpinLock;

/// Largest single request.
const REQ_MAX: usize = 256;

struct Rng {
    dev: Device,
    q: VirtQueue,
    buf: DmaBuf,
    /// A request is with the device (a timed-out one stays outstanding
    /// and is collected by the next read).
    outstanding: bool,
}

static RNG: SpinLock<Option<Rng>> = SpinLock::new(None);

/// Claim and start the first virtio-rng device, if any.
pub fn init() {
    let Some(dev) = super::take(DEVICE_RNG) else {
        return;
    };
    let t = &*dev.transport;
    if let Err(e) = negotiate(t, 0) {
        crate::kprintln!("[patina] virtio-rng: {}", e);
        return;
    }
    let (Some(mut q), Some(buf)) = (VirtQueue::new(t, 0, 4), DmaBuf::new(REQ_MAX)) else {
        crate::kprintln!("[patina] virtio-rng: no request queue / memory");
        return;
    };
    q.set_no_interrupt(true);
    driver_ok(t);
    *RNG.lock() = Some(Rng {
        dev,
        q,
        buf,
        outstanding: false,
    });
}

/// Whether a virtio-rng device is driven.
pub fn present() -> bool {
    RNG.lock().is_some()
}

/// Fill up to `out.len()` (at most 256) bytes from the device, waiting at
/// most `timeout_ns`. `sleep`: wait by sleeping 1 ms at a time (kernel
/// thread context) instead of spinning (boot). Returns the number of bytes
/// obtained (0 on timeout or without a device).
pub fn read(out: &mut [u8], timeout_ns: u64, sleep: bool) -> usize {
    let want = out.len().min(REQ_MAX);
    {
        let mut g = RNG.lock();
        let Some(r) = g.as_mut() else {
            return 0;
        };
        if !r.outstanding {
            let b = Buf {
                addr: r.buf.pa(0),
                len: REQ_MAX as u32,
                write: true,
            };
            if r.q.add(&[b]).is_none() {
                return 0;
            }
            r.outstanding = true;
            r.q.kick(&*r.dev.transport);
        }
    }
    let t0 = crate::arch::timer::now_ns();
    loop {
        {
            let mut g = RNG.lock();
            let r = g.as_mut().expect("rng vanished");
            if let Some((_, len)) = r.q.pop_used() {
                r.outstanding = false;
                let n = (len as usize).min(want);
                r.buf.read(0, &mut out[..n]);
                // Do not leave entropy that went into the generator behind.
                r.buf.write(0, &[0u8; REQ_MAX]);
                return n;
            }
        }
        let now = crate::arch::timer::now_ns();
        if now.saturating_sub(t0) >= timeout_ns {
            return 0;
        }
        if sleep {
            crate::task::wait::sleep_until(now + 1_000_000);
        } else {
            core::hint::spin_loop();
        }
    }
}
