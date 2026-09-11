//! Deterministic block content for proofs and tests.
//!
//! `fill(seed, tag, block, buf)` produces the same bytes on every machine
//! and every run. Verification regenerates the pattern rather than keeping a
//! copy, so a proof over an 80 GiB region needs no 80 GiB of reference data
//! — which is also why the proof itself cannot accidentally hold the arena
//! in RAM.

use xxhash_rust::xxh3::xxh3_64_with_seed;

/// xorshift64* seeded from (seed, tag, block). Fast enough to generate at
/// several GiB/s on one core so pattern generation never masks I/O cost.
pub fn fill(seed: u64, tag: u8, block: u64, buf: &mut [u8]) {
    let mut key = [0u8; 9];
    key[..8].copy_from_slice(&block.to_le_bytes());
    key[8] = tag;
    let mut x = xxh3_64_with_seed(&key, seed) | 1; // never zero
    for chunk in buf.chunks_mut(8) {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let bytes = v.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// Byte offset of the first mismatch, if any.
pub fn first_mismatch(expected: &[u8], actual: &[u8]) -> Option<usize> {
    expected
        .iter()
        .zip(actual.iter())
        .position(|(a, b)| a != b)
        .or_else(|| {
            if expected.len() != actual.len() {
                Some(expected.len().min(actual.len()))
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_distinct() {
        let mut a = vec![0u8; 4096];
        let mut b = vec![0u8; 4096];
        fill(7, b'A', 42, &mut a);
        fill(7, b'A', 42, &mut b);
        assert_eq!(a, b);
        fill(7, b'B', 42, &mut b);
        assert_ne!(a, b);
        fill(7, b'A', 43, &mut b);
        assert_ne!(a, b);
        fill(8, b'A', 42, &mut b);
        assert_ne!(a, b);
    }

    #[test]
    fn mismatch_position() {
        let a = [1u8, 2, 3, 4];
        let mut b = a;
        assert_eq!(first_mismatch(&a, &b), None);
        b[2] = 9;
        assert_eq!(first_mismatch(&a, &b), Some(2));
        assert_eq!(first_mismatch(&a, &b[..3]), Some(2));
        assert_eq!(first_mismatch(&a, &a[..3]), Some(3));
    }
}
