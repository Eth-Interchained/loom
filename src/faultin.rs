//! Transparent fault-in: a real pointer into an arena larger than RAM.
//!
//! This is the mechanism that turns Loom from a library you call into memory
//! your program just *has*. The application receives a `*mut u8` spanning
//! the whole arena and dereferences it normally. Loom runs only on faults.
//!
//! ```text
//!   app thread                    handler thread
//!   ----------                    --------------
//!   p[93_000_000_000] = 'x'
//!     │ page not present
//!     ▼
//!   kernel blocks the thread ───▶ read(uffd) -> pagefault @addr
//!                                   │ load the extent from the pool
//!                                   │ UFFDIO_COPY into place
//!   ◀─────────── resumes ──────────┘
//!   (every later touch of that page costs NOTHING — no Loom code runs)
//! ```
//!
//! **The property that matters:** once a page is mapped, a hit is free. Not
//! 197 ns of bookkeeping, not a 41 µs memcpy — zero. The CPU reads DRAM
//! directly. Loom is only in the path for faults.
//!
//! # Mechanism
//!
//! Linux `userfaultfd`, registered `MISSING | WP`:
//! - **MISSING** fault: the page has no backing yet. Load the containing
//!   extent from the pool and `UFFDIO_COPY` it in, write-protected.
//! - **WP** fault: the page is present but write-protected, so this access
//!   is a *write*. Mark the extent dirty and lift the protection. This is
//!   exact dirty tracking — no guessing, no assuming every touched page was
//!   written.
//! - **Eviction**: copy the bytes out, write back if dirty, then
//!   `madvise(MADV_DONTNEED)`. The physical page is released and the next
//!   touch faults MISSING again.
//!
//! macOS has no `userfaultfd`; the equivalent is a Mach exception port for
//! `EXC_BAD_ACCESS` handled on a dedicated thread. Same architecture, same
//! state machine — only the fault transport differs. This module proves the
//! architecture on a platform where it can actually be executed.
//!
//! # What this is not
//!
//! - Not system-wide. It covers one arena in one process.
//! - Not a way to make the OS report more RAM. See README.
//! - Fault granularity is a page (4 KiB) but the **load** unit is an extent
//!   (default 64 KiB), because a 4 KiB round trip to a 10 ms device is
//!   ruinous. Extent size is the single most important tuning knob here.

#![cfg(target_os = "linux")]

use crate::arena::{Loom, Region};
use crate::error::{LoomError, Result};
use crate::stats::LatencyHist;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ---------------------------------------------------------------------------
// userfaultfd ABI.
//
// These structs and ioctl numbers are the kernel's stable ABI. The ioctl
// numbers are COMPUTED rather than transcribed: a hand-copied constant is
// exactly the kind of thing that compiles cleanly and is silently wrong.
// ---------------------------------------------------------------------------

const UFFD_API: u64 = 0xAA;
const UFFDIO: u32 = 0xAA;

const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;

const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 0;
const UFFD_PAGEFAULT_FLAG_WP: u64 = 1 << 1;

const UFFDIO_COPY_MODE_WP: u64 = 1 << 1;

const UFFD_FEATURE_PAGEFAULT_FLAG_WP: u64 = 1 << 4;

/// `_IOC(dir, type, nr, size)` with dir = READ|WRITE.
const fn iowr(ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    const NRBITS: u32 = 8;
    const TYPEBITS: u32 = 8;
    const SIZEBITS: u32 = 14;
    const DIR_RW: u32 = 3;
    ((DIR_RW << (NRBITS + TYPEBITS + SIZEBITS))
        | (size << (NRBITS + TYPEBITS))
        | (ty << NRBITS)
        | nr) as libc::c_ulong
}

#[repr(C)]
#[derive(Default)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
#[derive(Default)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
#[derive(Default)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

#[repr(C)]
#[derive(Default)]
struct UffdioWriteprotect {
    range: UffdioRange,
    mode: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct UffdMsg {
    event: u8,
    _r1: u8,
    _r2: u16,
    _r3: u32,
    // union arg; only the pagefault variant is read.
    flags: u64,
    address: u64,
    _feat: u64,
}

const fn uffdio_api_no() -> libc::c_ulong {
    iowr(UFFDIO, 0x3F, std::mem::size_of::<UffdioApi>() as u32)
}
const fn uffdio_register_no() -> libc::c_ulong {
    iowr(UFFDIO, 0x00, std::mem::size_of::<UffdioRegister>() as u32)
}
const fn uffdio_copy_no() -> libc::c_ulong {
    iowr(UFFDIO, 0x03, std::mem::size_of::<UffdioCopy>() as u32)
}
const fn uffdio_writeprotect_no() -> libc::c_ulong {
    iowr(
        UFFDIO,
        0x06,
        std::mem::size_of::<UffdioWriteprotect>() as u32,
    )
}

// ---------------------------------------------------------------------------
// Configuration and stats
// ---------------------------------------------------------------------------

/// Bytes loaded per fault. A page is 4 KiB but a 4 KiB round trip to a
/// 10 ms device is ruinous; one fault should pay for a useful span.
pub const DEFAULT_EXTENT: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct FaultOptions {
    /// Bytes of the arena the program may address.
    pub len: usize,
    /// Bytes loaded and mapped per fault.
    pub extent: usize,
    /// Hard cap on mapped arena bytes. This is the budget, and it is
    /// enforced by counting extents we mapped — not hoped for.
    pub resident_budget: usize,
}

impl FaultOptions {
    pub fn new(len: usize, resident_budget: usize) -> Self {
        FaultOptions {
            len,
            extent: DEFAULT_EXTENT,
            resident_budget,
        }
    }
}

/// Everything the fault path did. Counters only; no inference.
#[derive(Default)]
pub struct FaultStats {
    /// MISSING faults — the page had no backing and had to be loaded.
    pub load_faults: u64,
    /// WP faults — the page was present and this access was a write.
    pub write_faults: u64,
    /// Extents evicted to stay inside the budget.
    pub evictions: u64,
    /// Evictions that had to write back because the extent was dirty.
    pub writebacks: u64,
    /// Extents currently mapped.
    pub resident_extents: u64,
    /// Peak mapped bytes observed. The witness for the budget.
    pub peak_resident_bytes: u64,
    pub load_latency: LatencyHist,
    pub wp_latency: LatencyHist,
    /// A fault the handler could not resolve. Never silent: the handler
    /// records the address and the reason, and the faulting thread is left
    /// blocked rather than resumed onto wrong bytes.
    pub failures: Vec<String>,
}

impl FaultStats {
    pub fn render(&self, extent: usize) -> String {
        format!(
            "load faults: {}  write faults: {}  evictions: {} ({} needed writeback)\n  \
             resident: {} extents ({})  peak: {}\n  \
             load latency: {}\n  wp   latency: {}{}",
            self.load_faults,
            self.write_faults,
            self.evictions,
            self.writebacks,
            self.resident_extents,
            crate::stats::fmt_bytes(self.resident_extents * extent as u64),
            crate::stats::fmt_bytes(self.peak_resident_bytes),
            self.load_latency.summary(),
            self.wp_latency.summary(),
            if self.failures.is_empty() {
                String::new()
            } else {
                format!(
                    "\n  FAILURES ({}): {:?}",
                    self.failures.len(),
                    self.failures
                )
            }
        )
    }
}

// ---------------------------------------------------------------------------
// Residency bookkeeping
// ---------------------------------------------------------------------------

struct Residency {
    /// Mapped extents in arrival order. FIFO eviction: simple, and honest
    /// about being simple. CLOCK belongs here once there is a workload to
    /// tune against.
    order: VecDeque<usize>,
    /// Per-extent: mapped, and dirty.
    mapped: Vec<bool>,
    dirty: Vec<bool>,
}

struct Shared {
    base: usize,
    len: usize,
    extent: usize,
    max_extents: usize,
    uffd: libc::c_int,
    loom: Mutex<Loom>,
    region: Region,
    res: Mutex<Residency>,
    stats: Mutex<FaultStats>,
    stop: AtomicBool,
    /// Bumped every time the handler finishes resolving a fault, so a test
    /// can wait for progress without sleeping blindly.
    resolved: AtomicU64,
}

// SAFETY: every field is either atomic, behind a Mutex, or immutable after
// construction. `base` is a raw address used only for arithmetic and for
// ioctl arguments; the pages it names are owned by this arena for its
// lifetime.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// A transparent arena. Drop unregisters, stops the handler and unmaps.
pub struct FaultArena {
    shared: Arc<Shared>,
    handler: Option<std::thread::JoinHandle<()>>,
}

impl FaultArena {
    /// Reserve `opts.len` bytes of address space backed by `region` of
    /// `loom`, and start the fault handler.
    ///
    /// The returned pointer is valid for `opts.len` bytes and may be
    /// dereferenced from any thread. Reads return the region's bytes;
    /// writes are tracked and written back on eviction.
    pub fn new(loom: Loom, region: Region, opts: FaultOptions) -> Result<Self> {
        let page = page_size();
        if opts.extent == 0 || opts.extent % page != 0 {
            return Err(LoomError::Invalid(format!(
                "extent {} must be a non-zero multiple of the page size {}",
                opts.extent, page
            )));
        }
        if opts.len == 0 || opts.len % opts.extent != 0 {
            return Err(LoomError::Invalid(format!(
                "len {} must be a non-zero multiple of extent {}",
                opts.len, opts.extent
            )));
        }
        if opts.len as u64 > region.len {
            return Err(LoomError::Invalid(format!(
                "len {} exceeds region length {}",
                opts.len, region.len
            )));
        }
        let max_extents = opts.resident_budget / opts.extent;
        if max_extents == 0 {
            return Err(LoomError::Invalid(format!(
                "resident budget {} is smaller than one extent ({})",
                opts.resident_budget, opts.extent
            )));
        }

        // 1. userfaultfd, with write-protect support demanded explicitly.
        // SAFETY: a plain syscall with no pointer arguments.
        let uffd = unsafe {
            libc::syscall(
                libc::SYS_userfaultfd,
                libc::O_CLOEXEC | libc::O_NONBLOCK as libc::c_int,
            )
        } as libc::c_int;
        if uffd < 0 {
            return Err(crate::error::io_err(
                "userfaultfd",
                "kernel may require CAP_SYS_PTRACE; see vm.unprivileged_userfaultfd",
            ));
        }
        let mut api = UffdioApi {
            api: UFFD_API,
            features: UFFD_FEATURE_PAGEFAULT_FLAG_WP,
            ioctls: 0,
        };
        // SAFETY: uffd is a valid fd we just created; `api` is a correctly
        // sized, correctly laid out argument for this ioctl.
        if unsafe { libc::ioctl(uffd, uffdio_api_no(), &mut api) } < 0 {
            let e = crate::error::io_err(
                "ioctl(UFFDIO_API)",
                "write-protect mode unavailable on this kernel",
            );
            // SAFETY: fd is ours and still open.
            unsafe { libc::close(uffd) };
            return Err(e);
        }

        // 2. Reserve the address space. Anonymous and unpopulated: every
        //    first touch is a MISSING fault.
        // SAFETY: null hint with a non-zero length; the kernel chooses the
        // address. MAP_NORESERVE keeps a huge reservation from charging
        // commit.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                opts.len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            let e = crate::error::io_err("mmap", format!("{} bytes", opts.len));
            // SAFETY: fd is ours and still open.
            unsafe { libc::close(uffd) };
            return Err(e);
        }

        // 3. Register for both missing and write-protect faults.
        let mut reg = UffdioRegister {
            range: UffdioRange {
                start: base as u64,
                len: opts.len as u64,
            },
            mode: UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_WP,
            ioctls: 0,
        };
        // SAFETY: uffd valid; the range is exactly the mapping we just made.
        if unsafe { libc::ioctl(uffd, uffdio_register_no(), &mut reg) } < 0 {
            let e = crate::error::io_err("ioctl(UFFDIO_REGISTER)", "MISSING|WP");
            // SAFETY: both resources are ours and still live.
            unsafe {
                libc::munmap(base, opts.len);
                libc::close(uffd);
            }
            return Err(e);
        }

        let n_ext = opts.len / opts.extent;
        let shared = Arc::new(Shared {
            base: base as usize,
            len: opts.len,
            extent: opts.extent,
            max_extents,
            uffd,
            loom: Mutex::new(loom),
            region,
            res: Mutex::new(Residency {
                order: VecDeque::with_capacity(max_extents + 1),
                mapped: vec![false; n_ext],
                dirty: vec![false; n_ext],
            }),
            stats: Mutex::new(FaultStats::default()),
            stop: AtomicBool::new(false),
            resolved: AtomicU64::new(0),
        });

        let h = {
            let s = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("loom-faultin".into())
                .spawn(move || handler_loop(s))
                .map_err(|e| LoomError::Io {
                    op: "spawn(handler)",
                    ctx: "loom-faultin".into(),
                    source: e,
                })?
        };

        Ok(FaultArena {
            shared,
            handler: Some(h),
        })
    }

    /// The base pointer. Valid for `len()` bytes.
    pub fn as_ptr(&self) -> *mut u8 {
        self.shared.base as *mut u8
    }

    /// Addressable bytes. Never zero — `new` rejects a zero length.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.shared.len
    }

    pub fn extent(&self) -> usize {
        self.shared.extent
    }

    /// Mapped bytes right now — the enforced budget, counted from what the
    /// handler actually mapped.
    pub fn resident_bytes(&self) -> usize {
        self.shared.res.lock().unwrap().order.len() * self.shared.extent
    }

    pub fn budget_bytes(&self) -> usize {
        self.shared.max_extents * self.shared.extent
    }

    /// Snapshot of the fault counters.
    pub fn stats(&self) -> FaultStats {
        let s = self.shared.stats.lock().unwrap();
        FaultStats {
            load_faults: s.load_faults,
            write_faults: s.write_faults,
            evictions: s.evictions,
            writebacks: s.writebacks,
            resident_extents: self.shared.res.lock().unwrap().order.len() as u64,
            peak_resident_bytes: s.peak_resident_bytes,
            load_latency: s.load_latency.clone(),
            wp_latency: s.wp_latency.clone(),
            failures: s.failures.clone(),
        }
    }

    /// Write every dirty extent back and sync the pool. Leaves mappings in
    /// place.
    pub fn flush(&self) -> Result<()> {
        let dirty: Vec<usize> = {
            let r = self.shared.res.lock().unwrap();
            r.order.iter().copied().filter(|&e| r.dirty[e]).collect()
        };
        for ext in dirty {
            writeback_extent(&self.shared, ext)?;
            self.shared.res.lock().unwrap().dirty[ext] = false;
        }
        self.shared.loom.lock().unwrap().sync()
    }
}

impl Drop for FaultArena {
    fn drop(&mut self) {
        // Flush first: dropping must not lose writes the program made
        // through the pointer.
        if let Err(e) = self.flush() {
            eprintln!("loom: faultin flush on drop FAILED: {e} — dirty extents may be lost");
        }
        self.shared.stop.store(true, Ordering::SeqCst);
        // Unmapping while the handler may be mid-ioctl would be a
        // use-after-free of the mapping, so join first. The handler polls
        // `stop` on its read timeout, so it exits promptly.
        if let Some(h) = self.handler.take() {
            let _ = h.join();
        }
        // SAFETY: both resources were created by this arena, the handler
        // thread has exited, and no other reference to the mapping exists.
        unsafe {
            libc::munmap(self.shared.base as *mut libc::c_void, self.shared.len);
            libc::close(self.shared.uffd);
        }
    }
}

fn page_size() -> usize {
    // SAFETY: sysconf takes an int and returns a long; no pointers.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 {
        v as usize
    } else {
        4096
    }
}

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

/// Copy an extent's bytes out of the mapping and write them to the pool.
/// Called before releasing a dirty extent, and by `flush`.
fn writeback_extent(s: &Shared, ext: usize) -> Result<()> {
    let off = ext * s.extent;
    let mut buf = vec![0u8; s.extent];
    // SAFETY: [base+off, +extent) is inside the mapping and is mapped (the
    // caller only writes back extents recorded as mapped). The destination
    // is a distinct heap buffer.
    unsafe {
        std::ptr::copy_nonoverlapping((s.base + off) as *const u8, buf.as_mut_ptr(), s.extent);
    }
    let mut loom = s.loom.lock().unwrap();
    loom.write(s.region, off as u64, &buf)
}

/// Resolve a MISSING fault: load the containing extent and map it in.
fn resolve_missing(s: &Shared, ext: usize) -> Result<()> {
    let t = Instant::now();

    // Make room first, so the budget is never exceeded even transiently.
    loop {
        let victim = {
            let r = s.res.lock().unwrap();
            if r.order.len() < s.max_extents {
                None
            } else {
                r.order.front().copied()
            }
        };
        let Some(v) = victim else { break };
        let was_dirty = s.res.lock().unwrap().dirty[v];
        if was_dirty {
            writeback_extent(s, v)?;
            let mut st = s.stats.lock().unwrap();
            st.writebacks += 1;
        }
        // SAFETY: the extent is inside the mapping. MADV_DONTNEED on
        // anonymous memory drops the pages; the next touch faults MISSING
        // again, which is exactly the behaviour we want from eviction.
        let rc = unsafe {
            libc::madvise(
                (s.base + v * s.extent) as *mut libc::c_void,
                s.extent,
                libc::MADV_DONTNEED,
            )
        };
        if rc != 0 {
            return Err(crate::error::io_err(
                "madvise(MADV_DONTNEED)",
                format!("extent {v}"),
            ));
        }
        {
            let mut r = s.res.lock().unwrap();
            r.order.pop_front();
            r.mapped[v] = false;
            r.dirty[v] = false;
        }
        s.stats.lock().unwrap().evictions += 1;
    }

    // Read the extent's bytes from the pool.
    let mut buf = vec![0u8; s.extent];
    {
        let mut loom = s.loom.lock().unwrap();
        loom.read(s.region, (ext * s.extent) as u64, &mut buf)?;
    }

    // Map it in, write-protected so the first write is observable.
    let mut copy = UffdioCopy {
        dst: (s.base + ext * s.extent) as u64,
        src: buf.as_ptr() as u64,
        len: s.extent as u64,
        mode: UFFDIO_COPY_MODE_WP,
        copy: 0,
    };
    // SAFETY: uffd is valid; dst names a registered range inside our
    // mapping; src is a live heap buffer of exactly `len` bytes.
    if unsafe { libc::ioctl(s.uffd, uffdio_copy_no(), &mut copy) } < 0 {
        return Err(crate::error::io_err(
            "ioctl(UFFDIO_COPY)",
            format!("extent {ext}"),
        ));
    }

    {
        let mut r = s.res.lock().unwrap();
        r.mapped[ext] = true;
        r.dirty[ext] = false;
        r.order.push_back(ext);
        let bytes = (r.order.len() * s.extent) as u64;
        let mut st = s.stats.lock().unwrap();
        st.load_faults += 1;
        st.load_latency.record(t.elapsed());
        if bytes > st.peak_resident_bytes {
            st.peak_resident_bytes = bytes;
        }
    }
    Ok(())
}

/// Resolve a WP fault: this access is a write to a present page, so the
/// extent is now dirty. Lift the protection so the write can proceed.
fn resolve_wp(s: &Shared, ext: usize) -> Result<()> {
    let t = Instant::now();
    let mut wp = UffdioWriteprotect {
        range: UffdioRange {
            start: (s.base + ext * s.extent) as u64,
            len: s.extent as u64,
        },
        mode: 0, // clear write protection
    };
    // SAFETY: uffd valid; the range is inside the registered mapping.
    if unsafe { libc::ioctl(s.uffd, uffdio_writeprotect_no(), &mut wp) } < 0 {
        return Err(crate::error::io_err(
            "ioctl(UFFDIO_WRITEPROTECT clear)",
            format!("extent {ext}"),
        ));
    }
    s.res.lock().unwrap().dirty[ext] = true;
    let mut st = s.stats.lock().unwrap();
    st.write_faults += 1;
    st.wp_latency.record(t.elapsed());
    Ok(())
}

fn handler_loop(s: Arc<Shared>) {
    let mut pfd = libc::pollfd {
        fd: s.uffd,
        events: libc::POLLIN,
        revents: 0,
    };
    while !s.stop.load(Ordering::SeqCst) {
        // Poll with a timeout so `stop` is noticed promptly even when no
        // faults are arriving.
        // SAFETY: pfd is a valid single-element pollfd array.
        let n = unsafe { libc::poll(&mut pfd, 1, 50) };
        if n <= 0 {
            continue;
        }
        let mut msg = UffdMsg::default();
        // SAFETY: reading exactly one uffd_msg into a correctly sized,
        // correctly laid out struct from the uffd descriptor.
        let r = unsafe {
            libc::read(
                s.uffd,
                &mut msg as *mut UffdMsg as *mut libc::c_void,
                std::mem::size_of::<UffdMsg>(),
            )
        };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            s.stats
                .lock()
                .unwrap()
                .failures
                .push(format!("read(uffd) failed: {e}"));
            return;
        }
        if r as usize != std::mem::size_of::<UffdMsg>() {
            s.stats
                .lock()
                .unwrap()
                .failures
                .push(format!("short read from uffd: {r} bytes"));
            continue;
        }
        if msg.event != UFFD_EVENT_PAGEFAULT {
            // Not ours to interpret. Record it rather than ignoring it; an
            // unhandled event class is information, not noise.
            s.stats
                .lock()
                .unwrap()
                .failures
                .push(format!("unexpected uffd event {:#x}", msg.event));
            continue;
        }

        let addr = msg.address as usize;
        if addr < s.base || addr >= s.base + s.len {
            s.stats
                .lock()
                .unwrap()
                .failures
                .push(format!("fault at {addr:#x} outside arena"));
            continue;
        }
        let ext = (addr - s.base) / s.extent;
        let is_wp = msg.flags & UFFD_PAGEFAULT_FLAG_WP != 0;
        let is_write = msg.flags & UFFD_PAGEFAULT_FLAG_WRITE != 0;

        let outcome = if is_wp {
            resolve_wp(&s, ext)
        } else {
            let r = resolve_missing(&s, ext);
            // A write that faults MISSING arrives with WRITE set and no WP
            // flag; the extent is mapped write-protected, so the retried
            // store will take a WP fault and be recorded then. Nothing to
            // special-case, but worth naming so the absence of a branch
            // here is deliberate rather than an oversight.
            let _ = is_write;
            r
        };

        if let Err(e) = outcome {
            // The faulting thread stays blocked. That is deliberate: waking
            // it onto bytes we failed to load would hand it silent garbage.
            // A hang that says why beats corruption that doesn't.
            s.stats
                .lock()
                .unwrap()
                .failures
                .push(format!("unresolved fault at {addr:#x} (extent {ext}): {e}"));
        } else {
            s.resolved.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{CreateOptions, OpenOptions};
    use crate::pattern;

    const BS: u32 = 64 * 1024;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("loom-faultin-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Fill a pool with a deterministic pattern and return it plus the region.
    fn seeded(path: &std::path::Path, blocks: u64, budget: u64) -> (Loom, Region) {
        let mut l = Loom::create_with(
            path,
            CreateOptions {
                capacity: (blocks + 8) * BS as u64,
                block_size: BS,
                budget,
            },
            OpenOptions {
                budget: Some(budget),
                prefetch_depth: None,
            },
        )
        .unwrap();
        let r = l.alloc(blocks * BS as u64).unwrap();
        let mut buf = vec![0u8; BS as usize];
        for b in 0..blocks {
            pattern::fill(11, b'F', b, &mut buf);
            l.write(r, b * BS as u64, &buf).unwrap();
        }
        l.sync().unwrap();
        (l, r)
    }

    /// The headline: a raw pointer over an arena much larger than the
    /// mapped budget, read with ordinary dereferences, correct throughout,
    /// and never exceeding the budget.
    #[test]
    fn raw_pointer_reads_are_correct_and_residency_stays_bounded() {
        let p = tmp("read");
        let blocks = 256u64; // 16 MiB of arena
        let (loom, region) = seeded(&p, blocks, 8 * BS as u64);
        let arena_len = (blocks * BS as u64) as usize;
        let budget = 16 * 64 * 1024; // 1 MiB mapped, i.e. 1/16th of the arena

        let fa = FaultArena::new(
            loom,
            region,
            FaultOptions {
                len: arena_len,
                extent: 64 * 1024,
                resident_budget: budget,
            },
        )
        .unwrap();
        let ptr = fa.as_ptr();

        // Read every byte of every block through the pointer and compare
        // against the regenerated pattern. No Loom API calls at all.
        let mut expect = vec![0u8; BS as usize];
        for b in 0..blocks {
            pattern::fill(11, b'F', b, &mut expect);
            // SAFETY: [ptr + b*BS, +BS) is inside the arena; faults are
            // resolved by the handler thread.
            let got = unsafe {
                std::slice::from_raw_parts(ptr.add((b * BS as u64) as usize), BS as usize)
            };
            assert_eq!(
                pattern::first_mismatch(&expect, got),
                None,
                "block {b} wrong through the raw pointer"
            );
            assert!(
                fa.resident_bytes() <= fa.budget_bytes(),
                "residency {} exceeded budget {} at block {b}",
                fa.resident_bytes(),
                fa.budget_bytes()
            );
        }

        let st = fa.stats();
        println!("{}", st.render(fa.extent()));
        assert!(
            st.failures.is_empty(),
            "handler failures: {:?}",
            st.failures
        );
        assert!(st.load_faults > 0, "no faults were taken at all");
        assert!(
            st.evictions > 0,
            "arena is 16x the budget; eviction must have happened"
        );
        assert_eq!(
            st.write_faults, 0,
            "a read-only pass must produce no write faults — dirty tracking is not exact"
        );
        assert!(
            st.peak_resident_bytes <= budget as u64,
            "peak {} over budget {}",
            st.peak_resident_bytes,
            budget
        );
        drop(fa);
        std::fs::remove_file(&p).unwrap();
    }

    /// Writes through the pointer must be tracked and must survive
    /// eviction and reload.
    #[test]
    fn raw_pointer_writes_are_tracked_and_persist() {
        let p = tmp("write");
        let blocks = 128u64;
        let (loom, region) = seeded(&p, blocks, 8 * BS as u64);
        let arena_len = (blocks * BS as u64) as usize;

        let fa = FaultArena::new(
            loom,
            region,
            FaultOptions {
                len: arena_len,
                extent: 64 * 1024,
                resident_budget: 8 * 64 * 1024, // 16x smaller than the arena
            },
        )
        .unwrap();
        let ptr = fa.as_ptr();

        // Stamp a marker into every block through the pointer.
        for b in 0..blocks {
            let off = (b * BS as u64) as usize;
            // SAFETY: inside the arena.
            unsafe {
                let s = std::slice::from_raw_parts_mut(ptr.add(off), 8);
                s.copy_from_slice(&(0xC0FFEE00u64 + b).to_le_bytes());
            }
        }
        let st = fa.stats();
        assert!(
            st.write_faults > 0,
            "writes produced no WP faults — dirty tracking is not working"
        );
        // Every block was written, so every block must have been dirtied.
        assert!(
            st.writebacks > 0,
            "eviction of dirty extents produced no writebacks"
        );
        println!("{}", st.render(fa.extent()));
        assert!(
            st.failures.is_empty(),
            "handler failures: {:?}",
            st.failures
        );

        // Read them all back — most have been evicted and reloaded.
        for b in 0..blocks {
            let off = (b * BS as u64) as usize;
            // SAFETY: inside the arena.
            let got = unsafe { std::slice::from_raw_parts(ptr.add(off), 8) };
            assert_eq!(
                u64::from_le_bytes(got.try_into().unwrap()),
                0xC0FFEE00u64 + b,
                "marker lost for block {b}"
            );
        }

        // And the rest of each block still holds the original pattern —
        // a writeback must not have corrupted the untouched bytes.
        let mut expect = vec![0u8; BS as usize];
        for b in [0u64, 7, 63, 127] {
            pattern::fill(11, b'F', b, &mut expect);
            let off = (b * BS as u64) as usize;
            // SAFETY: inside the arena.
            let got = unsafe { std::slice::from_raw_parts(ptr.add(off + 8), BS as usize - 8) };
            assert_eq!(
                pattern::first_mismatch(&expect[8..], got),
                None,
                "block {b}: bytes outside the marker were damaged"
            );
        }

        fa.flush().unwrap();
        drop(fa);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn rejects_geometry_that_cannot_work() {
        let p = tmp("geom");
        let (loom, region) = seeded(&p, 16, 4 * BS as u64);
        // Budget smaller than one extent.
        let e = FaultArena::new(
            loom,
            region,
            FaultOptions {
                len: 16 * BS as usize,
                extent: 64 * 1024,
                resident_budget: 1024,
            },
        )
        .err()
        .expect("must refuse a budget below one extent");
        assert!(e.to_string().contains("smaller than one extent"), "{e}");
        std::fs::remove_file(&p).unwrap();
    }

    /// The headline claim: once a page is mapped, a hit is free — there is
    /// no Loom code in the path at all. Proven two ways: a second pass over
    /// a working set that fits takes ZERO further faults, and it runs at
    /// memory speed rather than device speed.
    #[test]
    fn a_hit_costs_nothing_because_no_loom_code_runs() {
        let p = tmp("hitfree");
        let blocks = 16u64; // 1 MiB arena
        let (loom, region) = seeded(&p, blocks, 4 * BS as u64);
        let arena_len = (blocks * BS as u64) as usize;

        let fa = FaultArena::new(
            loom,
            region,
            FaultOptions {
                len: arena_len,
                extent: 64 * 1024,
                // Budget covers the WHOLE arena: after one pass nothing
                // should ever fault again.
                resident_budget: arena_len,
            },
        )
        .unwrap();
        let ptr = fa.as_ptr();

        let scan = || -> u64 {
            let mut acc = 0u64;
            // SAFETY: the whole arena is inside the mapping.
            let all = unsafe { std::slice::from_raw_parts(ptr, arena_len) };
            for c in all.chunks(4096) {
                acc = acc
                    .wrapping_add(c[0] as u64)
                    .wrapping_add(c[c.len() - 1] as u64);
            }
            acc
        };

        // Pass 1: cold. Faults expected.
        let t = Instant::now();
        let a = scan();
        let cold = t.elapsed();
        let after_cold = fa.stats();
        assert!(after_cold.load_faults > 0, "cold pass took no faults");
        assert_eq!(
            after_cold.evictions, 0,
            "budget covers the arena; nothing should evict"
        );

        // Pass 2: everything is mapped. No Loom code may run.
        let t = Instant::now();
        let b = scan();
        let warm = t.elapsed();
        let after_warm = fa.stats();

        assert_eq!(a, b, "the two passes disagree about the data");
        assert_eq!(
            after_warm.load_faults,
            after_cold.load_faults,
            "a warm pass took {} additional load faults — hits are NOT free",
            after_warm.load_faults - after_cold.load_faults
        );
        assert_eq!(
            after_warm.write_faults, 0,
            "a read-only pass produced write faults"
        );

        let warm_rate = arena_len as f64 / warm.as_secs_f64();
        println!(
            "  cold pass {:?} ({}/s), warm pass {:?} ({}/s) -> {:.0}x; faults unchanged at {}",
            cold,
            crate::stats::fmt_bytes((arena_len as f64 / cold.as_secs_f64()) as u64),
            warm,
            crate::stats::fmt_bytes(warm_rate as u64),
            cold.as_secs_f64() / warm.as_secs_f64().max(1e-12),
            after_warm.load_faults
        );
        // A warm scan is plain DRAM traffic. Anything in the hundreds of
        // MiB/s or below would mean software is still in the path.
        assert!(
            warm_rate > 500.0 * 1024.0 * 1024.0,
            "warm scan only reached {}/s — something is still in the hit path",
            crate::stats::fmt_bytes(warm_rate as u64)
        );
        assert!(fa.stats().failures.is_empty());
        drop(fa);
        std::fs::remove_file(&p).unwrap();
    }
}
