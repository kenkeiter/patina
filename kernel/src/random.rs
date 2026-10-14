//! Kernel random bytes for `AT_RANDOM` and `getrandom`.
//!
//! NOT cryptographically secure. This is a SplitMix64 generator seeded
//! from the CPU's hardware RNG (`cpu::hw_random`: RNDR / RDSEED / RDRAND)
//! when it has one, mixed with the boot counter (`timer::now_ns`, whose
//! low bits vary from boot to boot with firmware/VMM timing) and
//! `patina.epoch`; without a hardware RNG only the latter two
//! (virtio-rng is Phase 3). Go uses `AT_RANDOM` to
//! seed its hash functions and `getrandom` for `crypto/rand`; the latter
//! must get a real source before anything security-relevant runs on patina.
//! See doc/decisions.md D35 (PRNG) and D50 (hardware seeding).

use crate::sync::SpinLock;

static STATE: SpinLock<u64> = SpinLock::new(0x9e37_79b9_7f4a_7c15);

/// Seed at boot: `seed` (boot counter / epoch) plus, if the CPU has one,
/// a few hardware RNG words. Logs which source was used.
pub fn seed_boot(seed_val: u64) {
    let mut hw = None;
    for _ in 0..4 {
        if let Some(v) = crate::arch::cpu::hw_random() {
            hw = Some(hw.unwrap_or(0u64).rotate_left(23) ^ v);
        }
    }
    seed(seed_val);
    match hw {
        Some(v) => {
            seed(v);
            crate::kprintln!("[patina] random: seeded from {}", HW_RNG_NAME);
        }
        None => crate::kprintln!("[patina] random: seeded from boot counter only"),
    }
}

#[cfg(target_arch = "aarch64")]
const HW_RNG_NAME: &str = "RNDR";
#[cfg(target_arch = "x86_64")]
const HW_RNG_NAME: &str = "RDSEED/RDRAND";

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
