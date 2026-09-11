//! The backing store: one file, explicit positional I/O, kernel page cache
//! bypassed.
//!
//! This is the primitive decision of Loom v0. With `mmap` the kernel's
//! unified buffer cache decides which arena bytes are in RAM and the "hot
//! budget" is fiction. With `pread`/`pwrite` into frames Loom allocated, and
//! the file opened cache-bypassing, the only arena bytes in RAM are the ones
//! Loom put there.
//!
//! Platform notes, stated rather than hidden:
//! - macOS: `fcntl(F_NOCACHE, 1)` is a *hint*. It is honoured for
//!   page-aligned I/O (which [`crate::aligned::AlignedBuf`] guarantees) and
//!   ignored for unaligned I/O. Durability is `fcntl(F_FULLFSYNC)` — plain
//!   `fsync` does not flush the drive's write cache on macOS.
//! - Linux: `O_DIRECT`. Requires aligned buffers/offsets/lengths (we have
//!   them). If the filesystem refuses `O_DIRECT` the open fails loudly with
//!   the errno; Loom does not silently fall back to cached I/O. Durability is
//!   `fdatasync`.
//!
//! Every `unsafe` block here is a single libc call on an fd this struct owns.

use crate::error::{io_err, LoomError, Result};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub struct Backing {
    fd: libc::c_int,
    path: String,
    /// What cache-bypass mode the open actually established.
    pub cache_mode: &'static str,
}

impl Backing {
    /// Create a new file (fails if it exists) or open an existing one.
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| LoomError::Invalid("path contains NUL".into()))?;
        let pstr = path.display().to_string();

        let mut flags = libc::O_RDWR | libc::O_CLOEXEC;
        if create {
            flags |= libc::O_CREAT | libc::O_EXCL;
        }
        #[cfg(target_os = "linux")]
        {
            flags |= libc::O_DIRECT;
        }
        // SAFETY: cpath is a valid NUL-terminated string for the duration of
        // the call; open has no other memory-safety preconditions.
        let fd = unsafe { libc::open(cpath.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(io_err("open", pstr));
        }
        let cache_mode;
        #[cfg(target_os = "macos")]
        {
            // SAFETY: fd is a valid descriptor we just opened.
            let r = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) };
            if r != 0 {
                let e = io_err("fcntl(F_NOCACHE)", pstr);
                // SAFETY: fd is valid and owned here; closing before returning
                // the error avoids a leak.
                unsafe { libc::close(fd) };
                return Err(e);
            }
            cache_mode = "macOS F_NOCACHE (hint; honoured for aligned I/O)";
        }
        #[cfg(target_os = "linux")]
        {
            cache_mode = "Linux O_DIRECT";
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            // SAFETY: fd valid and owned.
            unsafe { libc::close(fd) };
            return Err(LoomError::Unsupported(
                "no cache-bypass primitive implemented for this platform".into(),
            ));
        }
        Ok(Backing {
            fd,
            path: pstr,
            cache_mode,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Set the file's logical length. On APFS/ext4/xfs this creates a sparse
    /// file: unwritten ranges consume no disk blocks.
    pub fn set_len(&self, len: u64) -> Result<()> {
        // SAFETY: fd valid; ftruncate has no pointer arguments.
        let r = unsafe { libc::ftruncate(self.fd, len as libc::off_t) };
        if r != 0 {
            return Err(io_err("ftruncate", format!("{} to {}", self.path, len)));
        }
        Ok(())
    }

    /// Logical length and bytes actually allocated on disk (st_blocks * 512).
    pub fn sizes(&self) -> Result<(u64, u64)> {
        // SAFETY: stat is POD; zeroed is a valid initial state; fstat fills it.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstat(self.fd, &mut st) };
        if r != 0 {
            return Err(io_err("fstat", self.path.clone()));
        }
        Ok((st.st_size as u64, (st.st_blocks as u64) * 512))
    }

    /// Read exactly `buf.len()` bytes at `offset`. Buffer, offset and length
    /// must be I/O-aligned (callers pass `AlignedBuf` slices at block
    /// boundaries). A short read past EOF is an error, not zero-fill: Loom's
    /// zero semantics live in the checksum table, not in EOF behaviour.
    pub fn pread_exact(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            // SAFETY: buf[done..] is a valid writable region of buf.len()-done
            // bytes; fd is valid.
            let n = unsafe {
                libc::pread(
                    self.fd,
                    buf[done..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - done,
                    (offset + done as u64) as libc::off_t,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(LoomError::Io {
                    op: "pread",
                    ctx: format!(
                        "{} @{} len {}",
                        self.path,
                        offset + done as u64,
                        buf.len() - done
                    ),
                    source: e,
                });
            }
            if n == 0 {
                return Err(LoomError::Io {
                    op: "pread",
                    ctx: format!(
                        "{} @{}: unexpected EOF after {} of {} bytes",
                        self.path,
                        offset,
                        done,
                        buf.len()
                    ),
                    source: std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
                });
            }
            done += n as usize;
        }
        Ok(())
    }

    /// Write all of `buf` at `offset`. Same alignment contract as pread.
    pub fn pwrite_all(&self, buf: &[u8], offset: u64) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            // SAFETY: buf[done..] is a valid readable region; fd is valid.
            let n = unsafe {
                libc::pwrite(
                    self.fd,
                    buf[done..].as_ptr() as *const libc::c_void,
                    buf.len() - done,
                    (offset + done as u64) as libc::off_t,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(LoomError::Io {
                    op: "pwrite",
                    ctx: format!(
                        "{} @{} len {}",
                        self.path,
                        offset + done as u64,
                        buf.len() - done
                    ),
                    source: e,
                });
            }
            if n == 0 {
                return Err(LoomError::Io {
                    op: "pwrite",
                    ctx: format!("{} @{}: wrote 0 bytes", self.path, offset + done as u64),
                    source: std::io::Error::from(std::io::ErrorKind::WriteZero),
                });
            }
            done += n as usize;
        }
        Ok(())
    }

    /// Force written data to stable storage — through the drive cache, not
    /// just to it.
    pub fn sync(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: fd valid; F_FULLFSYNC takes no argument.
            let r = unsafe { libc::fcntl(self.fd, libc::F_FULLFSYNC) };
            if r != 0 {
                return Err(io_err("fcntl(F_FULLFSYNC)", self.path.clone()));
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            // SAFETY: fd valid.
            let r = unsafe { libc::fdatasync(self.fd) };
            if r != 0 {
                return Err(io_err("fdatasync", self.path.clone()));
            }
        }
        Ok(())
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        // SAFETY: fd is valid and owned exclusively by this struct.
        unsafe { libc::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aligned::{AlignedBuf, IO_ALIGN};

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("loom-io-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn sparse_create_write_read_roundtrip() {
        let p = tmp("rt");
        let b = Backing::open(&p, true).unwrap();
        b.set_len(64 << 20).unwrap();
        let (len, alloc) = b.sizes().unwrap();
        assert_eq!(len, 64 << 20);
        assert!(
            alloc < (1 << 20),
            "sparse file should allocate ~0, got {alloc}"
        );

        let mut w = AlignedBuf::zeroed(IO_ALIGN * 4);
        for (i, x) in w.iter_mut().enumerate() {
            *x = (i % 251) as u8;
        }
        b.pwrite_all(&w, 32 << 20).unwrap();
        let mut r = AlignedBuf::zeroed(IO_ALIGN * 4);
        b.pread_exact(&mut r, 32 << 20).unwrap();
        assert_eq!(&w[..], &r[..]);
        b.sync().unwrap();
        let (_, alloc2) = b.sizes().unwrap();
        assert!(alloc2 >= IO_ALIGN as u64 * 4);
        drop(b);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn create_refuses_existing() {
        let p = tmp("excl");
        let _b = Backing::open(&p, true).unwrap();
        let e = match Backing::open(&p, true) {
            Ok(_) => panic!("O_EXCL must refuse"),
            Err(e) => e,
        };
        assert!(matches!(e, LoomError::Io { op: "open", .. }), "{e}");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn read_past_eof_is_an_error_not_zeros() {
        let p = tmp("eof");
        let b = Backing::open(&p, true).unwrap();
        b.set_len(IO_ALIGN as u64).unwrap();
        let mut r = AlignedBuf::zeroed(IO_ALIGN * 2);
        let e = b.pread_exact(&mut r, 0).expect_err("short read must error");
        assert!(e.to_string().contains("EOF"), "{e}");
        std::fs::remove_file(&p).unwrap();
    }
}
