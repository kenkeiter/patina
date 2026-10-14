//! The patina kernel's random number generator (replaces decision D35's
//! SplitMix64): a ChaCha20 DRBG with fast key erasure whose key is derived
//! with BLAKE2s from an entropy pool, in the style of Linux's `crng`
//! (`drivers/char/random.c`, 5.18+).
//!
//! * [`Drbg::add_entropy`] hashes input into the pool (BLAKE2s state).
//!   Inputs are never trusted to be uniform; only their entropy counts.
//! * [`Drbg::reseed`] finalizes the pool into a 32-byte seed, sets
//!   `key = BLAKE2s(old key ‖ seed)` and restarts the pool keyed with the
//!   seed, so earlier inputs keep contributing.
//! * [`Drbg::fill`] produces output with ChaCha20 (RFC 8439 block
//!   function, nonce 0): block 0 of the current key becomes the next key
//!   (fast key erasure: a later compromise of the state does not reveal
//!   earlier output), blocks 1.. are the output. The key changes after
//!   every call.
//!
//! No allocation, no `unsafe`, host-tested against the RFC 7693 and RFC
//! 8439 test vectors.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod blake2s;
pub mod chacha;

use blake2s::Blake2s;

/// Bytes of output after which the kernel should reseed (a policy hint
/// for the caller; the generator itself never blocks or fails).
pub const RESEED_AFTER_BYTES: u64 = 1 << 20;

/// ChaCha20 DRBG with a BLAKE2s entropy pool.
#[derive(Clone)]
pub struct Drbg {
    key: [u8; 32],
    pool: Blake2s,
    /// Bytes put into the pool since the last reseed.
    pool_bytes: usize,
    /// Bytes generated since the last reseed.
    since_reseed: u64,
    /// Number of reseeds so far.
    reseeds: u64,
}

impl core::fmt::Debug for Drbg {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Drbg")
            .field("reseeds", &self.reseeds)
            .field("since_reseed", &self.since_reseed)
            .finish_non_exhaustive()
    }
}

impl Default for Drbg {
    fn default() -> Self {
        Self::new()
    }
}

impl Drbg {
    /// An unseeded generator (all-zero key). Its output is deterministic
    /// until entropy is added and [`reseed`](Self::reseed) is called.
    pub const fn new() -> Self {
        Drbg {
            key: [0; 32],
            pool: Blake2s::new(),
            pool_bytes: 0,
            since_reseed: 0,
            reseeds: 0,
        }
    }

    /// Mix `data` into the entropy pool (takes effect at the next reseed).
    pub fn add_entropy(&mut self, data: &[u8]) {
        self.pool.update(data);
        self.pool_bytes = self.pool_bytes.saturating_add(data.len());
    }

    /// Fold the pool into the key.
    pub fn reseed(&mut self) {
        let seed = core::mem::take(&mut self.pool).finalize();
        let mut h = Blake2s::new();
        h.update(&self.key);
        h.update(&seed);
        self.key = h.finalize();
        self.pool.update(&seed);
        self.pool_bytes = 0;
        self.since_reseed = 0;
        self.reseeds += 1;
    }

    /// Number of reseeds so far (0 = never seeded).
    pub fn reseeds(&self) -> u64 {
        self.reseeds
    }

    /// Bytes generated since the last reseed.
    pub fn since_reseed(&self) -> u64 {
        self.since_reseed
    }

    /// Whether [`RESEED_AFTER_BYTES`] have been produced since the last
    /// reseed.
    pub fn wants_reseed(&self) -> bool {
        self.since_reseed >= RESEED_AFTER_BYTES
    }

    /// Fill `out` with output and advance the key.
    pub fn fill(&mut self, out: &mut [u8]) {
        let mut block = [0u8; 64];
        chacha::block(&self.key, 0, &[0; 12], &mut block);
        let mut next = [0u8; 32];
        next.copy_from_slice(&block[..32]);
        let mut counter = 1u32;
        for chunk in out.chunks_mut(64) {
            chacha::block(&self.key, counter, &[0; 12], &mut block);
            chunk.copy_from_slice(&block[..chunk.len()]);
            counter = counter.wrapping_add(1);
            if counter == 0 {
                // 256 GiB in one call: rekey from the stream and go on.
                chacha::block(&self.key, 0, &[1; 12], &mut block);
                self.key.copy_from_slice(&block[..32]);
            }
        }
        self.key = next;
        block.fill(0);
        self.since_reseed = self.since_reseed.saturating_add(out.len() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_key_erasing() {
        let mut a = Drbg::new();
        let mut b = Drbg::new();
        a.add_entropy(b"seed");
        b.add_entropy(b"seed");
        a.reseed();
        b.reseed();
        let (mut x, mut y) = ([0u8; 100], [0u8; 100]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_eq!(x, y);
        let mut z = [0u8; 100];
        a.fill(&mut z);
        assert_ne!(x, z, "key must change between calls");
        assert_eq!(a.since_reseed(), 200);
        assert_eq!(a.reseeds(), 1);
    }

    #[test]
    fn entropy_changes_output() {
        let mut a = Drbg::new();
        let mut b = Drbg::new();
        a.add_entropy(b"one");
        b.add_entropy(b"two");
        a.reseed();
        b.reseed();
        let (mut x, mut y) = ([0u8; 32], [0u8; 32]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_ne!(x, y);
        // Pool inputs persist across reseeds: same new input, different
        // history → different output.
        a.add_entropy(b"x");
        b.add_entropy(b"x");
        a.reseed();
        b.reseed();
        a.fill(&mut x);
        b.fill(&mut y);
        assert_ne!(x, y);
    }

    #[test]
    fn odd_lengths_and_reseed_hint() {
        let mut a = Drbg::new();
        a.add_entropy(&[7; 64]);
        a.reseed();
        for n in [0usize, 1, 63, 64, 65, 1000] {
            let mut v = [0u8; 1000];
            a.fill(&mut v[..n]);
            assert!(v[n..].iter().all(|&b| b == 0));
        }
        let mut big = [0u8; 4096];
        for _ in 0..(RESEED_AFTER_BYTES / 4096) {
            a.fill(&mut big);
        }
        assert!(a.wants_reseed());
        a.reseed();
        assert!(!a.wants_reseed());
    }

    #[test]
    fn output_looks_uniform() {
        let mut a = Drbg::new();
        a.add_entropy(b"stats");
        a.reseed();
        let mut buf = [0u8; 65536];
        a.fill(&mut buf);
        let mut counts = [0u32; 256];
        for &b in buf.iter() {
            counts[b as usize] += 1;
        }
        // Expected 256 each; a chi-square-ish sanity bound.
        assert!(
            counts.iter().all(|&c| (150..370).contains(&c)),
            "{counts:?}"
        );
    }
}
