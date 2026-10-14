//! Kernel random bytes for `AT_RANDOM`, `getrandom` and `/dev/urandom`
//! (decision D60, replacing D35).
//!
//! The generator is `patina_rand::Drbg`: ChaCha20 with fast key erasure,
//! keyed from a BLAKE2s entropy pool. Inputs at boot ([`seed_boot`]): 64
//! bytes from virtio-rng when the VMM provides one (bounded 100 ms wait),
//! up to four words of the CPU's hardware RNG (`cpu::hw_random`: RNDR /
//! RDSEED / RDRAND), the boot counter and `patina.epoch`. Afterwards a
//! kernel thread reseeds from virtio-rng every [`RESEED_INTERVAL_NS`] and
//! whenever 1 MiB has been generated since the last reseed
//! (`patina_rand::RESEED_AFTER_BYTES`). Without virtio-rng and without a
//! CPU RNG the output is only as unpredictable as the boot timing, and the
//! boot log says so.

use core::sync::atomic::{AtomicBool, Ordering};

use patina_rand::Drbg;

use crate::drivers::Event;
use crate::drivers::virtio::rng;
use crate::sync::SpinLock;

static STATE: SpinLock<Drbg> = SpinLock::new(Drbg::new());
static RESEED: Event = Event::new();
static THREAD: AtomicBool = AtomicBool::new(false);

/// Periodic reseed from virtio-rng.
pub const RESEED_INTERVAL_NS: u64 = 60_000_000_000;

#[cfg(target_arch = "aarch64")]
const HW_RNG_NAME: &str = "RNDR";
#[cfg(target_arch = "x86_64")]
const HW_RNG_NAME: &str = "RDSEED/RDRAND";

/// Seed at boot (see the module docs) and start the reseed thread when
/// virtio-rng is present. Logs the sources used.
pub fn seed_boot(seed_val: u64) {
    let mut virtio = [0u8; 64];
    let nv = rng::read(&mut virtio, 100_000_000, false);
    let mut hw = 0;
    {
        let mut s = STATE.lock();
        s.add_entropy(&virtio[..nv]);
        for _ in 0..4 {
            if let Some(v) = crate::arch::cpu::hw_random() {
                s.add_entropy(&v.to_le_bytes());
                hw += 1;
            }
        }
        s.add_entropy(&seed_val.to_le_bytes());
        s.add_entropy(&crate::arch::timer::now_ns().to_le_bytes());
        s.reseed();
    }
    virtio.fill(0);
    let hw_name = if hw > 0 { HW_RNG_NAME } else { "" };
    if nv > 0 {
        crate::kprintln!(
            "[patina] random: seeded from virtio-rng ({} bytes){}{}",
            nv,
            if hw > 0 { " + " } else { "" },
            hw_name
        );
    } else if hw > 0 {
        crate::kprintln!("[patina] random: seeded from {} (no virtio-rng)", hw_name);
    } else {
        crate::kprintln!(
            "[patina] random: WARNING: seeded from boot counter only (no virtio-rng, no cpu rng)"
        );
    }
    if rng::present() && !THREAD.swap(true, Ordering::Relaxed) {
        crate::task::spawn_kernel_thread(reseed_thread);
    }
}

/// Mix `data` into the pool and reseed at once.
pub fn add_entropy_and_reseed(data: &[u8]) {
    let mut s = STATE.lock();
    s.add_entropy(data);
    s.reseed();
}

fn reseed_thread() {
    loop {
        let deadline = crate::arch::timer::now_ns() + RESEED_INTERVAL_NS;
        RESEED.wait(Some(deadline));
        let mut buf = [0u8; 32];
        let n = rng::read(&mut buf, 1_000_000_000, true);
        if n > 0 {
            add_entropy_and_reseed(&buf[..n]);
        }
        buf.fill(0);
    }
}

/// Fill `buf` with random bytes. Never blocks (spinlock only), any context.
pub fn fill(buf: &mut [u8]) {
    let wants = {
        let mut s = STATE.lock();
        // Stir in the clock: cheap, and keeps an unseeded boot from
        // repeating itself exactly.
        s.add_entropy(&crate::arch::timer::now_ns().to_le_bytes());
        s.fill(buf);
        s.wants_reseed()
    };
    if wants && THREAD.load(Ordering::Relaxed) {
        RESEED.signal();
    }
}
