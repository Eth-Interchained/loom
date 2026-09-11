//! On-disk layout of a pool file.
//!
//! ```text
//! offset 0            superblock          4 KiB   (this struct, little-endian)
//! offset 4 KiB        region table        64 KiB  (u64 count, then (offset,len) pairs)
//! offset 68 KiB       checksum table      nblocks * 8 bytes, rounded up to 4 KiB
//! data_off            block data          nblocks * block_size
//! ```
//!
//! Logical block `n` lives at `data_off + n * block_size` — an identity
//! mapping. v0 deliberately has no backing-block allocator: the file is
//! sparse, so unwritten logical blocks cost nothing on disk, and the mapping
//! needs no metadata that could be lost. Everything above `data_off` is
//! rebuildable from the superblock alone.
//!
//! The superblock and both tables are written with the same aligned,
//! cache-bypassing I/O as data.

use crate::aligned::{AlignedBuf, IO_ALIGN};
use crate::error::{LoomError, Result};

pub const MAGIC: [u8; 8] = *b"LOOMPOOL";
pub const FORMAT_VERSION: u32 = 1;
pub const SUPERBLOCK_LEN: u64 = IO_ALIGN as u64;
pub const REGION_TABLE_OFF: u64 = SUPERBLOCK_LEN;
pub const REGION_TABLE_LEN: u64 = 64 * 1024;
pub const MAX_REGIONS: usize = ((REGION_TABLE_LEN - 8) / 16) as usize; // 4095
pub const CHECKSUM_TABLE_OFF: u64 = REGION_TABLE_OFF + REGION_TABLE_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub block_size: u32,
    /// Logical arena capacity in bytes (a multiple of block_size).
    pub capacity: u64,
    /// Hot budget recorded at init; `open` may override it.
    pub default_budget: u64,
    pub nblocks: u64,
    pub checksum_table_len: u64,
    pub data_off: u64,
}

impl Superblock {
    pub fn new(block_size: u32, capacity: u64, default_budget: u64) -> Result<Self> {
        if block_size == 0 || block_size as usize % IO_ALIGN != 0 {
            return Err(LoomError::Invalid(format!(
                "block size {block_size} must be a non-zero multiple of {IO_ALIGN}"
            )));
        }
        if capacity == 0 || capacity % block_size as u64 != 0 {
            return Err(LoomError::Invalid(format!(
                "capacity {capacity} must be a non-zero multiple of block size {block_size}"
            )));
        }
        let nblocks = capacity / block_size as u64;
        let raw = nblocks * 8;
        let checksum_table_len = raw.div_ceil(IO_ALIGN as u64) * IO_ALIGN as u64;
        let data_off = CHECKSUM_TABLE_OFF + checksum_table_len;
        Ok(Superblock {
            block_size,
            capacity,
            default_budget,
            nblocks,
            checksum_table_len,
            data_off,
        })
    }

    pub fn file_len(&self) -> u64 {
        self.data_off + self.capacity
    }

    pub fn block_off(&self, block: u64) -> u64 {
        self.data_off + block * self.block_size as u64
    }

    pub fn encode(&self) -> AlignedBuf {
        let mut b = AlignedBuf::zeroed(IO_ALIGN);
        b[0..8].copy_from_slice(&MAGIC);
        b[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&self.block_size.to_le_bytes());
        b[16..24].copy_from_slice(&self.capacity.to_le_bytes());
        b[24..32].copy_from_slice(&self.default_budget.to_le_bytes());
        b[32..40].copy_from_slice(&self.nblocks.to_le_bytes());
        b[40..48].copy_from_slice(&self.checksum_table_len.to_le_bytes());
        b[48..56].copy_from_slice(&self.data_off.to_le_bytes());
        // Header checksum over bytes [0, 56) so a torn superblock is detected.
        let h = xxhash_rust::xxh3::xxh3_64(&b[0..56]);
        b[56..64].copy_from_slice(&h.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < 64 {
            return Err(LoomError::BadSuperblock(format!(
                "buffer too short ({} bytes)",
                b.len()
            )));
        }
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&b[0..8]);
        if magic != MAGIC {
            return Err(LoomError::BadMagic { found: magic });
        }
        let u32le = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap());
        let u64le = |s: &[u8]| u64::from_le_bytes(s.try_into().unwrap());
        let version = u32le(&b[8..12]);
        if version != FORMAT_VERSION {
            return Err(LoomError::Version {
                found: version,
                supported: FORMAT_VERSION,
            });
        }
        let stored_h = u64le(&b[56..64]);
        let h = xxhash_rust::xxh3::xxh3_64(&b[0..56]);
        if stored_h != h {
            return Err(LoomError::BadSuperblock(format!(
                "header checksum mismatch (stored {stored_h:#x}, computed {h:#x})"
            )));
        }
        let sb = Superblock {
            block_size: u32le(&b[12..16]),
            capacity: u64le(&b[16..24]),
            default_budget: u64le(&b[24..32]),
            nblocks: u64le(&b[32..40]),
            checksum_table_len: u64le(&b[40..48]),
            data_off: u64le(&b[48..56]),
        };
        // Recompute derived fields and demand agreement.
        let expect = Superblock::new(sb.block_size, sb.capacity, sb.default_budget)
            .map_err(|e| LoomError::BadSuperblock(e.to_string()))?;
        if expect != sb {
            return Err(LoomError::BadSuperblock(format!(
                "derived fields disagree: stored {sb:?}, recomputed {expect:?}"
            )));
        }
        Ok(sb)
    }
}

/// Region table codec. Regions are (offset, len) in arena bytes, appended
/// in allocation order; v0 never frees.
pub fn encode_regions(regions: &[(u64, u64)]) -> Result<AlignedBuf> {
    if regions.len() > MAX_REGIONS {
        return Err(LoomError::RegionTableFull { max: MAX_REGIONS });
    }
    let mut b = AlignedBuf::zeroed(REGION_TABLE_LEN as usize);
    b[0..8].copy_from_slice(&(regions.len() as u64).to_le_bytes());
    for (i, (off, len)) in regions.iter().enumerate() {
        let p = 8 + i * 16;
        b[p..p + 8].copy_from_slice(&off.to_le_bytes());
        b[p + 8..p + 16].copy_from_slice(&len.to_le_bytes());
    }
    Ok(b)
}

pub fn decode_regions(b: &[u8]) -> Result<Vec<(u64, u64)>> {
    let n = u64::from_le_bytes(b[0..8].try_into().unwrap()) as usize;
    if n > MAX_REGIONS {
        return Err(LoomError::BadSuperblock(format!(
            "region table claims {n} regions (max {MAX_REGIONS})"
        )));
    }
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let p = 8 + i * 16;
        let off = u64::from_le_bytes(b[p..p + 8].try_into().unwrap());
        let len = u64::from_le_bytes(b[p + 8..p + 16].try_into().unwrap());
        v.push((off, len));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let sb = Superblock::new(65536, 1 << 30, 64 << 20).unwrap();
        assert_eq!(sb.nblocks, 16384);
        assert_eq!(sb.checksum_table_len, 16384 * 8); // already 4K multiple
        let enc = sb.encode();
        let dec = Superblock::decode(&enc).unwrap();
        assert_eq!(sb, dec);
    }

    #[test]
    fn rejects_bad_magic_version_and_torn_header() {
        let sb = Superblock::new(65536, 1 << 30, 0).unwrap();
        let mut enc = sb.encode();
        enc[0] = b'X';
        assert!(matches!(
            Superblock::decode(&enc),
            Err(LoomError::BadMagic { .. })
        ));
        let mut enc = sb.encode();
        enc[8] = 9;
        assert!(matches!(
            Superblock::decode(&enc),
            Err(LoomError::Version { .. })
        ));
        let mut enc = sb.encode();
        enc[20] ^= 1; // capacity bit
        assert!(matches!(
            Superblock::decode(&enc),
            Err(LoomError::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_bad_geometry() {
        assert!(Superblock::new(4000, 1 << 30, 0).is_err());
        assert!(Superblock::new(65536, 65536 + 1, 0).is_err());
        assert!(Superblock::new(65536, 0, 0).is_err());
    }

    #[test]
    fn regions_roundtrip() {
        let r = vec![(0u64, 100u64), (100, 4096), (4196, 1 << 40)];
        let enc = encode_regions(&r).unwrap();
        assert_eq!(decode_regions(&enc).unwrap(), r);
        assert_eq!(
            decode_regions(&encode_regions(&[]).unwrap()).unwrap(),
            vec![]
        );
    }
}
