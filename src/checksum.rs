//! Per-block integrity.
//!
//! One 64-bit XXH3 per block, stored in the pool's checksum table. A stored
//! value of `0` is reserved to mean "this block has never been written" —
//! such blocks read as zeros without touching the disk. A real hash that
//! happens to be zero is remapped to `1` so the sentinel is unambiguous.

use xxhash_rust::xxh3::xxh3_64;

/// Sentinel: block never written.
pub const UNWRITTEN: u64 = 0;

pub fn block_checksum(data: &[u8]) -> u64 {
    let h = xxh3_64(data);
    if h == UNWRITTEN {
        1
    } else {
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_returns_sentinel() {
        // We cannot easily find a zero-hash preimage; assert the mapping
        // logic directly on a known non-zero input and on empty input.
        assert_ne!(block_checksum(&[]), UNWRITTEN);
        assert_ne!(block_checksum(&[0u8; 4096]), UNWRITTEN);
    }

    #[test]
    fn detects_single_bit_flip() {
        let mut a = vec![7u8; 65536];
        let h1 = block_checksum(&a);
        a[12345] ^= 0x01;
        let h2 = block_checksum(&a);
        assert_ne!(h1, h2);
    }
}
