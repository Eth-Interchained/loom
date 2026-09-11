//! The witness for the hot budget.
//!
//! Loom can promise what it *allocates*. Whether those bytes are physically
//! resident is the kernel's call — it may compress or swap Loom's frames
//! like any anonymous memory. So Loom never asserts residency; it measures
//! it with the same instrument the user has.
//!
//! - macOS: `task_info(TASK_VM_INFO)` → `phys_footprint` (the number
//!   Activity Monitor shows as "Memory") and `compressed`.
//! - Linux: `/proc/self/status` → `RssAnon + RssShmem` (anonymous resident),
//!   plus `RssFile` reported separately so a page-cache leak is visible.
//!
//! A field the platform cannot report is `None`. It is never zero.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    /// Bytes the OS attributes to this process as its memory footprint.
    /// macOS: phys_footprint. Linux: RssAnon + RssShmem.
    pub footprint: u64,
    /// Bytes of this process currently held compressed by the OS (macOS
    /// memory compressor). `None` where the platform does not report it.
    pub compressed: Option<u64>,
    /// Resident file-backed pages (Linux RssFile). Nonzero growth here while
    /// Loom runs would mean the page cache is caching our pool behind us.
    pub file_resident: Option<u64>,
    /// Total resident set including file-backed pages (macOS resident_size,
    /// Linux VmRSS). phys_footprint on macOS excludes clean file-backed
    /// pages, so an mmap'd file can look "free" there; this number does not
    /// let it.
    pub resident: Option<u64>,
    /// Where the numbers came from, for the report.
    pub source: &'static str,
}

impl fmt::Display for Footprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", crate::stats::fmt_bytes(self.footprint))?;
        match self.compressed {
            Some(c) => write!(f, " (compressed by OS: {})", crate::stats::fmt_bytes(c))?,
            None => write!(f, " (compressed: unknown on this platform)")?,
        }
        if let Some(fr) = self.file_resident {
            write!(f, " file-resident: {}", crate::stats::fmt_bytes(fr))?;
        }
        if let Some(r) = self.resident {
            write!(f, " rss: {}", crate::stats::fmt_bytes(r))?;
        }
        write!(f, " [{}]", self.source)
    }
}

#[cfg(target_os = "macos")]
pub fn current() -> Result<Footprint, String> {
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::message::mach_msg_type_number_t;
    use mach2::task::task_info;
    use mach2::task_info::{task_info_t, task_vm_info, TASK_VM_INFO};
    use mach2::traps::mach_task_self;
    use mach2::vm_types::natural_t;
    use std::mem;
    // SAFETY: task_vm_info is a plain-old-data struct; zeroing it is a valid
    // initial state. task_info writes at most `count` natural_t words into
    // it and updates `count`. mach_task_self() is always a valid port for
    // the calling task.
    unsafe {
        let mut info: task_vm_info = mem::zeroed();
        let mut count: mach_msg_type_number_t = (mem::size_of::<task_vm_info>()
            / mem::size_of::<natural_t>())
            as mach_msg_type_number_t;
        let kr = task_info(
            mach_task_self(),
            TASK_VM_INFO,
            &mut info as *mut task_vm_info as task_info_t,
            &mut count,
        );
        if kr != KERN_SUCCESS {
            return Err(format!(
                "task_info(TASK_VM_INFO) failed: kern_return_t {kr}"
            ));
        }
        // phys_footprint is only populated when the kernel filled at least
        // that many words (older kernels return a shorter struct).
        let words_needed = (mem::offset_of!(task_vm_info, phys_footprint) + mem::size_of::<u64>())
            / mem::size_of::<natural_t>();
        if (count as usize) < words_needed {
            return Err(format!(
                "task_info returned {count} words; phys_footprint needs {words_needed}"
            ));
        }
        Ok(Footprint {
            footprint: info.phys_footprint as u64,
            compressed: Some(info.compressed as u64),
            file_resident: None,
            resident: Some(info.resident_size as u64),
            source: "task_info(TASK_VM_INFO).phys_footprint",
        })
    }
}

#[cfg(target_os = "linux")]
pub fn current() -> Result<Footprint, String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("read /proc/self/status: {e}"))?;
    let mut anon = None;
    let mut shmem = None;
    let mut file = None;
    let mut rss = None;
    for line in status.lines() {
        let kb = |l: &str| -> Option<u64> {
            l.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .map(|v| v * 1024)
        };
        if line.starts_with("RssAnon:") {
            anon = kb(line);
        } else if line.starts_with("RssShmem:") {
            shmem = kb(line);
        } else if line.starts_with("RssFile:") {
            file = kb(line);
        } else if line.starts_with("VmRSS:") {
            rss = kb(line);
        }
    }
    match (anon, shmem) {
        (Some(a), Some(s)) => Ok(Footprint {
            footprint: a + s,
            compressed: None,
            file_resident: file,
            resident: rss,
            source: "/proc/self/status RssAnon+RssShmem",
        }),
        _ => Err("RssAnon/RssShmem missing from /proc/self/status (kernel < 4.5?)".into()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn current() -> Result<Footprint, String> {
    Err("process footprint measurement not implemented on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprint_reads_and_grows_with_allocation() {
        let before = current().expect("footprint available on CI platforms");
        // Touch 64 MiB so it is actually resident.
        let mut v = vec![0u8; 64 << 20];
        for i in (0..v.len()).step_by(4096) {
            v[i] = i as u8;
        }
        let after = current().unwrap();
        assert!(
            after.footprint >= before.footprint + (48 << 20),
            "footprint did not grow: before={before} after={after}"
        );
        std::hint::black_box(&v);
    }
}
