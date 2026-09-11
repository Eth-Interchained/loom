# Third-party dependencies

Loom's own code is under BUSL-1.1 (see LICENSE). The crates below are
dependencies with their own licenses; nothing in LICENSE changes them.

| Crate | Version | License | Used for |
|---|---|---|---|
| `libc` | 0.2 | MIT OR Apache-2.0 | `open`, `pread`, `pwrite`, `fcntl`, `ftruncate`, `fstat`, `mmap` (baseline only) |
| `xxhash-rust` | 0.8 (feature `xxh3`) | BSL-1.0 | per-block checksums, pattern seeding, superblock header checksum |
| `mach2` | 0.7 (macOS only) | BSD-2-Clause OR MIT OR Apache-2.0 | `task_info(TASK_VM_INFO)` for `phys_footprint` / `compressed` / `resident_size` |

`cargo tree` is the authoritative list; this file is the human summary.
