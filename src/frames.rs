//! The hot tier.
//!
//! A fixed number of page-aligned frames, allocated once at open and never
//! grown. `frame_count * block_size` *is* the hot budget: this is the only
//! place arena bytes live in RAM, so the bound on Loom's data footprint is
//! arithmetic on this allocation rather than a hope about the kernel.
//!
//! Residency policy is CLOCK (second-chance): cheap, deterministic, and good
//! enough to prove the primitive. Replacing it later touches this module
//! only.

use crate::aligned::AlignedBuf;

pub const NO_BLOCK: u64 = u64::MAX;

#[derive(Clone, Copy, Debug)]
pub struct FrameMeta {
    /// Logical block held, or NO_BLOCK if free.
    pub block: u64,
    pub dirty: bool,
    /// CLOCK reference bit; set on every access, cleared as the hand passes.
    pub referenced: bool,
}

pub struct FramePool {
    buf: AlignedBuf,
    block_size: usize,
    meta: Vec<FrameMeta>,
    free: Vec<u32>,
    hand: usize,
}

impl FramePool {
    pub fn new(frame_count: usize, block_size: usize) -> Self {
        assert!(frame_count > 0, "FramePool: zero frames");
        let buf = AlignedBuf::zeroed(frame_count * block_size);
        let meta = vec![
            FrameMeta {
                block: NO_BLOCK,
                dirty: false,
                referenced: false,
            };
            frame_count
        ];
        // Hand out low frames first so a partially used pool is contiguous.
        let free = (0..frame_count as u32).rev().collect();
        FramePool {
            buf,
            block_size,
            meta,
            free,
            hand: 0,
        }
    }

    pub fn frame_count(&self) -> usize {
        self.meta.len()
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Bytes of frame memory allocated — the hot budget as actually held.
    pub fn bytes(&self) -> usize {
        self.buf.len()
    }

    pub fn resident_count(&self) -> usize {
        self.meta.len() - self.free.len()
    }

    pub fn dirty_count(&self) -> usize {
        self.meta.iter().filter(|m| m.dirty).count()
    }

    pub fn meta(&self, frame: u32) -> FrameMeta {
        self.meta[frame as usize]
    }

    pub fn data(&self, frame: u32) -> &[u8] {
        let s = frame as usize * self.block_size;
        &self.buf[s..s + self.block_size]
    }

    pub fn data_mut(&mut self, frame: u32) -> &mut [u8] {
        let s = frame as usize * self.block_size;
        &mut self.buf[s..s + self.block_size]
    }

    pub fn touch(&mut self, frame: u32) {
        self.meta[frame as usize].referenced = true;
    }

    pub fn mark_dirty(&mut self, frame: u32) {
        self.meta[frame as usize].dirty = true;
    }

    pub fn mark_clean(&mut self, frame: u32) {
        self.meta[frame as usize].dirty = false;
    }

    /// Take a free frame if one exists.
    pub fn take_free(&mut self) -> Option<u32> {
        self.free.pop()
    }

    /// CLOCK victim selection. Returns a frame that is currently holding a
    /// block; the caller must write it back if dirty and update the block
    /// map before calling `assign`. Panics if the pool has free frames (the
    /// caller must prefer `take_free`) or is empty of residents.
    pub fn choose_victim(&mut self) -> u32 {
        assert!(
            self.free.is_empty(),
            "choose_victim with free frames available"
        );
        let n = self.meta.len();
        // At most two full sweeps: the first clears every reference bit, the
        // second is guaranteed to find a victim.
        for _ in 0..(2 * n + 1) {
            let i = self.hand;
            self.hand = (self.hand + 1) % n;
            let m = &mut self.meta[i];
            if m.block == NO_BLOCK {
                continue;
            }
            if m.referenced {
                m.referenced = false;
            } else {
                return i as u32;
            }
        }
        unreachable!("CLOCK sweep found no victim in a full pool");
    }

    /// Bind a frame to a block. The frame's contents are whatever the caller
    /// put there; `dirty` says whether they differ from the backing store.
    pub fn assign(&mut self, frame: u32, block: u64, dirty: bool) {
        self.meta[frame as usize] = FrameMeta {
            block,
            dirty,
            referenced: true,
        };
    }

    /// Unbind a frame and return it to the free list. Contents are left in
    /// place (they are garbage from the pool's point of view).
    pub fn release(&mut self, frame: u32) {
        let m = &mut self.meta[frame as usize];
        debug_assert!(!m.dirty, "release of a dirty frame loses data");
        m.block = NO_BLOCK;
        m.dirty = false;
        m.referenced = false;
        self.free.push(frame);
    }

    /// Iterate over resident frames.
    pub fn resident(&self) -> impl Iterator<Item = (u32, FrameMeta)> + '_ {
        self.meta
            .iter()
            .enumerate()
            .filter(|(_, m)| m.block != NO_BLOCK)
            .map(|(i, m)| (i as u32, *m))
    }

    /// Overwrite every *free* frame with a byte pattern. Used by the proof to
    /// show reads after eviction come from the backing store, not from stale
    /// frame memory. Resident frames are untouched.
    pub fn scribble_free(&mut self, byte: u8) -> usize {
        let free: Vec<u32> = self.free.clone();
        for &f in &free {
            self.data_mut(f).fill(byte);
        }
        free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_then_clock() {
        let mut p = FramePool::new(3, 4096);
        assert_eq!(p.bytes(), 3 * 4096);
        let a = p.take_free().unwrap();
        let b = p.take_free().unwrap();
        let c = p.take_free().unwrap();
        assert!(p.take_free().is_none());
        p.assign(a, 10, false);
        p.assign(b, 11, false);
        p.assign(c, 12, false);
        // All referenced: first sweep clears, second sweep evicts a (hand at 0).
        assert_eq!(p.choose_victim(), a);
        // Touch b so it survives; next victim should be c... hand now at 1
        // (after returning a at index 0, hand advanced to 1). b referenced?
        // It was cleared in the first sweep. Re-touch it:
        p.touch(b);
        assert_eq!(p.choose_victim(), c);
        p.mark_clean(a);
        p.release(a);
        assert_eq!(p.resident_count(), 2);
        assert_eq!(p.take_free(), Some(a));
    }

    #[test]
    fn scribble_only_touches_free_frames() {
        let mut p = FramePool::new(2, 4096);
        let a = p.take_free().unwrap();
        p.assign(a, 5, false);
        p.data_mut(a).fill(0xAA);
        let n = p.scribble_free(0x55);
        assert_eq!(n, 1);
        assert!(p.data(a).iter().all(|&x| x == 0xAA));
        let other = if a == 0 { 1 } else { 0 };
        assert!(p.data(other).iter().all(|&x| x == 0x55));
    }
}
