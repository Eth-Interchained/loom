//! Every failure path in Loom names itself. There is no error variant that
//! means "something went wrong"; each one carries the condition that caused
//! it, so a caller can distinguish a bad pool file from a bad argument from a
//! corrupt block.

use std::fmt;

#[derive(Debug)]
pub enum LoomError {
    /// An OS call failed. `op` names the call, `path_or_ctx` names what it was
    /// operating on, `source` is the raw errno-backed error.
    Io {
        op: &'static str,
        ctx: String,
        source: std::io::Error,
    },
    /// The pool file does not start with the Loom magic.
    BadMagic { found: [u8; 8] },
    /// The pool file is a Loom pool but from an incompatible format version.
    Version { found: u32, supported: u32 },
    /// The superblock is internally inconsistent (e.g. block size not a
    /// multiple of the I/O alignment, or table offsets overlapping).
    BadSuperblock(String),
    /// A block's stored checksum does not match the bytes read back from the
    /// backing store. This is surfaced, never masked.
    Corrupt {
        block: u64,
        expected: u64,
        actual: u64,
    },
    /// The arena has no room for the requested region.
    Capacity { requested: u64, available: u64 },
    /// A read or write addressed bytes outside its region.
    OutOfBounds {
        region: u32,
        offset: u64,
        len: u64,
        region_len: u64,
    },
    /// An argument violates a structural constraint (alignment, zero size,
    /// budget smaller than one block, ...).
    Invalid(String),
    /// The region table is full (v0 has a fixed-size table).
    RegionTableFull { max: usize },
    /// A capability this build/platform does not provide. The string names
    /// exactly what is missing and why.
    Unsupported(String),
}

impl fmt::Display for LoomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoomError::Io { op, ctx, source } => write!(f, "{op}({ctx}): {source}"),
            LoomError::BadMagic { found } => {
                write!(f, "not a Loom pool (magic bytes {:?})", found)
            }
            LoomError::Version { found, supported } => write!(
                f,
                "pool format version {found} not supported (this build supports {supported})"
            ),
            LoomError::BadSuperblock(s) => write!(f, "bad superblock: {s}"),
            LoomError::Corrupt {
                block,
                expected,
                actual,
            } => write!(
                f,
                "block {block} corrupt: stored checksum {expected:#018x}, read back {actual:#018x}"
            ),
            LoomError::Capacity {
                requested,
                available,
            } => write!(
                f,
                "arena capacity exhausted: requested {requested} bytes, {available} available"
            ),
            LoomError::OutOfBounds {
                region,
                offset,
                len,
                region_len,
            } => write!(
                f,
                "region {region}: access [{offset}, {offset}+{len}) outside region of {region_len} bytes"
            ),
            LoomError::Invalid(s) => write!(f, "invalid argument: {s}"),
            LoomError::RegionTableFull { max } => {
                write!(f, "region table full ({max} regions max in v0)")
            }
            LoomError::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for LoomError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoomError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, LoomError>;

pub(crate) fn io_err(op: &'static str, ctx: impl Into<String>) -> LoomError {
    LoomError::Io {
        op,
        ctx: ctx.into(),
        source: std::io::Error::last_os_error(),
    }
}
