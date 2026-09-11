//! Page-aligned heap buffers.
//!
//! Every byte Loom moves to or from the backing store goes through one of
//! these. Cache-bypassing I/O (`F_NOCACHE` on macOS, `O_DIRECT` on Linux)
//! is only honoured for aligned buffers, offsets and lengths; an unaligned
//! buffer would silently fall back to the kernel page cache and make the
//! hot-budget accounting a lie. So alignment is enforced by the type, not by
//! convention.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ops::{Deref, DerefMut};

/// The I/O alignment Loom assumes. 4 KiB satisfies APFS, ext4 and xfs on
/// every drive we care about (512e and 4Kn alike).
pub const IO_ALIGN: usize = 4096;

pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the buffer is a plain byte allocation with no interior pointers;
// ownership is unique (no aliasing handles are ever created), so moving it
// across threads is sound.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Allocate `len` zeroed bytes aligned to [`IO_ALIGN`]. `len` must be a
    /// non-zero multiple of the alignment.
    pub fn zeroed(len: usize) -> Self {
        assert!(len > 0, "AlignedBuf: zero length");
        assert!(
            len % IO_ALIGN == 0,
            "AlignedBuf: len {len} not a multiple of {IO_ALIGN}"
        );
        let layout = Layout::from_size_align(len, IO_ALIGN).expect("valid layout");
        // SAFETY: layout has non-zero size (asserted above) and a valid
        // power-of-two alignment. alloc_zeroed returns null only on OOM,
        // which we turn into a panic rather than a dangling pointer.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        AlignedBuf { ptr, len }
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr is valid for len bytes for the lifetime of self and
        // was initialised (zeroed) at allocation.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above; &mut self guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, IO_ALIGN).expect("valid layout");
        // SAFETY: ptr was returned by alloc_zeroed with exactly this layout
        // and has not been freed.
        unsafe { dealloc(self.ptr, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_and_zeroed() {
        let b = AlignedBuf::zeroed(IO_ALIGN * 3);
        assert_eq!(b.as_ptr() as usize % IO_ALIGN, 0);
        assert!(b.iter().all(|&x| x == 0));
        assert_eq!(b.len(), IO_ALIGN * 3);
    }

    #[test]
    #[should_panic]
    fn rejects_unaligned_len() {
        let _ = AlignedBuf::zeroed(IO_ALIGN + 1);
    }
}
