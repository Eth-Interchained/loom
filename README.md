# Loom

**A userspace buffer manager that gives a process a bounded RAM footprint over a storage-backed logical arena larger than RAM.**

You have 16–32 GiB of RAM and hundreds of GiB of SSD. Loom turns a configurable slice of that storage into a logical memory arena your program can address, while Loom — not the kernel's page cache — decides which parts are in RAM and holds that to a budget you set.

```text
Application
     │  alloc / read / write
     ▼
┌──────────────────────┐
│     LOOM ARENA       │  logical address space   (capacity, e.g. 128 GiB)
└──────────┬───────────┘
           │  block map + CLOCK residency policy
    ┌──────┴──────┐
    ▼             ▼
  HOT           COLD
  frame pool    pool file on SSD/HDD
  (budget,      sparse, cache-bypassing I/O,
   e.g. 8 GiB)  one checksum per block
```

Storage is not RAM. Loom does not make disk fast; it makes a large working set *addressable* with a *bounded, measured* RAM cost, and tells you exactly what that cost was.

## Status: v0 — the primitive, proven at sandbox scale

v0 exists to answer one question: *can Loom provide a storage-backed logical arena substantially larger than physical RAM, keep its own RAM consumption bounded, and correctly retrieve data as blocks move between hot and cold?*

`loom prove` runs that experiment against a real device. Its output is a verdict in which every number is a measurement. See [PROOF-RUNS.md](PROOF-RUNS.md) for recorded runs.

What v0 **does**:

- Sparse pool file; a 128 GiB arena costs ~68 KiB on disk until used.
- Explicit `alloc` / `read` / `write` over regions (Level 1 API).
- Fixed frame pool = the hot budget. `frame_count × block_size` is the only place arena bytes live in RAM.
- CLOCK eviction, dirty tracking, writeback on eviction and on `sync`.
- XXH3-64 per block, verified on every promotion from disk. A flipped byte on disk returns `Err(Corrupt { block })`, never data.
- Clean `sync` → close → reopen persistence (`F_FULLFSYNC` on macOS, `fdatasync` on Linux).
- Its own footprint witness: `phys_footprint` + `compressed` via `task_info` on macOS, `RssAnon+RssShmem` (+`RssFile`, `VmRSS`) on Linux.
- Hit and miss latency histograms kept **separate**. They are never averaged.

What v0 **does not do** (stated, not implied):

- **Crash consistency.** A crash between `sync`s can leave blocks written whose checksums were not; those blocks then read as `Corrupt`. Detected, not recovered. No log, no atomic table updates.
- **Free.** Regions are never released. The table is fixed at 4095 regions.
- **Prefetch, compression, device profiling, HDD-specific extents, `loom stats`, memory-pressure response, Linux as a first-class target, any binding other than Rust.**
- **Residency guarantees.** Loom bounds what it *allocates*. The OS may still compress or swap those frames; Loom reports that (`compressed` on macOS) instead of denying it. `mlock` is deliberately not on by default.

## The primitive decision

Three ways to build this were compared; Loom uses the third.

| Approach | Who decides which arena bytes are in RAM | Verdict |
|---|---|---|
| `mmap` the pool file | The kernel's unified buffer cache. `madvise` on file-backed pages is advisory on macOS. | A "budget" would be a number in a struct. This is macOS swap with a logo. |
| `mmap` + `mincore`/`madvise`/`msync` hints | Still the kernel; you can *watch* it, not steer it. | Worse: it looks like control. |
| **`pread`/`pwrite` into Loom-owned frames, file opened cache-bypassing** | **Loom.** The frames are the hot tier; the kernel page cache never holds pool data. | The budget is arithmetic on allocations Loom made. |

Cache bypass is `fcntl(F_NOCACHE)` on macOS (a hint, honoured for page-aligned I/O — every Loom buffer is 4 KiB-aligned by type) and `O_DIRECT` on Linux (refuses to open rather than silently falling back). Everything Loom uses is public API: `open`, `pread`, `pwrite`, `fcntl`, `ftruncate`, `fstat`, `task_info`. No private frameworks.

What Loom owns that `mmap` would not give you: the logical address space, the block map, the frame pool, the residency decision on every miss, dirty tracking and writeback timing, a checksum per block, and the counters describing all of it. That is a buffer manager for one flat address space — less than a database, more than a file mapping.

## Block size

Default **64 KiB**, fixed at `init`. Reasoning: at 4 KiB a 128 GiB arena needs 32 M map entries (≈1 GiB of metadata, which alone breaks the budget story); at 64 KiB it needs 2 M (≈64 MiB); at 1 MiB sub-block writes become expensive read-modify-writes. HDD backends will want 1 MiB+; it is one parameter. Metadata in RAM is reported by `loom info`.

## Install / build

```sh
cargo build --release
./target/release/loom --help
```

Rust 1.75+. Dependencies: `libc`, `xxhash-rust`; `mach2` on macOS. Nothing else.

## Usage

```sh
# create a sparse 128 GiB pool with an 8 GiB hot budget
loom init ~/loom.pool --size 128G --budget 8G

loom info ~/loom.pool
```

```rust
use loom::{Loom, OpenOptions, GIB};

let mut arena = Loom::open("~/loom.pool".as_ref(), OpenOptions { budget: Some(8 * GIB) })?;
let region = arena.alloc(80 * GIB)?;
arena.write(region, offset, &data)?;
arena.read(region, offset, &mut buf)?;
arena.sync()?;   // writeback + tables + full fsync
arena.close()?;  // sync, then close; errors are returned, not swallowed
```

Every read/write goes through a frame copy. That is the price of owning residency; it is the price every database buffer manager pays for the same reason.

## Running the proof

```sh
# sandbox scale (finishes in under a minute on an SSD)
loom prove --pool /Volumes/Fast/loom.pool --size 8G --budget 256M --region 6G

# the real one: arena region 10x the budget, budget below your RAM
loom prove --pool /Volumes/Fast/loom.pool --size 128G --budget 8G --region 80G
```

Put the pool on the device you are making a claim about. Never benchmark `/tmp` and infer anything about another disk.

The proof, in order — each step asserted, the run fails on the first false claim:

0. Measure the process footprint before Loom allocates anything.
1. Create the pool; assert it is sparse (≈0 on disk).
2. Allocate a region ≥ 4× the budget (refused otherwise).
3. Write deterministic pattern A across the whole region, sampling footprint throughout; assert the data landed on disk (`st_blocks`).
4. Assert `max footprint ≤ start + hot bytes + metadata + 48 MiB slack`, continuously.
5. Evict everything, overwrite every free frame with `0xEE`, read A back, compare byte-for-byte.
6. Report Loom's own counters (the sequential passes are 100 % misses by construction).
7. Write pattern B, `sync`, close, **reopen**, verify B.
8. Flip one byte on disk at unchanged size → expect `Corrupt` for exactly that block. Restore the byte → expect a clean read (the false-positive check).
9. Skewed workload (90 % in a window sized to 80 % of the frames, 10 % uniform over the region): Loom, then raw cache-bypassing `pread` on the same blocks, then plain `mmap` of the same file. Hit/miss latency reported separately; RAM after each baseline reported.

Exit 0 on pass, 1 on fail. The last line is the verdict sentence filled with measurements, or `PROOF FAILED:` with the reasons.

## Failure criteria — what would kill this

1. **Footprint not bounded.** Max footprint tracks region size instead of budget → something is caching behind Loom (unaligned I/O defeating the bypass, or metadata blow-up).
2. **Any correctness failure.** One mismatched byte in step 5 or 7, or step 8 returning data. No partial credit.
3. **Hot hits are not hits.** Hit latency with a disk-shaped tail while `compressed` climbs means the OS took the hot tier away; the budget is decorative on that machine.
4. **`mmap` wins on everything.** If plain `mmap` beats Loom on throughput *and* p99 *and* its RAM stays within Loom's budget, Loom is a cache in front of a better cache. Observed so far (see PROOF-RUNS.md): `mmap` matches Loom's p50, has a lower p99 because the page cache quietly holds several times Loom's budget, and does fewer ops/s (it faults per 4 KiB page). That is the trade Loom exists to make explicit — but it must keep being measured, not assumed.
5. **Loom overhead is the workload.** A hit costing more than a few µs over a raw memcpy makes Level 1 unusable as a primitive.

## Roadmap (not promises)

- **Level 1** — explicit arena: `alloc`/`read`/`write`/`sync`. **This.**
- **Level 2** — pinned handles: `pin(block) -> &mut [u8]` guards, like a database buffer page, so hot data can be used in place without a copy.
- **Level 3** — allocator: `loom_malloc`/`loom_free` over the frame pool + Rust `GlobalAlloc`. Only makes sense once Level 2 shows the copy is the cost.
- **Level 4** — transparent runtime (`loom run python job.py`). On macOS this is `PROT_NONE` regions plus a Mach exception / `SIGSEGV` handler as a poor man's `userfaultfd`. Public API, slow, unproven. Not promised.
- Bindings (C, Python, Node), prefetch, compression, device profiling (`loom bench --path <device>`), HDD extents, `loom stats`, memory-pressure shrink via `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`, Linux as a peer target.

## Engineering rules this repo holds itself to

No fake metrics. Unknown is `None`, never `0`. No claim stronger than the OS primitive underneath. Hit and miss are never averaged. Corruption is detectable and surfaced. Every `unsafe` block says why it is sound. No silent fallbacks: if `O_DIRECT` is refused, the open fails and says so.

## License

BUSL-1.1 (Licensor: Interchained LLC; Additional Use Grant: None; Change Date 2030-09-11; Change License GPL-3.0-only). Full GPLv3 text in `COPYING-GPL-3.0.txt`. Dependencies keep their own licenses — see `THIRD_PARTY.md`.
