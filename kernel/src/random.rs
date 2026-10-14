//! Kernel random bytes for `AT_RANDOM` and `getrandom`.
//!
//! NOT cryptographically secure. There is no entropy source yet (virtio-rng
//! is Phase 3), so this is a SplitMix64 generator seeded from the boot
//! counter (`timer::now_ns`, whose low bits vary from boot to boot with
//! firmware/VMM timing) mixed with `patina.epoch`. Go uses `AT_RANDOM` to
//! seed its hash functions and `getrandom` for `crypto/rand`; the latter
//! must get a real source before anything security-relevant runs on patina.
//! W2a decision, to be recorded in doc/decisions.md.

use crate::sync::SpinLock;

static STATE: SpinLock<u64> = SpinLock::new(0x9e37_79b9_7f4a_7c15);

/// Mix `seed` into the generator state.
pub fn seed(seed: u64) {
    let mut s = STATE.lock();
    let mut v = *s ^ seed;
    *s = splitmix(&mut v);
}

fn splitmix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Fill `buf` with pseudo-random bytes.
pub fn fill(buf: &mut [u8]) {
    let mut s = STATE.lock();
    // Stir in the clock each call so successive boots and calls diverge
    // even with identical seeds.
    *s ^= crate::arch::timer::now_ns().rotate_left(17);
    for chunk in buf.chunks_mut(8) {
        let v = splitmix(&mut s).to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
}
