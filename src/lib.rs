//! # Loom
//!
//! A userspace buffer manager that gives a process a **bounded RAM
//! footprint** over a **storage-backed logical arena larger than RAM**.
//!
//! ```text
//! Application
//!      │  alloc / read / write
//!      ▼
//! ┌──────────────────────┐
//! │     LOOM ARENA       │  logical address space (capacity, e.g. 128 GiB)
//! └──────────┬───────────┘
//!            │  block map + CLOCK residency policy
//!     ┌──────┴──────┐
//!     ▼             ▼
//!   HOT           COLD
//!   frame pool    pool file (sparse, cache-bypassing I/O)
//!   (budget,      identity-mapped blocks + checksum table
//!    e.g. 8 GiB)
//! ```
//!
//! What Loom owns: the logical address space, the block map, the frame
//! pool (which *is* the hot budget), the residency decision on every miss,
//! dirty tracking, writeback, a checksum per block, and the counters
//! describing all of it.
//!
//! What the OS still owns: physical backing of Loom's frames. Loom measures
//! that with [`footprint::current`] instead of asserting it.
//!
//! What v0 does **not** do: free regions, survive a crash between `sync`s,
//! prefetch, compress, or learn device characteristics. See README.

pub mod aligned;
pub mod arena;
pub mod checksum;
pub mod error;
pub mod footprint;
pub mod frames;
pub mod io;
pub mod pattern;
pub mod prove;
pub mod stats;
pub mod superblock;

pub use arena::{CreateOptions, Info, Loom, OpenOptions, Region};
pub use error::{LoomError, Result};
pub use stats::Stats;

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

/// Default block size. 64 KiB keeps the block map at 64 MiB for a 128 GiB
/// arena (4 KiB blocks would need 1 GiB) while keeping sub-block
/// read-modify-write cheap. HDD backends will want larger; it is a pool
/// parameter, fixed at init.
pub const DEFAULT_BLOCK_SIZE: u32 = 64 * KIB as u32;
