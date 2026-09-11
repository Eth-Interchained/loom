//! The arena: a logical address space larger than RAM, served from a
//! bounded frame pool with explicit residency.
//!
//! What Loom owns here (and mmap would not give us):
//! - the block map: logical block → resident frame or none
//! - the residency decision on every miss (free frame, else CLOCK victim)
//! - dirty tracking and writeback timing
//! - a checksum per block, verified on every promotion from disk
//! - the counters that describe all of the above
//!
//! What stays with the kernel: physical backing of the frames themselves.
//! See [`crate::footprint`] for how that is measured rather than assumed.

use crate::aligned::AlignedBuf;
use crate::checksum::{block_checksum, UNWRITTEN};
use crate::error::{LoomError, Result};
use crate::frames::FramePool;
use crate::io::Backing;
use crate::stats::Stats;
use crate::superblock::{
    decode_regions, encode_regions, Superblock, MAX_REGIONS, REGION_TABLE_LEN, REGION_TABLE_OFF,
    SUPERBLOCK_LEN,
};
use std::path::Path;
use std::time::Instant;

const NO_FRAME: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub id: u32,
    /// Arena byte offset (block-aligned).
    pub offset: u64,
    /// Usable length as requested.
    pub len: u64,
}

#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub capacity: u64,
    pub block_size: u32,
    /// Default hot budget recorded in the pool; `open` may override.
    pub budget: u64,
}

#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Override the pool's recorded default budget.
    pub budget: Option<u64>,
    /// Blocks to read per speculative batch once a sequential run is
    /// detected. `None` uses [`DEFAULT_PREFETCH_DEPTH`]; `Some(0)` disables
    /// prefetch entirely (useful for measuring what it is worth).
    pub prefetch_depth: Option<usize>,
}

/// Blocks per speculative batch. At 64 KiB blocks that is one 1 MiB read
/// instead of sixteen 64 KiB reads — the point is amortising the device
/// round-trip, which dominates everything on a seek-bound device.
pub const DEFAULT_PREFETCH_DEPTH: usize = 16;

/// A sequential run must be this long before speculating. Two consecutive
/// blocks is a coincidence; three is a pattern. Guessing earlier wastes
/// bandwidth on random workloads.
const SEQ_RUN_THRESHOLD: u32 = 3;

/// Static facts about an open arena — the numbers Loom can state as
/// allocations, not as hopes.
#[derive(Debug, Clone)]
pub struct Info {
    pub path: String,
    pub capacity: u64,
    pub block_size: u32,
    pub nblocks: u64,
    pub budget: u64,
    pub frame_count: usize,
    /// Bytes of frame memory actually allocated (== frame_count * block_size).
    pub hot_bytes_allocated: u64,
    /// Block map + checksum table + region table, in RAM.
    pub metadata_bytes: u64,
    pub regions: usize,
    pub allocated_arena_bytes: u64,
    pub cache_mode: &'static str,
    /// Bytes the backing file currently occupies on disk.
    pub disk_allocated: u64,
}

pub struct Loom {
    backing: Backing,
    sb: Superblock,
    regions: Vec<(u64, u64)>,
    checksums: AlignedBuf,
    map: Vec<u32>,
    frames: FramePool,
    budget: u64,
    stats: Stats,
    dirty_meta: bool,
    /// Speculative-read depth in blocks; 0 disables.
    prefetch_depth: usize,
    /// Staging buffer for batched reads. Allocated only if prefetch is on,
    /// and counted in `metadata_bytes` so it cannot hide from the budget.
    prefetch_buf: Option<AlignedBuf>,
    /// Last block a miss was taken on, and how long the ascending run is.
    last_miss_block: Option<u64>,
    seq_run: u32,
}

impl Loom {
    /// Create a new pool file and open it with default open options.
    pub fn create(path: &Path, opts: CreateOptions) -> Result<Self> {
        Self::create_with(path, opts, OpenOptions::default())
    }

    /// Create a new pool file and open it with the given open options.
    ///
    /// Separate from [`Loom::create`] because a caller who asks for, say,
    /// prefetch disabled must get that on the freshly created arena too —
    /// not only after a close and reopen. `create` silently substituting
    /// defaults made a measurement of "prefetch off" report prefetch
    /// happening, which is exactly the kind of quiet substitution this
    /// codebase is supposed to refuse.
    pub fn create_with(path: &Path, opts: CreateOptions, open: OpenOptions) -> Result<Self> {
        let sb = Superblock::new(opts.block_size, opts.capacity, opts.budget)?;
        if opts.budget < opts.block_size as u64 {
            return Err(LoomError::Invalid(format!(
                "budget {} smaller than one block ({})",
                opts.budget, opts.block_size
            )));
        }
        let backing = Backing::open(path, true)?;
        backing.set_len(sb.file_len())?;
        backing.pwrite_all(&sb.encode(), 0)?;
        backing.pwrite_all(&encode_regions(&[])?, REGION_TABLE_OFF)?;
        // The checksum table is left as sparse zeros: 0 == UNWRITTEN.
        backing.sync()?;
        drop(backing);
        Self::open(path, open)
    }

    pub fn open(path: &Path, opts: OpenOptions) -> Result<Self> {
        let backing = Backing::open(path, false)?;
        let mut hdr = AlignedBuf::zeroed(SUPERBLOCK_LEN as usize);
        backing.pread_exact(&mut hdr, 0)?;
        let sb = Superblock::decode(&hdr)?;
        let (file_len, _) = backing.sizes()?;
        if file_len != sb.file_len() {
            return Err(LoomError::BadSuperblock(format!(
                "file length {} does not match superblock geometry {}",
                file_len,
                sb.file_len()
            )));
        }
        let mut rt = AlignedBuf::zeroed(REGION_TABLE_LEN as usize);
        backing.pread_exact(&mut rt, REGION_TABLE_OFF)?;
        let regions = decode_regions(&rt)?;
        for (i, (off, len)) in regions.iter().enumerate() {
            if off
                .checked_add(*len)
                .map(|e| e > sb.capacity)
                .unwrap_or(true)
            {
                return Err(LoomError::BadSuperblock(format!(
                    "region {i} [{off}, +{len}) exceeds capacity {}",
                    sb.capacity
                )));
            }
        }
        let mut checksums = AlignedBuf::zeroed(sb.checksum_table_len as usize);
        backing.pread_exact(&mut checksums, crate::superblock::CHECKSUM_TABLE_OFF)?;

        let budget = opts.budget.unwrap_or(sb.default_budget);
        let frame_count = (budget / sb.block_size as u64) as usize;
        if frame_count == 0 {
            return Err(LoomError::Invalid(format!(
                "budget {} yields zero frames of {} bytes",
                budget, sb.block_size
            )));
        }
        let frames = FramePool::new(frame_count, sb.block_size as usize);
        let map = vec![NO_FRAME; sb.nblocks as usize];
        // Never speculate over more than a quarter of the pool: a batch
        // that evicts most of the hot set to make room for guesses is a
        // thrash machine, not a prefetcher.
        let prefetch_depth = opts
            .prefetch_depth
            .unwrap_or(DEFAULT_PREFETCH_DEPTH)
            .min(frame_count / 4);
        let prefetch_buf = if prefetch_depth > 1 {
            Some(AlignedBuf::zeroed(prefetch_depth * sb.block_size as usize))
        } else {
            None
        };
        Ok(Loom {
            backing,
            sb,
            regions,
            checksums,
            map,
            frames,
            budget,
            stats: Stats::default(),
            dirty_meta: false,
            prefetch_depth,
            prefetch_buf,
            last_miss_block: None,
            seq_run: 0,
        })
    }

    // ---- geometry -------------------------------------------------------

    pub fn block_size(&self) -> u32 {
        self.sb.block_size
    }

    pub fn capacity(&self) -> u64 {
        self.sb.capacity
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }

    pub fn frame_count(&self) -> usize {
        self.frames.frame_count()
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn regions(&self) -> Vec<Region> {
        self.regions
            .iter()
            .enumerate()
            .map(|(i, (o, l))| Region {
                id: i as u32,
                offset: *o,
                len: *l,
            })
            .collect()
    }

    pub fn metadata_bytes(&self) -> u64 {
        (self.map.len() * std::mem::size_of::<u32>()) as u64
            + self.checksums.len() as u64
            + REGION_TABLE_LEN
            + (self.frames.frame_count() * std::mem::size_of::<crate::frames::FrameMeta>()) as u64
            + self.prefetch_buf.as_ref().map(|b| b.len()).unwrap_or(0) as u64
    }

    pub fn prefetch_depth(&self) -> usize {
        self.prefetch_depth
    }

    pub fn info(&self) -> Result<Info> {
        let (_, disk) = self.backing.sizes()?;
        Ok(Info {
            path: self.backing.path().to_string(),
            capacity: self.sb.capacity,
            block_size: self.sb.block_size,
            nblocks: self.sb.nblocks,
            budget: self.budget,
            frame_count: self.frames.frame_count(),
            hot_bytes_allocated: self.frames.bytes() as u64,
            metadata_bytes: self.metadata_bytes(),
            regions: self.regions.len(),
            allocated_arena_bytes: self.next_free_offset(),
            cache_mode: self.backing.cache_mode,
            disk_allocated: disk,
        })
    }

    pub fn resident_blocks(&self) -> usize {
        self.frames.resident_count()
    }

    pub fn dirty_blocks(&self) -> usize {
        self.frames.dirty_count()
    }

    /// Byte offset in the backing file of a logical block. Exposed so a proof
    /// can tamper with exactly one block on disk and check detection.
    pub fn backing_offset_of_block(&self, block: u64) -> u64 {
        self.sb.block_off(block)
    }

    pub fn block_of_region_offset(&self, r: Region, off: u64) -> u64 {
        (r.offset + off) / self.sb.block_size as u64
    }

    // ---- regions --------------------------------------------------------

    fn next_free_offset(&self) -> u64 {
        self.regions
            .last()
            .map(|(o, l)| self.round_up(*o + *l))
            .unwrap_or(0)
    }

    fn round_up(&self, v: u64) -> u64 {
        let bs = self.sb.block_size as u64;
        v.div_ceil(bs) * bs
    }

    /// Allocate a region of `len` bytes, block-aligned, from the arena.
    /// v0 never frees; the region table persists on `sync`.
    pub fn alloc(&mut self, len: u64) -> Result<Region> {
        if len == 0 {
            return Err(LoomError::Invalid("alloc of zero bytes".into()));
        }
        if self.regions.len() >= MAX_REGIONS {
            return Err(LoomError::RegionTableFull { max: MAX_REGIONS });
        }
        let start = self.next_free_offset();
        let end = start
            .checked_add(self.round_up(len))
            .ok_or_else(|| LoomError::Invalid("region length overflow".into()))?;
        if end > self.sb.capacity {
            return Err(LoomError::Capacity {
                requested: len,
                available: self.sb.capacity.saturating_sub(start),
            });
        }
        self.regions.push((start, len));
        self.dirty_meta = true;
        Ok(Region {
            id: (self.regions.len() - 1) as u32,
            offset: start,
            len,
        })
    }

    fn check_bounds(&self, r: Region, off: u64, len: u64) -> Result<()> {
        let ok = self
            .regions
            .get(r.id as usize)
            .map(|&(o, l)| o == r.offset && l == r.len)
            .unwrap_or(false)
            && off.checked_add(len).map(|e| e <= r.len).unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(LoomError::OutOfBounds {
                region: r.id,
                offset: off,
                len,
                region_len: r.len,
            })
        }
    }

    // ---- residency ------------------------------------------------------

    fn ck(&self, block: u64) -> u64 {
        let p = block as usize * 8;
        u64::from_le_bytes(self.checksums[p..p + 8].try_into().unwrap())
    }

    fn set_ck(&mut self, block: u64, v: u64) {
        let p = block as usize * 8;
        self.checksums[p..p + 8].copy_from_slice(&v.to_le_bytes());
        self.dirty_meta = true;
    }

    fn writeback(&mut self, frame: u32) -> Result<()> {
        let m = self.frames.meta(frame);
        debug_assert!(m.dirty);
        let t = Instant::now();
        let h = block_checksum(self.frames.data(frame));
        let off = self.sb.block_off(m.block);
        self.backing.pwrite_all(self.frames.data(frame), off)?;
        self.set_ck(m.block, h);
        self.frames.mark_clean(frame);
        self.stats.writebacks += 1;
        self.stats.bytes_written_backing += self.sb.block_size as u64;
        self.stats.writeback_latency.record(t.elapsed());
        Ok(())
    }

    /// Load `block` and up to `depth-1` following blocks in ONE read.
    ///
    /// The point is the device round-trip, not the bytes: sixteen 64 KiB
    /// reads cost sixteen seeks, one 1 MiB read costs one. On the first
    /// hardware run a seek was 10.49 ms and the bytes were free, so this is
    /// the single biggest lever available.
    ///
    /// Rules it will not break:
    /// - Speculative blocks only take free or CLEAN frames. Prefetch must
    ///   never turn a read into a writeback; that would cost more than it saves.
    /// - A resident block is never clobbered — it may be dirty, and the
    ///   frame is the only copy of those bytes.
    /// - A never-written block (checksum == UNWRITTEN) is zero-filled, not
    ///   trusted: whatever the disk holds there is meaningless.
    /// - A checksum mismatch on a *speculative* block is skipped silently
    ///   rather than returned as an error, because the caller did not ask
    ///   for it. It will be re-read, verified and reported the moment
    ///   someone actually does.
    ///
    /// Returns the number of blocks made resident, or None if the batch
    /// could not be attempted (no staging buffer, no reclaimable frames).
    fn prefetch_batch(&mut self, first: u64, depth: usize) -> Result<Option<u64>> {
        let bs = self.sb.block_size as usize;
        let count = (depth as u64).min(self.sb.nblocks - first);
        if count < 2 || self.prefetch_buf.is_none() {
            return Ok(None);
        }
        // Claim frames up front. If we cannot get one for a given block we
        // simply load fewer — a short batch is still one seek.
        let mut claimed: Vec<(u64, u32)> = Vec::with_capacity(count as usize);
        for i in 0..count {
            let b = first + i;
            if self.map[b as usize] != NO_FRAME {
                continue; // already resident; never clobber
            }
            match self.frames.take_free_or_clean() {
                Some((f, outgoing)) => {
                    // The frame arrives detached; its previous block's map
                    // entry is ours to clear. Leaving it would point the old
                    // block at a frame now holding different bytes —
                    // silently wrong data on a later read.
                    if outgoing != crate::frames::NO_BLOCK {
                        self.map[outgoing as usize] = NO_FRAME;
                        self.stats.evictions += 1;
                    }
                    claimed.push((b, f))
                }
                None => break,
            }
        }
        if claimed.is_empty() {
            return Ok(None);
        }
        // One read covering the whole span, including blocks we skipped.
        let span = count as usize * bs;
        let off = self.sb.block_off(first);
        let mut buf = self.prefetch_buf.take().expect("checked above");
        let read = self.backing.pread_exact(&mut buf[..span], off);
        if let Err(e) = read {
            // Hand back every frame we took, then report. Nothing is left
            // half-bound.
            for (_, f) in claimed {
                self.frames.release(f);
            }
            self.prefetch_buf = Some(buf);
            return Err(e);
        }
        self.stats.prefetch_batches += 1;
        self.stats.bytes_read_backing += span as u64;

        let mut loaded = 0u64;
        for (b, f) in claimed {
            let i = (b - first) as usize;
            let src = &buf[i * bs..(i + 1) * bs];
            let stored = self.ck(b);
            if stored == UNWRITTEN {
                self.frames.data_mut(f).fill(0);
            } else if block_checksum(src) != stored {
                // Not ours to complain about — nobody asked for this block.
                // `b` was never mapped and the outgoing block's mapping was
                // already cleared, so returning the frame leaves no stale
                // entry behind.
                self.frames.release(f);
                continue;
            } else {
                self.frames.data_mut(f).copy_from_slice(src);
            }
            self.frames.assign_prefetched(f, b);
            self.map[b as usize] = f;
            loaded += 1;
        }
        self.prefetch_buf = Some(buf);
        self.stats.prefetch_blocks += loaded;
        Ok(Some(loaded))
    }

    /// Make `block` resident and return its frame. If `full_overwrite`, the
    /// caller promises to overwrite every byte before reading, so the load
    /// from disk is skipped.
    fn ensure_resident(&mut self, block: u64, full_overwrite: bool) -> Result<u32> {
        let t = Instant::now();
        let f = self.map[block as usize];
        if f != NO_FRAME {
            if self.frames.claim_prefetch(f) {
                self.stats.prefetch_used += 1;
            }
            self.frames.touch(f);
            self.stats.hits += 1;
            self.stats.hit_latency.record(t.elapsed());
            return Ok(f);
        }

        // Miss. Update the sequential-run detector before doing anything
        // else, so the decision is made on the access pattern rather than
        // on what the pool happens to hold.
        self.seq_run = match self.last_miss_block {
            Some(prev) if block == prev + 1 => self.seq_run.saturating_add(1),
            _ => 1,
        };
        self.last_miss_block = Some(block);

        // Speculate only on an established forward run, and never when the
        // caller is about to overwrite the whole block (they want no bytes
        // from disk at all).
        if !full_overwrite && self.prefetch_depth > 1 && self.seq_run >= SEQ_RUN_THRESHOLD {
            let depth = self.prefetch_depth;
            if self.prefetch_batch(block, depth)?.is_some() {
                let f = self.map[block as usize];
                if f != NO_FRAME {
                    // The batch served the requested block. Still a miss —
                    // it was not resident when asked for — but it cost a
                    // shared seek rather than its own.
                    self.frames.claim_prefetch(f);
                    self.frames.touch(f);
                    self.stats.misses += 1;
                    self.stats.miss_latency.record(t.elapsed());
                    return Ok(f);
                }
            }
        }

        let frame = match self.frames.take_free() {
            Some(f) => f,
            None => {
                let victim = self.frames.choose_victim();
                let vm = self.frames.meta(victim);
                if vm.dirty {
                    self.writeback(victim)?;
                }
                self.map[vm.block as usize] = NO_FRAME;
                self.frames.release(victim);
                self.stats.evictions += 1;
                self.frames.take_free().expect("just released")
            }
        };

        if full_overwrite {
            self.stats.full_overwrites += 1;
        } else {
            let stored = self.ck(block);
            if stored == UNWRITTEN {
                self.frames.data_mut(frame).fill(0);
                self.stats.zero_fills += 1;
            } else {
                let off = self.sb.block_off(block);
                if let Err(e) = self.backing.pread_exact(self.frames.data_mut(frame), off) {
                    self.frames.release(frame);
                    return Err(e);
                }
                self.stats.bytes_read_backing += self.sb.block_size as u64;
                let actual = block_checksum(self.frames.data(frame));
                if actual != stored {
                    // Do not leave poisoned bytes addressable.
                    self.frames.data_mut(frame).fill(0);
                    self.frames.release(frame);
                    return Err(LoomError::Corrupt {
                        block,
                        expected: stored,
                        actual,
                    });
                }
            }
        }
        self.frames.assign(frame, block, false);
        self.map[block as usize] = frame;
        self.stats.misses += 1;
        self.stats.miss_latency.record(t.elapsed());
        Ok(frame)
    }

    // ---- zero-copy access ----------------------------------------------
    //
    // `read`/`write` copy between the caller's buffer and the frame. On the
    // first hardware run that copy was 41µs of a 41µs hit — 99.5% of the
    // cost of serving hot data, and nothing to do with the backing device.
    //
    // These two give the caller the frame's bytes directly. Soundness comes
    // from the borrow checker rather than a pin count: the closure runs
    // while `&mut self` is held, so no other arena call — and therefore no
    // eviction — can happen while the borrow is live. Zero copy, zero
    // `unsafe`, no way to hold a stale frame.
    //
    // The cost of that simplicity: one block at a time, and the slice cannot
    // outlive the closure. A caller needing two blocks at once wants the
    // explicit pin-count API, which is not built yet.

    /// Borrow up to `max` bytes at `off` **in place**, with no copy.
    ///
    /// The slice passed to `f` stops at the end of the containing block, so
    /// it may be shorter than `max`; its length is the caller's signal to
    /// loop. Returns whatever `f` returns.
    pub fn with_slice<R>(
        &mut self,
        r: Region,
        off: u64,
        max: usize,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<R> {
        let (frame, inb, n) = self.locate(r, off, max, false)?;
        self.stats.zero_copy += 1;
        let data = &self.frames.data(frame)[inb..inb + n];
        Ok(f(data))
    }

    /// Mutable in-place borrow. The block is marked dirty unconditionally —
    /// the caller was handed write access, so Loom must assume it was used.
    pub fn with_slice_mut<R>(
        &mut self,
        r: Region,
        off: u64,
        max: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R> {
        let (frame, inb, n) = self.locate(r, off, max, false)?;
        self.stats.zero_copy += 1;
        let out = {
            let data = &mut self.frames.data_mut(frame)[inb..inb + n];
            f(data)
        };
        self.frames.mark_dirty(frame);
        Ok(out)
    }

    /// Resolve `off` to (frame, offset within frame, contiguous length),
    /// making the block resident if it isn't. Shared by the copying and
    /// zero-copy paths so there is exactly one residency code path.
    fn locate(
        &mut self,
        r: Region,
        off: u64,
        max: usize,
        full_overwrite: bool,
    ) -> Result<(u32, usize, usize)> {
        self.check_bounds(r, off, max.min(1) as u64)?;
        let bs = self.sb.block_size as u64;
        let abs = r.offset + off;
        let block = abs / bs;
        let inb = (abs % bs) as usize;
        // Clamp to the block end, and to what the region actually holds.
        let n = max
            .min(bs as usize - inb)
            .min((r.len - off).min(usize::MAX as u64) as usize);
        if n == 0 {
            return Err(LoomError::OutOfBounds {
                region: r.id,
                offset: off,
                len: max as u64,
                region_len: r.len,
            });
        }
        let frame = self.ensure_resident(block, full_overwrite && inb == 0 && n == bs as usize)?;
        Ok((frame, inb, n))
    }

    // ---- data path ------------------------------------------------------

    pub fn read(&mut self, r: Region, off: u64, buf: &mut [u8]) -> Result<()> {
        self.check_bounds(r, off, buf.len() as u64)?;
        let bs = self.sb.block_size as u64;
        let mut abs = r.offset + off;
        let mut done = 0usize;
        while done < buf.len() {
            let block = abs / bs;
            let inb = (abs % bs) as usize;
            let n = ((bs as usize) - inb).min(buf.len() - done);
            let f = self.ensure_resident(block, false)?;
            buf[done..done + n].copy_from_slice(&self.frames.data(f)[inb..inb + n]);
            done += n;
            abs += n as u64;
        }
        Ok(())
    }

    pub fn write(&mut self, r: Region, off: u64, data: &[u8]) -> Result<()> {
        self.check_bounds(r, off, data.len() as u64)?;
        let bs = self.sb.block_size as u64;
        let mut abs = r.offset + off;
        let mut done = 0usize;
        while done < data.len() {
            let block = abs / bs;
            let inb = (abs % bs) as usize;
            let n = ((bs as usize) - inb).min(data.len() - done);
            let full = inb == 0 && n == bs as usize;
            let f = self.ensure_resident(block, full)?;
            self.frames.data_mut(f)[inb..inb + n].copy_from_slice(&data[done..done + n]);
            self.frames.mark_dirty(f);
            done += n;
            abs += n as u64;
        }
        Ok(())
    }

    // ---- durability -----------------------------------------------------

    /// Write back every dirty frame, then persist the checksum table, region
    /// table and superblock, then force to stable storage. After `sync`
    /// returns, a clean reopen sees everything written so far. A crash
    /// *between* syncs is out of scope for v0 (see README).
    pub fn sync(&mut self) -> Result<()> {
        let dirty: Vec<u32> = self
            .frames
            .resident()
            .filter(|(_, m)| m.dirty)
            .map(|(f, _)| f)
            .collect();
        for f in dirty {
            self.writeback(f)?;
        }
        if self.dirty_meta {
            self.backing
                .pwrite_all(&self.checksums, crate::superblock::CHECKSUM_TABLE_OFF)?;
            self.backing
                .pwrite_all(&encode_regions(&self.regions)?, REGION_TABLE_OFF)?;
            self.backing.pwrite_all(&self.sb.encode(), 0)?;
            self.dirty_meta = false;
        }
        self.backing.sync()
    }

    /// Write back and drop every resident frame. Afterwards no arena bytes
    /// are in RAM; the next read of any block is a miss served from the
    /// backing store. Used by the proof and available to callers who want a
    /// cold hot-tier.
    pub fn evict_all(&mut self) -> Result<()> {
        let all: Vec<(u32, crate::frames::FrameMeta)> = self.frames.resident().collect();
        for (f, m) in all {
            if m.dirty {
                self.writeback(f)?;
            }
            self.map[m.block as usize] = NO_FRAME;
            self.frames.release(f);
            self.stats.evictions += 1;
        }
        Ok(())
    }

    /// Proof hook: overwrite every free frame with `byte`. After `evict_all`
    /// this destroys any stale copy of arena data in RAM, so a subsequent
    /// correct read proves the bytes came from the backing store.
    pub fn debug_scribble_free_frames(&mut self, byte: u8) -> usize {
        self.frames.scribble_free(byte)
    }

    /// Sync and close. Prefer this over `drop` so errors are seen.
    pub fn close(mut self) -> Result<()> {
        self.sync()?;
        self.dirty_meta = false;
        // Drop runs after; it will find nothing dirty.
        Ok(())
    }
}

impl Drop for Loom {
    fn drop(&mut self) {
        // A Drop cannot return an error, but it must not swallow one either.
        if self.frames.dirty_count() > 0 || self.dirty_meta {
            if let Err(e) = self.sync() {
                eprintln!(
                    "loom: sync on drop FAILED for {}: {e} — unsynced data may be lost; call close() to handle this",
                    self.backing.path()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("loom-arena-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    const BS: u32 = 4096 * 4; // 16 KiB blocks keep tests fast

    fn create(p: &Path, cap: u64, budget: u64) -> Loom {
        Loom::create(
            p,
            CreateOptions {
                capacity: cap,
                block_size: BS,
                budget,
            },
        )
        .unwrap()
    }

    #[test]
    fn write_read_roundtrip_with_eviction_under_budget() {
        let p = tmp("rt");
        // 64 blocks arena, 4 frames budget: everything must cycle through disk.
        let mut l = create(&p, 64 * BS as u64, 4 * BS as u64);
        let r = l.alloc(60 * BS as u64 + 100).unwrap();
        let mut buf = vec![0u8; BS as usize];
        for b in 0..60u64 {
            pattern::fill(1, b'A', b, &mut buf);
            l.write(r, b * BS as u64, &buf).unwrap();
        }
        assert!(l.resident_blocks() <= 4);
        assert!(l.stats().evictions >= 56, "{}", l.stats().render());
        l.evict_all().unwrap();
        assert_eq!(l.resident_blocks(), 0);
        l.debug_scribble_free_frames(0xEE);
        let mut got = vec![0u8; BS as usize];
        for b in 0..60u64 {
            pattern::fill(1, b'A', b, &mut buf);
            l.read(r, b * BS as u64, &mut got).unwrap();
            assert_eq!(pattern::first_mismatch(&buf, &got), None, "block {b}");
        }
        // Tail 100 bytes never written -> zeros.
        let mut tail = vec![1u8; 100];
        l.read(r, 60 * BS as u64, &mut tail).unwrap();
        assert!(tail.iter().all(|&x| x == 0));
        l.close().unwrap();
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn unaligned_writes_span_blocks_and_persist_across_reopen() {
        let p = tmp("span");
        {
            let mut l = create(&p, 16 * BS as u64, 2 * BS as u64);
            let r = l.alloc(10 * BS as u64).unwrap();
            let data: Vec<u8> = (0..(3 * BS as usize + 777))
                .map(|i| (i * 7) as u8)
                .collect();
            l.write(r, BS as u64 / 2 + 13, &data).unwrap();
            l.close().unwrap();
        }
        let mut l = Loom::open(
            &p,
            OpenOptions {
                budget: Some(BS as u64),
                prefetch_depth: None,
            },
        )
        .unwrap();
        let regs = l.regions();
        assert_eq!(regs.len(), 1);
        let r = regs[0];
        let data: Vec<u8> = (0..(3 * BS as usize + 777))
            .map(|i| (i * 7) as u8)
            .collect();
        let mut got = vec![0u8; data.len()];
        l.read(r, BS as u64 / 2 + 13, &mut got).unwrap();
        assert_eq!(pattern::first_mismatch(&data, &got), None);
        // Byte just before the write is still zero.
        let mut one = [9u8; 1];
        l.read(r, BS as u64 / 2 + 12, &mut one).unwrap();
        assert_eq!(one[0], 0);
        l.close().unwrap();
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn corruption_is_detected_and_restoring_clears_it() {
        let p = tmp("corrupt");
        let (r, off) = {
            let mut l = create(&p, 8 * BS as u64, 2 * BS as u64);
            let r = l.alloc(4 * BS as u64).unwrap();
            let buf = vec![0x5Au8; BS as usize];
            l.write(r, 2 * BS as u64, &buf).unwrap();
            let off = l.backing_offset_of_block(l.block_of_region_offset(r, 2 * BS as u64));
            l.close().unwrap();
            (r, off)
        };
        // Flip one byte on disk at unchanged size.
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&p)
                .unwrap();
            f.seek(SeekFrom::Start(off + 100)).unwrap();
            let mut b = [0u8; 1];
            f.read_exact(&mut b).unwrap();
            assert_eq!(b[0], 0x5A);
            f.seek(SeekFrom::Start(off + 100)).unwrap();
            f.write_all(&[0x5B]).unwrap();
            f.sync_all().unwrap();
        }
        {
            let mut l = Loom::open(&p, OpenOptions::default()).unwrap();
            let mut got = vec![0u8; BS as usize];
            let e = l.read(r, 2 * BS as u64, &mut got).expect_err("must detect");
            match e {
                LoomError::Corrupt { block, .. } => assert_eq!(block, 2),
                other => panic!("wrong error: {other}"),
            }
            // Other blocks still readable.
            l.read(r, 0, &mut got).unwrap();
            assert!(got.iter().all(|&x| x == 0));
            l.close().unwrap();
        }
        // Restore the byte: the false-positive check.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.seek(SeekFrom::Start(off + 100)).unwrap();
            f.write_all(&[0x5A]).unwrap();
            f.sync_all().unwrap();
        }
        let mut l = Loom::open(&p, OpenOptions::default()).unwrap();
        let mut got = vec![0u8; BS as usize];
        l.read(r, 2 * BS as u64, &mut got).unwrap();
        assert!(got.iter().all(|&x| x == 0x5A));
        l.close().unwrap();
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn bounds_and_capacity_errors() {
        let p = tmp("bounds");
        let mut l = create(&p, 4 * BS as u64, BS as u64);
        let r = l.alloc(2 * BS as u64).unwrap();
        let mut b = [0u8; 8];
        assert!(matches!(
            l.read(r, 2 * BS as u64 - 4, &mut b),
            Err(LoomError::OutOfBounds { .. })
        ));
        assert!(matches!(
            l.alloc(3 * BS as u64),
            Err(LoomError::Capacity { .. })
        ));
        let r2 = l.alloc(2 * BS as u64).unwrap();
        assert_eq!(r2.offset, 2 * BS as u64);
        assert!(matches!(l.alloc(1), Err(LoomError::Capacity { .. })));
        l.close().unwrap();
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn hot_bytes_never_exceed_budget() {
        let p = tmp("budget");
        let l = create(&p, 1024 * BS as u64, 7 * BS as u64 + 123);
        assert_eq!(l.frame_count(), 7);
        assert_eq!(l.info().unwrap().hot_bytes_allocated, 7 * BS as u64);
        drop(l);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn zero_copy_agrees_with_copying_read_and_persists() {
        let p = tmp("zerocopy");
        let expect: Vec<u8> = (0..BS as usize).map(|i| (i * 31 % 251) as u8).collect();
        {
            let mut l = create(&p, 32 * BS as u64, 4 * BS as u64);
            let r = l.alloc(16 * BS as u64).unwrap();
            // Write via the zero-copy path.
            let n = l
                .with_slice_mut(r, 5 * BS as u64, BS as usize, |dst| {
                    dst.copy_from_slice(&expect);
                    dst.len()
                })
                .unwrap();
            assert_eq!(n, BS as usize, "whole block should be contiguous");
            // Force it out of RAM entirely, then read it back both ways.
            l.evict_all().unwrap();
            l.debug_scribble_free_frames(0x77);
            let mut copied = vec![0u8; BS as usize];
            l.read(r, 5 * BS as u64, &mut copied).unwrap();
            assert_eq!(pattern::first_mismatch(&expect, &copied), None);
            l.evict_all().unwrap();
            l.debug_scribble_free_frames(0x88);
            let borrowed = l
                .with_slice(r, 5 * BS as u64, BS as usize, |src| src.to_vec())
                .unwrap();
            assert_eq!(pattern::first_mismatch(&expect, &borrowed), None);
            assert!(l.stats().zero_copy >= 2);
            l.close().unwrap();
        }
        // And across a reopen, so the dirty flag really was set.
        let mut l = Loom::open(&p, OpenOptions::default()).unwrap();
        let r = l.regions()[0];
        let got = l
            .with_slice(r, 5 * BS as u64, BS as usize, |src| src.to_vec())
            .unwrap();
        assert_eq!(pattern::first_mismatch(&expect, &got), None);
        l.close().unwrap();
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn zero_copy_stops_at_the_block_boundary() {
        let p = tmp("zerocopy-bound");
        let mut l = create(&p, 8 * BS as u64, 2 * BS as u64);
        let r = l.alloc(4 * BS as u64).unwrap();
        // Ask for two blocks' worth starting 100 bytes in; must be clamped.
        let got = l.with_slice(r, 100, 2 * BS as usize, |s| s.len()).unwrap();
        assert_eq!(got, BS as usize - 100);
        l.close().unwrap();
        std::fs::remove_file(&p).unwrap();
    }

    /// The dangerous case. A dirty frame is the ONLY copy of those bytes;
    /// a speculative read that overwrote it would destroy data silently.
    #[test]
    fn prefetch_never_clobbers_a_dirty_frame() {
        let p = tmp("prefetch-dirty");
        let precious = vec![0xABu8; BS as usize];
        let mut l = Loom::create(
            &p,
            CreateOptions {
                capacity: 256 * BS as u64,
                block_size: BS,
                budget: 32 * BS as u64,
            },
        )
        .unwrap();
        let r = l.alloc(200 * BS as u64).unwrap();
        // Lay down a pattern everywhere so reads have something to verify.
        let mut scratch = vec![0u8; BS as usize];
        for b in 0..200u64 {
            pattern::fill(9, b'P', b, &mut scratch);
            l.write(r, b * BS as u64, &scratch).unwrap();
        }
        l.sync().unwrap();
        l.evict_all().unwrap();

        // Dirty exactly one block, in the middle of what we are about to scan.
        l.write(r, 60 * BS as u64, &precious).unwrap();
        assert_eq!(l.dirty_blocks(), 1);

        // Now scan sequentially straight through it. This is the pattern
        // that triggers prefetch batches covering block 60.
        for b in 50..80u64 {
            pattern::fill(9, b'P', b, &mut scratch);
            let mut got = vec![0u8; BS as usize];
            l.read(r, b * BS as u64, &mut got).unwrap();
            if b == 60 {
                // Still the dirty bytes, NOT what is on disk.
                assert_eq!(
                    pattern::first_mismatch(&precious, &got),
                    None,
                    "prefetch clobbered a dirty frame: block 60 lost its unflushed bytes"
                );
            } else {
                assert_eq!(pattern::first_mismatch(&scratch, &got), None, "block {b}");
            }
        }
        assert!(
            l.stats().prefetch_batches > 0,
            "sequential scan should have triggered prefetch; counters: {}",
            l.stats().render()
        );
        // And the dirty bytes survive a real round trip.
        l.sync().unwrap();
        l.evict_all().unwrap();
        let mut got = vec![0u8; BS as usize];
        l.read(r, 60 * BS as u64, &mut got).unwrap();
        assert_eq!(pattern::first_mismatch(&precious, &got), None);
        l.close().unwrap();
        std::fs::remove_file(&p).unwrap();
    }

    /// Prefetch must cut the number of device round-trips on a sequential
    /// scan, and must be switchable off so the claim is measurable.
    #[test]
    fn prefetch_cuts_device_round_trips_and_can_be_disabled() {
        let p = tmp("prefetch-count");
        {
            let mut l = Loom::create(
                &p,
                CreateOptions {
                    capacity: 512 * BS as u64,
                    block_size: BS,
                    budget: 64 * BS as u64,
                },
            )
            .unwrap();
            let r = l.alloc(400 * BS as u64).unwrap();
            let mut scratch = vec![0u8; BS as usize];
            for b in 0..400u64 {
                pattern::fill(3, b'Q', b, &mut scratch);
                l.write(r, b * BS as u64, &scratch).unwrap();
            }
            l.close().unwrap();
        }
        let scan = |depth: Option<usize>| -> (u64, u64, u64) {
            let mut l = Loom::open(
                &p,
                OpenOptions {
                    budget: Some(64 * BS as u64),
                    prefetch_depth: depth,
                },
            )
            .unwrap();
            let r = l.regions()[0];
            let mut scratch = vec![0u8; BS as usize];
            let mut got = vec![0u8; BS as usize];
            for b in 0..400u64 {
                pattern::fill(3, b'Q', b, &mut scratch);
                l.read(r, b * BS as u64, &mut got).unwrap();
                assert_eq!(pattern::first_mismatch(&scratch, &got), None, "block {b}");
            }
            let st = l.stats().clone();
            l.close().unwrap();
            (st.prefetch_batches, st.prefetch_blocks, st.prefetch_used)
        };

        let (off_batches, _, _) = scan(Some(0));
        assert_eq!(off_batches, 0, "prefetch_depth 0 must issue no batches");

        let (batches, blocks, used) = scan(None);
        assert!(
            batches > 0,
            "default prefetch should batch a sequential scan"
        );
        assert!(blocks > 0);
        // A pure forward scan is the best case: almost every speculative
        // block should be asked for. Assert most, not all — the first two
        // blocks precede run detection and the tail is clamped.
        assert!(
            used * 10 >= blocks * 8,
            "prefetch was mostly wasted: {used} of {blocks} used"
        );
        // 400 blocks in `batches` reads is the whole point.
        assert!(
            batches < 200,
            "expected far fewer batches than blocks, got {batches}"
        );
        std::fs::remove_file(&p).unwrap();
    }

    /// A speculative read must not report corruption the caller never asked
    /// about — but the moment they DO ask, it must be reported.
    #[test]
    fn prefetch_defers_corruption_until_the_block_is_asked_for() {
        let p = tmp("prefetch-corrupt");
        let bad_block;
        {
            let mut l = Loom::create(
                &p,
                CreateOptions {
                    capacity: 128 * BS as u64,
                    block_size: BS,
                    budget: 16 * BS as u64,
                },
            )
            .unwrap();
            let r = l.alloc(100 * BS as u64).unwrap();
            let mut scratch = vec![0u8; BS as usize];
            for b in 0..100u64 {
                pattern::fill(5, b'R', b, &mut scratch);
                l.write(r, b * BS as u64, &scratch).unwrap();
            }
            bad_block = l.block_of_region_offset(r, 40 * BS as u64);
            let off = l.backing_offset_of_block(bad_block);
            l.close().unwrap();
            use std::io::{Seek, SeekFrom, Write as _};
            let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.seek(SeekFrom::Start(off + 77)).unwrap();
            f.write_all(&[0xFF]).unwrap();
            f.sync_all().unwrap();
        }
        let mut l = Loom::open(&p, OpenOptions::default()).unwrap();
        let r = l.regions()[0];
        let mut got = vec![0u8; BS as usize];
        // Scan up to (not into) the bad block — prefetch will have spanned
        // it, and must not have failed the run.
        for b in 30..40u64 {
            l.read(r, b * BS as u64, &mut got).unwrap();
        }
        // Now ask for it directly. Now it must be reported.
        match l.read(r, 40 * BS as u64, &mut got) {
            Err(LoomError::Corrupt { block, .. }) => assert_eq!(block, bad_block),
            other => panic!("corruption not reported when asked for: {other:?}"),
        }
        l.close().unwrap();
        std::fs::remove_file(&p).unwrap();
    }
}
