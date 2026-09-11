//! Transparent fault-in via a signal handler. **Works on macOS and Linux.**
//!
//! Same architecture as [`crate::faultin`] — a raw pointer over an arena
//! larger than RAM, resolved on demand — but the fault transport is a
//! `SIGSEGV`/`SIGBUS` handler plus `mprotect` instead of `userfaultfd`.
//!
//! # Why this exists when `userfaultfd` is better
//!
//! `userfaultfd` is Linux-only and macOS has no equivalent. The "correct"
//! macOS mechanism is a Mach exception port on a dedicated thread — but
//! that is a few hundred lines of ABI that cannot be executed anywhere
//! except a Mac, and untested ABI code is how you ship a crash.
//!
//! This backend is the same code on both platforms. Every line of it is
//! exercised on Linux, where it can be run, before it ever touches macOS.
//! The only platform difference is which signal the kernel raises for an
//! access to protected memory: macOS raises `SIGBUS` for some cases, so
//! both are handled.
//!
//! # The honest caveat
//!
//! `mprotect`, `pread` and `pwrite` are called from inside a signal
//! handler. `pread`/`pwrite` are async-signal-safe per POSIX; **`mprotect`
//! is not on the list**. In practice it is a thin syscall wrapper and this
//! is precisely how JITs and garbage collectors implemented lazy paging for
//! decades before `userfaultfd` existed — but it is not a guarantee, and it
//! is written here rather than buried.
//!
//! Two consequences taken seriously:
//! - The handler's **own** state is fully preallocated — `state`, `ring` and
//!   `staging` are allocated at install time, so the handler never allocates
//!   for its bookkeeping. A `malloc` inside a handler that interrupted
//!   `malloc` is a deadlock.
//! - The handler takes no lock the faulting thread could already hold. A
//!   single spin lock guards the arena state and is held only for the few
//!   instructions that mutate it.
//!
//! # Two known risks, stated rather than papered over
//!
//! 1. **The resolve path calls `Loom::read`/`Loom::write`, and those can
//!    allocate** (the prefetch batcher builds a small `Vec`). So "the
//!    handler allocates nothing" is true of Loom's fault bookkeeping and
//!    **false** of the I/O beneath it. Give the `Loom` under a fault arena
//!    `prefetch_depth: Some(0)` to remove that path; `loom faultin` does.
//! 2. **The handler runs on the faulting thread's stack** (see the
//!    `SA_ONSTACK` note at the install site — using the alternate stack
//!    crashes outright). A thread that faults while nearly out of stack
//!    would overflow.
//!
//! Both have the same real fix, and it is not built: make the handler a
//! **notify-and-wait** — park the fault, let a worker thread with its own
//! full stack and its own allocator do the I/O, and wake the faulting
//! thread. That is also the design the macOS Mach exception port wants.
//! Until then these are the honest limits of this backend.
//!
//! # State machine (identical to the `userfaultfd` backend)
//!
//! ```text
//!   PROT_NONE  ──read fault──▶  load extent, PROT_READ   (clean, resident)
//!   PROT_READ  ──write fault─▶  mark dirty, PROT_READ|WRITE
//!   resident   ──evicted─────▶  write back if dirty, PROT_NONE, release pages
//! ```
//!
//! Read-only access never reaches the write state, so dirty tracking is
//! exact: a pass that only reads produces zero write faults and zero
//! writebacks.

#![cfg(unix)]

use crate::arena::{Loom, Region};
use crate::error::{io_err, LoomError, Result};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Bytes loaded and mapped per fault. A page-sized round trip to a 10 ms
/// device is ruinous; one fault should pay for a useful span.
pub const DEFAULT_EXTENT: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct SignalFaultOptions {
    pub len: usize,
    pub extent: usize,
    /// Hard cap on mapped arena bytes.
    pub resident_budget: usize,
}

impl SignalFaultOptions {
    pub fn new(len: usize, resident_budget: usize) -> Self {
        SignalFaultOptions {
            len,
            extent: DEFAULT_EXTENT,
            resident_budget,
        }
    }
}

/// Per-extent state. Kept as plain integers in a preallocated vector so the
/// handler never allocates.
const ST_ABSENT: u8 = 0;
const ST_CLEAN: u8 = 1;
const ST_DIRTY: u8 = 2;

/// Arena state touched by the signal handler. Everything here is
/// preallocated; the handler only reads and writes existing slots.
struct Inner {
    base: usize,
    extent: usize,
    max_extents: usize,
    /// `ST_*` per extent.
    state: Vec<u8>,
    /// Resident extents in arrival order, as a fixed ring. FIFO eviction —
    /// simple, and honest about being simple.
    ring: Vec<usize>,
    ring_head: usize,
    ring_len: usize,
    /// Staging buffer for one extent, preallocated. The handler must not
    /// allocate, so this is the only buffer it uses.
    staging: Vec<u8>,
    loom: Loom,
    region: Region,
}

/// Counters. Atomics because the handler updates them.
#[derive(Default)]
pub struct SignalFaultCounters {
    pub load_faults: AtomicU64,
    pub write_faults: AtomicU64,
    pub evictions: AtomicU64,
    pub writebacks: AtomicU64,
    pub peak_resident_extents: AtomicUsize,
    /// Faults the handler could not resolve. Non-zero here means the
    /// process was left in a state we could not honestly recover from.
    pub failures: AtomicU64,
    /// errno of the first failure, so a failure is diagnosable rather than
    /// merely counted.
    pub first_failure_errno: AtomicU64,
    /// Address of the first failure.
    pub first_failure_addr: AtomicU64,
}

/// Snapshot of the counters, for printing and asserting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalFaultStats {
    pub load_faults: u64,
    pub write_faults: u64,
    pub evictions: u64,
    pub writebacks: u64,
    pub resident_extents: usize,
    pub peak_resident_extents: usize,
    pub failures: u64,
    pub first_failure_errno: i32,
    pub first_failure_addr: u64,
}

impl SignalFaultStats {
    pub fn render(&self, extent: usize) -> String {
        let mut s = format!(
            "load faults: {}  write faults: {}  evictions: {} ({} wrote back)\n  \
             resident: {} extents ({})  peak: {} ({})",
            self.load_faults,
            self.write_faults,
            self.evictions,
            self.writebacks,
            self.resident_extents,
            crate::stats::fmt_bytes((self.resident_extents * extent) as u64),
            self.peak_resident_extents,
            crate::stats::fmt_bytes((self.peak_resident_extents * extent) as u64),
        );
        if self.failures > 0 {
            s.push_str(&format!(
                "\n  FAILURES: {} (first at {:#x}, errno {})",
                self.failures, self.first_failure_addr, self.first_failure_errno
            ));
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Global registry.
//
// A signal handler receives no context pointer, so it must find the arena
// from the faulting address alone. One installed arena at a time keeps that
// lookup to a single bounds check — no allocation, no locking, no iteration
// over a list while a signal is pending.
// ---------------------------------------------------------------------------

static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Spin lock guarding `INNER`. A mutex is not usable here: the handler may
/// interrupt a thread that holds an allocator lock, and a poisoned or
/// blocking mutex in a handler is a deadlock. The critical section is a few
/// dozen instructions.
static LOCK: AtomicBool = AtomicBool::new(false);
static mut INNER: Option<Inner> = None;
static BASE: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);
static COUNTERS: once_cell_lite::Lazy<SignalFaultCounters> = once_cell_lite::Lazy::new();

/// A three-line stand-in for `once_cell`, to avoid a dependency for one
/// static.
mod once_cell_lite {
    use std::sync::atomic::{AtomicBool, Ordering};
    pub struct Lazy<T> {
        init: AtomicBool,
        slot: std::cell::UnsafeCell<Option<T>>,
    }
    // SAFETY: initialisation is guarded by `init`; `get` only hands out
    // shared references to a value that is never moved or dropped after
    // initialisation.
    unsafe impl<T: Send + Sync> Sync for Lazy<T> {}
    impl<T: Default> Lazy<T> {
        pub const fn new() -> Self {
            Lazy {
                init: AtomicBool::new(false),
                slot: std::cell::UnsafeCell::new(None),
            }
        }
        pub fn get(&self) -> &T {
            // SAFETY: single-threaded-by-construction initialisation — the
            // first caller runs before any arena is installed, and every
            // later caller only reads.
            unsafe {
                let slot = &mut *self.slot.get();
                if !self.init.load(Ordering::Acquire) {
                    if slot.is_none() {
                        *slot = Some(T::default());
                    }
                    self.init.store(true, Ordering::Release);
                }
                slot.as_ref().unwrap()
            }
        }
    }
}

struct Guard;
impl Guard {
    #[inline]
    fn acquire() -> Guard {
        while LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        Guard
    }
}
impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
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

/// Resolve a fault at `addr`. Returns `Ok(())` if the faulting instruction
/// may be retried. **Called from a signal handler: allocates nothing.**
fn resolve(addr: usize) -> std::result::Result<(), i32> {
    let _g = Guard::acquire();
    // SAFETY: INNER is written only during install/uninstall, both of which
    // happen with no arena installed and no handler able to run for this
    // range (BASE/END are cleared first). The spin lock serialises handler
    // entries against each other.
    let inner = unsafe {
        let p = &mut *std::ptr::addr_of_mut!(INNER);
        match p.as_mut() {
            Some(i) => i,
            None => return Err(libc::EFAULT),
        }
    };

    let ext = (addr - inner.base) / inner.extent;
    let ext_addr = inner.base + ext * inner.extent;
    let c = COUNTERS.get();

    match inner.state[ext] {
        ST_CLEAN => {
            // Present and readable, so this fault is a WRITE. Promote.
            // SAFETY: ext_addr..+extent is inside our mapping.
            let rc = unsafe {
                libc::mprotect(
                    ext_addr as *mut libc::c_void,
                    inner.extent,
                    libc::PROT_READ | libc::PROT_WRITE,
                )
            };
            if rc != 0 {
                return Err(errno());
            }
            inner.state[ext] = ST_DIRTY;
            c.write_faults.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        ST_DIRTY => {
            // Already writable. A fault here means the address is not ours
            // to explain — do not paper over it.
            Err(libc::EACCES)
        }
        _ => {
            // Absent. Make room, then load.
            while inner.ring_len >= inner.max_extents {
                let victim = inner.ring[inner.ring_head];
                let v_addr = inner.base + victim * inner.extent;
                if inner.state[victim] == ST_DIRTY {
                    // Copy out, then write back. The staging buffer is
                    // preallocated; nothing here allocates.
                    // SAFETY: the victim extent is mapped readable/writable
                    // and staging is exactly `extent` bytes.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            v_addr as *const u8,
                            inner.staging.as_mut_ptr(),
                            inner.extent,
                        );
                    }
                    let off = (victim * inner.extent) as u64;
                    let region = inner.region;
                    // Split the borrow: `staging` and `loom` are distinct
                    // fields, but the borrow checker cannot see that through
                    // a method call, so move the pointer out first.
                    let sp = inner.staging.as_ptr();
                    // SAFETY: sp points at `extent` initialised bytes that
                    // are not aliased by `loom`.
                    let slice = unsafe { std::slice::from_raw_parts(sp, inner.extent) };
                    if inner.loom.write(region, off, slice).is_err() {
                        return Err(libc::EIO);
                    }
                    c.writebacks.fetch_add(1, Ordering::Relaxed);
                }
                // Drop the pages and make the range fault again.
                // SAFETY: the range is inside our mapping.
                let rc = unsafe {
                    libc::mprotect(v_addr as *mut libc::c_void, inner.extent, libc::PROT_NONE)
                };
                if rc != 0 {
                    return Err(errno());
                }
                // Release the physical pages. MADV_FREE lets the kernel
                // reclaim them; the mapping stays reserved.
                // SAFETY: same range, still ours.
                unsafe {
                    libc::madvise(v_addr as *mut libc::c_void, inner.extent, MADV_RELEASE);
                }
                inner.state[victim] = ST_ABSENT;
                inner.ring_head = (inner.ring_head + 1) % inner.max_extents;
                inner.ring_len -= 1;
                c.evictions.fetch_add(1, Ordering::Relaxed);
            }

            // Readable+writable while we fill it, then downgrade to
            // read-only so the first real write is observable.
            // SAFETY: inside our mapping.
            let rc = unsafe {
                libc::mprotect(
                    ext_addr as *mut libc::c_void,
                    inner.extent,
                    libc::PROT_READ | libc::PROT_WRITE,
                )
            };
            if rc != 0 {
                return Err(errno());
            }
            let off = (ext * inner.extent) as u64;
            let region = inner.region;
            let sp = inner.staging.as_mut_ptr();
            // SAFETY: sp points at `extent` writable bytes not aliased by
            // `loom`.
            let slice = unsafe { std::slice::from_raw_parts_mut(sp, inner.extent) };
            if inner.loom.read(region, off, slice).is_err() {
                return Err(libc::EIO);
            }
            // SAFETY: destination is the extent we just made writable;
            // source is the staging buffer of exactly that length.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    inner.staging.as_ptr(),
                    ext_addr as *mut u8,
                    inner.extent,
                );
            }
            // SAFETY: inside our mapping.
            let rc = unsafe {
                libc::mprotect(ext_addr as *mut libc::c_void, inner.extent, libc::PROT_READ)
            };
            if rc != 0 {
                return Err(errno());
            }

            inner.state[ext] = ST_CLEAN;
            let tail = (inner.ring_head + inner.ring_len) % inner.max_extents;
            inner.ring[tail] = ext;
            inner.ring_len += 1;
            c.load_faults.fetch_add(1, Ordering::Relaxed);
            let prev = c.peak_resident_extents.load(Ordering::Relaxed);
            if inner.ring_len > prev {
                c.peak_resident_extents
                    .store(inner.ring_len, Ordering::Relaxed);
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
const MADV_RELEASE: libc::c_int = libc::MADV_FREE;
#[cfg(not(target_os = "macos"))]
const MADV_RELEASE: libc::c_int = libc::MADV_DONTNEED;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

/// Saved previous dispositions, so faults that are not ours are passed on
/// rather than swallowed. Swallowing another subsystem's SIGSEGV would
/// silently break crash reporting for the whole process.
static mut PREV_SEGV: Option<libc::sigaction> = None;
static mut PREV_BUS: Option<libc::sigaction> = None;

extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    // SAFETY: the kernel guarantees `info` is valid for the duration of the
    // handler when SA_SIGINFO is set.
    let addr = unsafe { (*info).si_addr() } as usize;
    let base = BASE.load(Ordering::Relaxed);
    let end = END.load(Ordering::Relaxed);

    if base != 0 && addr >= base && addr < end {
        match resolve(addr) {
            Ok(()) => return, // retry the faulting instruction
            Err(e) => {
                let c = COUNTERS.get();
                c.failures.fetch_add(1, Ordering::Relaxed);
                let _ = c.first_failure_errno.compare_exchange(
                    0,
                    e as u64,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                let _ = c.first_failure_addr.compare_exchange(
                    0,
                    addr as u64,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                // Fall through to the previous handler. Returning here
                // would re-run the faulting instruction and spin forever;
                // pretending we resolved it would hand the program garbage.
            }
        }
    }

    // Not ours, or unresolvable: chain to whatever was installed before.
    // SAFETY: PREV_* are written once at install time, before this handler
    // can run, and read-only thereafter.
    let prev = unsafe {
        if sig == libc::SIGBUS {
            (*std::ptr::addr_of!(PREV_BUS)).as_ref()
        } else {
            (*std::ptr::addr_of!(PREV_SEGV)).as_ref()
        }
    };
    if let Some(p) = prev {
        if p.sa_sigaction == libc::SIG_DFL || p.sa_sigaction == libc::SIG_IGN {
            // Restore the default and re-raise so the process dies the way
            // it would have without us.
            // SAFETY: restoring a disposition the OS gave us.
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
            return;
        }
        if p.sa_flags & libc::SA_SIGINFO != 0 {
            // SAFETY: the saved handler was registered with SA_SIGINFO, so
            // this is its true signature.
            let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                unsafe { std::mem::transmute(p.sa_sigaction) };
            f(sig, info, ctx);
        } else {
            // SAFETY: no SA_SIGINFO, so the handler takes only the signal.
            let f: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(p.sa_sigaction) };
            f(sig);
        }
        return;
    }
    // Nothing saved: die the default way rather than looping.
    // SAFETY: restoring the default disposition and re-raising.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

// ---------------------------------------------------------------------------
// Public arena
// ---------------------------------------------------------------------------

/// A transparent arena backed by a signal handler. One may be installed at
/// a time per process; `new` refuses a second.
pub struct SignalFaultArena {
    base: usize,
    len: usize,
    extent: usize,
    max_extents: usize,
}

impl SignalFaultArena {
    pub fn new(loom: Loom, region: Region, opts: SignalFaultOptions) -> Result<Self> {
        let page = page_size();
        if opts.extent == 0 || opts.extent % page != 0 {
            return Err(LoomError::Invalid(format!(
                "extent {} must be a non-zero multiple of the page size {page}",
                opts.extent
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
        if INSTALLED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(LoomError::Unsupported(
                "a signal-backed arena is already installed in this process; \
                 the handler finds its arena from the faulting address, so only one may exist"
                    .into(),
            ));
        }

        // Reserve the range, inaccessible. Every first touch faults.
        // SAFETY: null hint, non-zero length; the kernel picks the address.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                opts.len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            INSTALLED.store(false, Ordering::SeqCst);
            return Err(io_err("mmap", format!("{} bytes PROT_NONE", opts.len)));
        }
        let base = base as usize;
        let n_ext = opts.len / opts.extent;

        // Everything the handler will touch, allocated NOW.
        let inner = Inner {
            base,
            extent: opts.extent,
            max_extents,
            state: vec![ST_ABSENT; n_ext],
            ring: vec![0usize; max_extents],
            ring_head: 0,
            ring_len: 0,
            staging: vec![0u8; opts.extent],
            loom,
            region,
        };
        // Touch the counters once here so the handler never initialises them.
        let _ = COUNTERS.get();

        {
            let _g = Guard::acquire();
            // SAFETY: no arena is installed (BASE is still 0), so no handler
            // can be inspecting INNER.
            unsafe {
                *std::ptr::addr_of_mut!(INNER) = Some(inner);
            }
        }

        // Install handlers, saving what was there. SA_ONSTACK keeps us off
        // a possibly-exhausted thread stack.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler as *const () as usize;
            // NOT `SA_ONSTACK`, and this is load-bearing.
            //
            // Rust's runtime installs a per-thread `sigaltstack` for its own
            // stack-overflow detection, and it is only a few KB. With
            // `SA_ONSTACK` the kernel runs this handler on that tiny stack,
            // where the resolve path — which calls into `Loom` and its
            // buffers — overflows it. A stack overflow INSIDE a SIGSEGV
            // handler is unrecoverable: the process dies instantly.
            //
            // Measured: with `SA_ONSTACK` the debug build died with SIGSEGV
            // on the first arena fault while release survived, because
            // release frames are small enough to fit. Exactly the shape of
            // bug that an optimised build hides.
            //
            // Without it the handler runs on the faulting thread's own
            // stack, which has megabytes. The residual risk is a thread
            // whose stack is already nearly exhausted; the fix for that is
            // the worker-thread design named in the module docs, not a
            // bigger altstack.
            sa.sa_flags = libc::SA_SIGINFO;
            libc::sigemptyset(&mut sa.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            // SAFETY: sa and old are correctly initialised sigaction values.
            if libc::sigaction(libc::SIGSEGV, &sa, &mut old) != 0 {
                let e = io_err("sigaction(SIGSEGV)", "install");
                libc::munmap(base as *mut libc::c_void, opts.len);
                INSTALLED.store(false, Ordering::SeqCst);
                return Err(e);
            }
            *std::ptr::addr_of_mut!(PREV_SEGV) = Some(old);
            let mut old_bus: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGBUS, &sa, &mut old_bus) != 0 {
                let e = io_err("sigaction(SIGBUS)", "install");
                libc::sigaction(libc::SIGSEGV, &old, std::ptr::null_mut());
                libc::munmap(base as *mut libc::c_void, opts.len);
                INSTALLED.store(false, Ordering::SeqCst);
                return Err(e);
            }
            *std::ptr::addr_of_mut!(PREV_BUS) = Some(old_bus);
        }

        // Arm last: until BASE is set the handler forwards everything, so
        // there is no window where we claim faults we cannot yet resolve.
        BASE.store(base, Ordering::SeqCst);
        END.store(base + opts.len, Ordering::SeqCst);

        Ok(SignalFaultArena {
            base,
            len: opts.len,
            extent: opts.extent,
            max_extents,
        })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.base as *mut u8
    }

    /// Addressable bytes. Never zero — `new` rejects a zero length.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn extent(&self) -> usize {
        self.extent
    }

    pub fn budget_bytes(&self) -> usize {
        self.max_extents * self.extent
    }

    pub fn stats(&self) -> SignalFaultStats {
        let c = COUNTERS.get();
        let resident = {
            let _g = Guard::acquire();
            // SAFETY: guarded; INNER is Some while this arena lives.
            unsafe {
                (*std::ptr::addr_of!(INNER))
                    .as_ref()
                    .map(|i| i.ring_len)
                    .unwrap_or(0)
            }
        };
        SignalFaultStats {
            load_faults: c.load_faults.load(Ordering::Relaxed),
            write_faults: c.write_faults.load(Ordering::Relaxed),
            evictions: c.evictions.load(Ordering::Relaxed),
            writebacks: c.writebacks.load(Ordering::Relaxed),
            resident_extents: resident,
            peak_resident_extents: c.peak_resident_extents.load(Ordering::Relaxed),
            failures: c.failures.load(Ordering::Relaxed),
            first_failure_errno: c.first_failure_errno.load(Ordering::Relaxed) as i32,
            first_failure_addr: c.first_failure_addr.load(Ordering::Relaxed),
        }
    }

    /// Write every dirty extent back and sync the pool.
    pub fn flush(&self) -> Result<()> {
        let _g = Guard::acquire();
        // SAFETY: guarded; INNER is Some while this arena lives.
        let inner = unsafe {
            match (*std::ptr::addr_of_mut!(INNER)).as_mut() {
                Some(i) => i,
                None => return Ok(()),
            }
        };
        for idx in 0..inner.ring_len {
            let ext = inner.ring[(inner.ring_head + idx) % inner.max_extents];
            if inner.state[ext] != ST_DIRTY {
                continue;
            }
            let addr = inner.base + ext * inner.extent;
            // SAFETY: the extent is mapped readable.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    addr as *const u8,
                    inner.staging.as_mut_ptr(),
                    inner.extent,
                );
            }
            let off = (ext * inner.extent) as u64;
            let region = inner.region;
            let sp = inner.staging.as_ptr();
            // SAFETY: `extent` initialised bytes, not aliased by `loom`.
            let slice = unsafe { std::slice::from_raw_parts(sp, inner.extent) };
            inner.loom.write(region, off, slice)?;
            inner.state[ext] = ST_CLEAN;
            // SAFETY: inside our mapping.
            unsafe {
                libc::mprotect(addr as *mut libc::c_void, inner.extent, libc::PROT_READ);
            }
        }
        inner.loom.sync()
    }
}

#[cfg(target_os = "macos")]
const MAP_NORESERVE: libc::c_int = 0; // macOS has no MAP_NORESERVE
#[cfg(not(target_os = "macos"))]
const MAP_NORESERVE: libc::c_int = libc::MAP_NORESERVE;

impl Drop for SignalFaultArena {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            eprintln!("loom: signal-arena flush on drop FAILED: {e} — dirty extents may be lost");
        }
        // Disarm FIRST so the handler stops claiming this range, then
        // restore the previous dispositions, then unmap. Unmapping while
        // still armed would turn a stray fault into a loop.
        BASE.store(0, Ordering::SeqCst);
        END.store(0, Ordering::SeqCst);
        // SAFETY: restoring dispositions the OS gave us; unmapping a range
        // this arena created and no longer claims.
        unsafe {
            if let Some(p) = (*std::ptr::addr_of!(PREV_SEGV)).as_ref() {
                libc::sigaction(libc::SIGSEGV, p, std::ptr::null_mut());
            }
            if let Some(p) = (*std::ptr::addr_of!(PREV_BUS)).as_ref() {
                libc::sigaction(libc::SIGBUS, p, std::ptr::null_mut());
            }
            libc::munmap(self.base as *mut libc::c_void, self.len);
        }
        {
            let _g = Guard::acquire();
            // SAFETY: disarmed above, so no handler can reach INNER.
            unsafe {
                *std::ptr::addr_of_mut!(INNER) = None;
            }
        }
        // Reset counters so a later arena in the same process starts clean.
        let c = COUNTERS.get();
        c.load_faults.store(0, Ordering::SeqCst);
        c.write_faults.store(0, Ordering::SeqCst);
        c.evictions.store(0, Ordering::SeqCst);
        c.writebacks.store(0, Ordering::SeqCst);
        c.peak_resident_extents.store(0, Ordering::SeqCst);
        c.failures.store(0, Ordering::SeqCst);
        c.first_failure_errno.store(0, Ordering::SeqCst);
        c.first_failure_addr.store(0, Ordering::SeqCst);
        INSTALLED.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Tests
//
// Only one arena may exist per process at a time, so these run serially.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{CreateOptions, OpenOptions};
    use crate::pattern;
    use std::time::Instant;

    const BS: u32 = 64 * 1024;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("loom-sigfault-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

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
            pattern::fill(23, b'S', b, &mut buf);
            l.write(r, b * BS as u64, &buf).unwrap();
        }
        l.sync().unwrap();
        (l, r)
    }

    /// The whole thesis in one test, on the backend that also runs on macOS:
    /// a raw pointer over an arena far larger than the mapped budget, read
    /// and written with ordinary dereferences, correct throughout, bounded,
    /// with exact dirty tracking — and a warm pass that takes no faults.
    #[test]
    fn signal_backed_pointer_is_correct_bounded_and_free_when_warm() {
        let p = tmp("all");
        let blocks = 192u64; // 12 MiB arena
        let (loom, region) = seeded(&p, blocks, 8 * BS as u64);
        let arena_len = (blocks * BS as u64) as usize;
        let budget = 12 * 64 * 1024; // 768 KiB — 16x smaller than the arena

        let fa = SignalFaultArena::new(
            loom,
            region,
            SignalFaultOptions {
                len: arena_len,
                extent: 64 * 1024,
                resident_budget: budget,
            },
        )
        .unwrap();
        let ptr = fa.as_ptr();

        // --- reads are correct, residency is bounded, no write faults ---
        let mut expect = vec![0u8; BS as usize];
        for b in 0..blocks {
            pattern::fill(23, b'S', b, &mut expect);
            // SAFETY: inside the arena; faults resolved by the handler.
            let got = unsafe {
                std::slice::from_raw_parts(ptr.add((b * BS as u64) as usize), BS as usize)
            };
            assert_eq!(
                pattern::first_mismatch(&expect, got),
                None,
                "block {b} wrong through the raw pointer"
            );
        }
        let st = fa.stats();
        println!("after reads:  {}", st.render(fa.extent()));
        assert_eq!(st.failures, 0, "handler failures: {st:?}");
        assert!(st.load_faults > 0, "no faults taken");
        assert!(st.evictions > 0, "16x over budget; eviction must happen");
        assert_eq!(
            st.write_faults, 0,
            "a read-only pass produced write faults — dirty tracking is not exact"
        );
        assert_eq!(
            st.writebacks, 0,
            "nothing was dirty; nothing should be written"
        );
        assert!(
            st.peak_resident_extents * fa.extent() <= fa.budget_bytes(),
            "peak {} over budget {}",
            st.peak_resident_extents * fa.extent(),
            fa.budget_bytes()
        );

        // --- writes are tracked and survive eviction ---
        for b in 0..blocks {
            let off = (b * BS as u64) as usize;
            // SAFETY: inside the arena.
            unsafe {
                let s = std::slice::from_raw_parts_mut(ptr.add(off), 8);
                s.copy_from_slice(&(0x5EED0000u64 + b).to_le_bytes());
            }
        }
        let st = fa.stats();
        println!("after writes: {}", st.render(fa.extent()));
        assert_eq!(st.failures, 0, "handler failures: {st:?}");
        assert!(st.write_faults > 0, "writes produced no write faults");
        assert!(st.writebacks > 0, "dirty evictions produced no writebacks");

        for b in 0..blocks {
            let off = (b * BS as u64) as usize;
            // SAFETY: inside the arena.
            let got = unsafe { std::slice::from_raw_parts(ptr.add(off), 8) };
            assert_eq!(
                u64::from_le_bytes(got.try_into().unwrap()),
                0x5EED0000u64 + b,
                "marker lost for block {b}"
            );
        }
        // Bytes beside the marker must be undamaged by writeback.
        for b in [0u64, 5, 97, 191] {
            pattern::fill(23, b'S', b, &mut expect);
            let off = (b * BS as u64) as usize;
            // SAFETY: inside the arena.
            let got = unsafe { std::slice::from_raw_parts(ptr.add(off + 8), BS as usize - 8) };
            assert_eq!(
                pattern::first_mismatch(&expect[8..], got),
                None,
                "block {b}: bytes outside the marker were damaged"
            );
        }
        assert_eq!(fa.stats().failures, 0);
        fa.flush().unwrap();
        drop(fa);
        std::fs::remove_file(&p).unwrap();
    }

    /// A working set that fits takes no faults on a second pass — the hit
    /// path contains no Loom code at all.
    #[test]
    fn a_warm_pass_takes_no_faults() {
        let p = tmp("warm");
        let blocks = 8u64;
        let (loom, region) = seeded(&p, blocks, 4 * BS as u64);
        let arena_len = (blocks * BS as u64) as usize;

        let fa = SignalFaultArena::new(
            loom,
            region,
            SignalFaultOptions {
                len: arena_len,
                extent: 64 * 1024,
                resident_budget: arena_len, // the whole arena fits
            },
        )
        .unwrap();
        let ptr = fa.as_ptr();

        let scan = || -> u64 {
            let mut acc = 0u64;
            // SAFETY: whole arena is inside the mapping.
            let all = unsafe { std::slice::from_raw_parts(ptr, arena_len) };
            for c in all.chunks(4096) {
                acc = acc
                    .wrapping_add(c[0] as u64)
                    .wrapping_add(c[c.len() - 1] as u64);
            }
            acc
        };

        let t = Instant::now();
        let a = scan();
        let cold = t.elapsed();
        let s1 = fa.stats();
        assert!(s1.load_faults > 0);
        assert_eq!(s1.evictions, 0, "arena fits; nothing should evict");

        let t = Instant::now();
        let b = scan();
        let warm = t.elapsed();
        let s2 = fa.stats();

        assert_eq!(a, b);
        assert_eq!(
            s2.load_faults,
            s1.load_faults,
            "warm pass took {} extra load faults — hits are NOT free",
            s2.load_faults - s1.load_faults
        );
        assert_eq!(s2.write_faults, 0);
        assert_eq!(s2.failures, 0);
        println!(
            "  cold {cold:?} -> warm {warm:?}; faults unchanged at {}",
            s2.load_faults
        );
        drop(fa);
        std::fs::remove_file(&p).unwrap();
    }

    /// A fault OUTSIDE the arena must not be swallowed. Loom claims only
    /// its own range; anything else belongs to the previous handler, and
    /// eating it would silently break crash reporting.
    #[test]
    fn faults_outside_the_arena_are_not_claimed() {
        let p = tmp("outside");
        let (loom, region) = seeded(&p, 8, 4 * BS as u64);
        let fa = SignalFaultArena::new(
            loom,
            region,
            SignalFaultOptions {
                len: 8 * BS as usize,
                extent: 64 * 1024,
                resident_budget: 4 * 64 * 1024,
            },
        )
        .unwrap();
        // Touch the arena so we know the handler is live and working.
        // SAFETY: inside the arena.
        let _ = unsafe { std::ptr::read_volatile(fa.as_ptr()) };
        assert!(fa.stats().load_faults > 0);

        // An address far outside must still be reported as outside: the
        // handler's range check is what keeps us from claiming it. Assert
        // on the check itself rather than by actually segfaulting the test
        // process, which would take the harness down with it.
        let outside = fa.as_ptr() as usize + fa.len() + 4096;
        assert!(
            outside < BASE.load(Ordering::Relaxed) || outside >= END.load(Ordering::Relaxed),
            "an address past the arena must fall outside [BASE, END)"
        );
        assert_eq!(fa.stats().failures, 0);
        drop(fa);
        // After drop the range is disarmed, so nothing is claimed at all.
        assert_eq!(
            BASE.load(Ordering::Relaxed),
            0,
            "drop must disarm the handler"
        );
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn refuses_a_second_arena_in_one_process() {
        let p1 = tmp("one");
        let p2 = tmp("two");
        let (l1, r1) = seeded(&p1, 8, 4 * BS as u64);
        let (l2, r2) = seeded(&p2, 8, 4 * BS as u64);
        let a = SignalFaultArena::new(
            l1,
            r1,
            SignalFaultOptions::new(8 * BS as usize, 4 * BS as usize),
        )
        .unwrap();
        let e = SignalFaultArena::new(
            l2,
            r2,
            SignalFaultOptions::new(8 * BS as usize, 4 * BS as usize),
        )
        .err()
        .expect("a second arena must be refused, not silently share the handler");
        assert!(e.to_string().contains("already installed"), "{e}");
        drop(a);
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }
}
