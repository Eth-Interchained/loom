# Recorded proof runs

Every number below is copied verbatim from `loom prove` output. Nothing is rounded.

## 2026-09-11 — transparent fault-in, proven (Hyperagent sandbox, Linux 6.18, `userfaultfd`)

The mechanism that turns Loom from a library you call into memory a program
just has. A raw `*mut u8` over an arena much larger than the mapped budget,
dereferenced with ordinary loads and stores.

```
$ cargo test --release --lib faultin -- --nocapture

a_hit_costs_nothing_because_no_loom_code_runs
  cold pass 1.774576ms (563.5 MiB/s), warm pass 829ns -> faults unchanged at 16

raw_pointer_reads_are_correct_and_residency_stays_bounded
  load faults: 256  write faults: 0  evictions: 240 (0 needed writeback)
  resident: 16 extents (1.0 MiB)  peak: 1.0 MiB
  load latency: n=256 p50<=81.9us p99<=159.3us max=159.3us mean=75.1us
  wp   latency: n=0 (no samples)

raw_pointer_writes_are_tracked_and_persist
  load faults: 128  write faults: 128  evictions: 120 (120 needed writeback)
  resident: 8 extents (512.0 KiB)  peak: 512.0 KiB
  load latency: n=128 p50<=163.8us p99<=196.6us max=262.7us mean=131.9us
  wp   latency: n=128 p50<=6.1us p99<=9.0us max=9.0us mean=5.8us

4 passed
```

### What this establishes

1. **A 16 MiB arena addressed through a raw pointer with 1 MiB mapped**, every
   byte correct, peak residency exactly the budget. No Loom API calls in the
   test's hot loop — just pointer dereferences.
2. **A hit costs nothing.** The warm pass took **zero additional faults**
   (16 before, 16 after) and ran in 829 us vs 1.77 ms cold. Caveat stated
   plainly: the scan touches 2 bytes per 4 KiB chunk, so the warm pass is
   running out of L2/L3 — the throughput figure is cache speed, not memory
   bandwidth. **The load-bearing evidence is the unchanged fault count**, which
   proves there is no Loom code in the hit path at all.
3. **Dirty tracking is exact.** The read-only pass produced **0 write faults**;
   the write pass produced exactly one WP fault per written extent (128/128)
   and wrote back exactly the dirty ones (120/120). No "assume everything
   touched was written."
4. **A WP fault costs ~6 us** (p50 6.1 us, mean 5.8 us). That is the price of
   exact dirty tracking. Against the iMac's measured 10.49 ms disk read it is
   0.06% — free. Against NVMe at ~15 us it would be ~40% — which is why this
   mechanism suits slow backing storage specifically.
5. **Writeback does not damage neighbours.** The write test stamps 8 bytes into
   each block and then asserts the *other* 65,528 bytes still hold the original
   pattern after eviction and reload.

### Not established

- **macOS.** No `userfaultfd` there; the Mach exception-port equivalent is
  designed but unwritten and unrun. The architecture is identical; only the
  fault transport differs.
- **Injection.** This covers an arena in the *current* process. Getting into a
  program you did not write needs `LD_PRELOAD` / `DYLD_INSERT_LIBRARIES`,
  neither built. On macOS, hardened-runtime binaries refuse it outright.
- **Concurrency.** One handler thread serialises faults. A real ceiling.
- **Permissions.** This kernel reports `vm.unprivileged_userfaultfd = 0`, yet
  the syscall succeeded for this process. Not chased down; on a VPS a
  non-root process may need `sysctl vm.unprivileged_userfaultfd=1`.

## 2026-09-11 — prefetch and zero-copy, measured A/B (Hyperagent sandbox, Linux/ext4)

Added after the iMac run identified queue depth 1 as the ceiling. `--prefetch 0`
vs the default 16, everything else identical.

```
=== --prefetch 0 ===
    prefetch depth in force: 0 blocks (disabled)
[3] wrote pattern A: 6.00 GiB in 10.51s (584.8 MiB/s)
    read A back: 98304 blocks in 10.98s (559.7 MiB/s), 0 mismatching, hits during pass: 0
      prefetch: 0 batches, 0 blocks, 0 later used (n/a — prefetch issued nothing)
    reopened; read B back: 98304 blocks in 10.57s (581.5 MiB/s), 0 mismatching
    loom  20000 ops in 0.42s (47577 ops/s) p50<=6.1us p99<=81.9us max=331.5us hit 70.1%

=== --prefetch 16 ===
    prefetch depth in force: 16 blocks (one 1.0 MiB read per batch)
[3] wrote pattern A: 6.00 GiB in 10.45s (588.1 MiB/s)
    read A back: 98304 blocks in 8.86s (693.5 MiB/s), 0 mismatching, hits during pass: 81918
      prefetch: 5462 batches, 87392 blocks, 81918 later used (93.7% useful)
    reopened; read B back: 98304 blocks in 8.26s (744.1 MiB/s), 0 mismatching
    loom  20000 ops in 0.43s (46546 ops/s) p50<=6.1us p99<=81.9us max=122.5us hit 70.1%
```

|              | off | on | |
|--------------|-----|----|-|
| read A | 559.7 MiB/s | **693.5 MiB/s** | 1.24x |
| read B | 581.5 MiB/s | **744.1 MiB/s** | 1.28x |
| skewed (random) workload | 0.42 s | 0.43 s | unchanged |
| speculation useful | — | **93.7%** | |

- **Device round-trips: 98,304 -> ~16,400** (5,462 batches plus the singles
  that precede run detection). A 6x reduction.
- **Only 1.24x throughput here**, because a miss on this device costs 65 us —
  the round-trip was never the bottleneck. The gain is bounded by what the
  device charges per trip.
- **On the iMac a miss costs 10.49 ms.** Same 6x reduction in trips against a
  per-trip cost ~160x higher. That is a prediction, not a result, and it is
  the next thing to measure on real hardware.
- **The random workload is unchanged**, which is the important negative
  result: run detection (3 consecutive missed blocks) correctly declines to
  speculate on random access, so prefetch costs nothing where it cannot help.

### A defect this A/B found

The first attempt at this measurement reported `5462 batches` for
`--prefetch 0`. `Loom::create` hardcoded `OpenOptions::default()`, so the
freshly created arena always got default prefetch regardless of what the
caller asked for; only the reopened instance honoured the flag. Fixed by
adding `create_with`. A measurement of "feature off" that silently runs the
feature is worse than no measurement.

### And a correctness bug prefetch exposed, found by running it

The sequential-scan test failed on block 110 — `2 + 18x6`, a batch start,
which pointed straight at the claim loop. `FramePool::take_free_or_clean`
returned a frame **without detaching it**, so as the CLOCK hand wrapped
inside a single 16-block claim loop it handed out the same frame twice: two
blocks mapped to one frame, one returning the other's bytes. Silent wrong
data, and it would never have appeared in a single-block miss path.

Fixed at the source — the function now detaches and reports the outgoing
block, making a double hand-out structurally impossible. Verified by
re-introducing the bug: 2 tests fail (the unit property test and the
end-to-end scan), then pass again once restored.

## 2026-09-11 — Mark's iMac (Intel, macOS, APFS, 957 GB internal volume)

The first run on real macOS: `F_NOCACHE`, `F_FULLFSYNC` and
`task_info(TASK_VM_INFO)` all executing for the first time. Device reports
`Solid State: Yes` via `diskutil info /dev/disk2`, but every latency number
below is platter-class, and `diskutil info` on a Fusion Drive reports the
*APFS container* rather than the physical store the data landed on — so the
pool most likely lived on the spinning half. That makes this machine the
literal user in the product thesis.

```
$ ./target/release/loom prove --pool ~/loom.pool --size 8G --budget 256M --region 6G
[0] process footprint before Loom: 352.0 KiB (compressed by OS: 0 B) rss: 680.0 KiB
[1] pool created in 2.37s: 131072 blocks of 64.0 KiB, frames 4096 (256.0 MiB hot),
    metadata 1.6 MiB, on-disk 68.0 KiB, I/O: macOS F_NOCACHE
    footprint bound = 352.0 KiB + 256.0 MiB + 1.6 MiB + 48.0 MiB slack = 306.0 MiB
[2] region 0 allocated: 6.00 GiB = 98304 blocks = 24.0x the hot budget
[3] wrote pattern A: 6.00 GiB in 218.67s (28.1 MiB/s); on-disk now 5.75 GiB
[4] footprint after write A: max 258.1 MiB <= bound 306.0 MiB
[5] evicted all; scribbled 4096 free frames with 0xEE
    read A back: 98304 blocks in 97.64s (62.9 MiB/s), 0 mismatching blocks
[6] miss latency:      n=196608 p50<=327.7us p99<=29.36ms max=4.96s mean=1.53ms
    writeback latency: n=98304  p50<=458.8us p99<=41.94ms max=4.96s mean=2.21ms
[7] sync (writeback + tables + full fsync) took 3.77s; closed
    reopened; read B back: 98304 blocks in 411.81s, 0 mismatching
[4] footprint after read B: max 258.3 MiB <= bound 306.0 MiB  OS compressed 340.0 KiB
[8] flipped one byte in block 49152: Loom returned Corrupt for exactly that block
    restored the byte: block reads clean again (no false positive)
[9] skewed workload: 20000 reads; 90% within the first 3276 blocks (204.8 MiB)
    loom    20000 ops in 129.18s (155 ops/s) p50<=41.0us  p99<=83.89ms max=712.23ms
            hit rate 70.1%  footprint 258.5 MiB (compressed by OS: 95.3 MiB) rss 163.6 MiB
      hit:  n=14022 p50<=127ns   p99<=511ns    max=148.0us  mean=197ns
      miss: n=5979  p50<=14.68ms p99<=201.33ms max=712.22ms mean=21.51ms
    pread   20000 ops in 374.19s (53 ops/s)  p50<=10.49ms p99<=134.22ms max=1.26s
    mmap    20000 ops in 96.89s (206 ops/s)  p50<=7.2us   p99<=50.33ms  max=955.64ms
            rss 321.9 MiB

PROOF PASSED.
```

### What held

- **Bounded footprint:** max 258.5 MiB against a 306 MiB bound, throughout,
  including the skewed workload.
- **Correctness:** 0 mismatching blocks across ~295,000 block verifications —
  pattern A after `evict_all` *and* scribbling every free frame with 0xEE,
  pattern B after a close and reopen.
- **Tamper:** detected on exactly the flipped block; clean again once restored.
  Both halves, including the false-positive check.
- **The tier is a tier.** Hit mean **197 ns**, miss mean **21.51 ms** — five
  orders of magnitude. 14,022 hits cost 2.8 ms in total; 5,979 misses cost
  128.6 s. Essentially all wall time is the cold path, which means Loom's own
  machinery (block map, CLOCK, dirty bits, checksum) is free at this scale.
- **Loom beat raw uncached I/O 2.9x** (129.18 s vs 374.19 s) purely because
  70% of its reads never reached the device. That is the product working.

### What this run exposed

1. **The device is ~10 ms per random 64 KiB read** (`pread` baseline, no Loom
   in the path — so this is the machine, not our code). Sequential reads
   62.9 MiB/s. Both are platter numbers.
2. **Queue depth 1 is our ceiling.** Loom issues one synchronous
   `pread`/`pwrite` at a time, and `F_NOCACHE` removes the kernel readahead
   and writeback batching that would otherwise build queue depth for us.
   Taking residency from the kernel also took on the obligation to manage
   concurrency, and v0 does not. At QD1, throughput is
   `block_size / round-trip`, which is exactly what the numbers show.
3. **Sparse-hole writes cost ~2.4x allocated writes.** Write A into holes ran
   28.1 MiB/s; the closing sync wrote ~256 MiB over allocated extents at
   ~68 MiB/s. `prove` now times the write-B pass explicitly so this ratio is
   measured rather than inferred; on Linux/ext4 the same comparison is
   **1.30x** (635.8 MiB/s sparse vs 826.9 MiB/s allocated). Real on both
   filesystems, worse on APFS. Wants `F_PREALLOCATE` / `fallocate`.
4. **Read B was 4.2x slower than read A over identical offsets** (411.81 s vs
   97.64 s). The only intervening event was pattern B overwriting pattern A.
   On a copy-on-write filesystem an overwrite relocates the extent, so Loom's
   identity mapping (logical block *n* at `data_off + n*bs`) stops
   corresponding to physical locality, and a "sequential" logical pass becomes
   physically random.

   **Cross-filesystem evidence (added after the run):** the same code on
   Linux/ext4 wrote the same 12 GiB and showed **no read regression at all** —
   read A 644.0 MiB/s, read B 644.5 MiB/s. If the cause were device-side
   housekeeping after heavy writes, it would have shown up there too. ext4
   overwrites in place; APFS relocates. That points hard at copy-on-write
   relocation rather than the drive, and it makes physical placement a Loom
   design question rather than a device quirk.
5. **Failure criterion #3 fired partially.** 95.4 MiB of the 258.5 MiB hot
   tier was compressed by the OS (rss 163.6 MiB). Hits stayed fast
   (p99 511 ns) so the tier still functioned, but ~37% of the budget was not
   in DRAM. This is exactly why Loom measures residency instead of claiming
   it — a version that promised residency would have lied here silently.

### Failure criterion #4, scored straight

|          | throughput | p99 | max | RAM |
|----------|-----------|-----|-----|-----|
| mmap | **206 ops/s** | **50.33 ms** | 955.64 ms | 321.9 MiB |
| loom | 155 ops/s | 83.89 ms | **712.23 ms** | **258.5 MiB, declared** |

mmap took throughput and p99. The criterion requires all three of throughput,
p99 *and* staying within Loom's budget; mmap used **321.9 MiB, more than
Loom's entire 306 MiB bound**, and offers no way to cap or even report it. So
the criterion does not fire — but it came within one leg, and that is the
honest state of the performance story: **Loom's differentiator today is
bounded and honest, not fast.** Earning the speed back is prefetch,
coalesced I/O and preallocation, all inside `io.rs` and the miss path.


## 2026-09-11 — Hyperagent sandbox (Linux, Amazon Linux 2023, 2 vCPU, 4 GiB RAM, virtio disk /dev/vdb)

Region (6 GiB) is 1.5x the physical RAM of the machine and 24x the hot budget. Debug-symbol release build. `O_DIRECT` backend.

```
$ loom prove --pool /agent/workspace/loom-sandbox.pool --size 8G --budget 256M --region 6G --ops 50000 --seed 7
LOOM PROVE  pool=/agent/workspace/loom-sandbox.pool  arena=8.00 GiB  budget=256.0 MiB  block=64.0 KiB  region=6.00 GiB  seed=7
[0] process footprint before Loom: 164.0 KiB (compressed: unknown on this platform) file-resident: 2.1 MiB rss: 2.3 MiB [/proc/self/status RssAnon+RssShmem]
[1] pool created in 189.96ms: 131072 blocks of 64.0 KiB, frames 4096 (256.0 MiB hot), metadata 1.6 MiB, on-disk 68.0 KiB , I/O: Linux O_DIRECT
    footprint bound = start 164.0 KiB + hot 256.0 MiB + metadata 1.6 MiB + slack 48.0 MiB = 305.8 MiB
[2] region 0 allocated: 6.00 GiB = 98304 blocks = 24.0x the hot budget
[3] wrote pattern A: 6.00 GiB in 9.53s (644.6 MiB/s); resident 4096 of 4096 frames; on-disk now 5.75 GiB
[4] footprint after write A: max 257.9 MiB ≤ bound 305.8 MiB  (47.9 MiB headroom)
[5] evicted all; scribbled 4096 free frames with 0xEE; resident = 0
    read A back: 98304 blocks in 10.76s (571.0 MiB/s), 0 mismatching blocks, hits during pass: 0
[4] footprint after read A: max 258.0 MiB ≤ bound 305.8 MiB  (47.8 MiB headroom)
[6] Loom internal counters after the sequential passes (0 hits is expected):
    hits: 0  misses: 196608  hit rate: 0.00%
      zero-fills: 0  full-overwrites: 98304  evictions: 192512  writebacks: 98304
      backing read: 6.00 GiB  backing written: 6.00 GiB
      hit latency:       n=0 (no samples)
      miss latency:      n=196608 p50≤65.5µs p99≤131.1µs max=15.60ms mean=59.0µs
      writeback latency: n=98304 p50≤65.5µs p99≤131.1µs max=15.60ms mean=74.8µs
[7] wrote pattern B, sync (writeback + tables + full fsync) took 0.22s; closed
    reopened; read B back: 98304 blocks in 9.71s, 0 mismatching
[4] footprint after read B: max 258.0 MiB ≤ bound 305.8 MiB  (47.8 MiB headroom)
[8] flipped one byte in block 49152 on disk: Loom returned Corrupt for exactly that block
    restored the byte: block reads clean again (no false positive)
[9] skewed workload: 50000 reads of 64.0 KiB blocks; 90% within the first 3276 blocks (204.8 MiB), 10% uniform over 6.00 GiB
    loom       50000 ops in 0.95s (52584 ops/s)  p50≤6.1µs p99≤81.9µs max=7.81ms  hit rate 77.3%  footprint after: 258.5 MiB (compressed: unknown on this platform) file-resident: 2.1 MiB rss: 260.6 MiB [/proc/self/status RssAnon+RssShmem]
    Loom internal latency across the whole run, hit and miss kept separate:
      hit:  n=38631 p50≤47ns p99≤191ns max=699ns mean=47ns
      miss: n=11370 p50≤65.5µs p99≤98.3µs max=7.81ms mean=62.9µs
[4] footprint after skew workload: max 258.5 MiB ≤ bound 305.8 MiB  (47.3 MiB headroom)
    pread      50000 ops in 1.49s (33648 ops/s)  p50≤32.8µs p99≤41.0µs max=805.0µs  footprint after: 1.8 MiB (compressed: unknown on this platform) file-resident: 2.1 MiB rss: 4.0 MiB [/proc/self/status RssAnon+RssShmem]  (every read goes to the device; no cache)
    mmap       50000 ops in 6.76s (7395 ops/s)  p50≤6.1µs p99≤3.15ms max=9.18ms  footprint after: 1.8 MiB (compressed: unknown on this platform) file-resident: 344.3 MiB rss: 346.2 MiB [/proc/self/status RssAnon+RssShmem]  (kernel decides residency; RAM use is whatever the page cache took)

PROOF PASSED. Loom created a logical arena region of 6.00 GiB over a hot budget of 256.0 MiB (24.0x larger), kept the process footprint at or below 305.8 MiB (max observed 258.5 MiB), evicted and promoted every block correctly (patterns A and B verified byte-for-byte, B across a close/reopen), detected a single flipped byte on disk and cleared when it was restored, and measured the cost: hit p50≤47ns / miss p50≤65.5µs; skewed workload p99: loom≤81.9µs vs pread≤41.0µs vs mmap≤3.15ms.

real	0m48.426s
user	0m19.856s
sys	0m9.845s
```

Re-run on 2026-09-11 with the write-B timing and progress output added
(same machine, same parameters, `--ops 50000`):

```
[3] wrote pattern A: 6.00 GiB in 9.66s (635.8 MiB/s)
    read A back: 98304 blocks in ~9.5s (~644.0 MiB/s), 0 mismatching
[7] wrote pattern B over the allocated region: 6.00 GiB in 7.43s (826.9 MiB/s)
    vs write A into sparse holes 9.66s (635.8 MiB/s) = 1.30x
    reopened; read B back: 98304 blocks in 9.53s (644.5 MiB/s), 0 mismatching
```

Two things this pins down: the sparse-allocation write penalty is **1.30x on
ext4** (vs ~2.4x on APFS), and **read B does not regress on ext4** — which is
the control that implicates copy-on-write relocation on APFS rather than the
drive.

Reading it honestly:

- **Bounded footprint:** max 258.5 MiB against a 256 MiB budget + 1.6 MiB metadata. The 48 MiB slack was not needed; ~1 MiB of it was used.
- **Correctness:** 0 mismatching blocks in 3 × 98,304 blocks (A after eviction+scribble, B after close/reopen), tamper detected on exactly the flipped block, no false positive after restore.
- **Tiering cost:** hit p50 ≤ 47 ns (map lookup + 64 KiB memcpy), miss p50 ≤ 65.5 µs (O_DIRECT read + XXH3 verify). The tier is a tier: three orders of magnitude between hit and miss.
- **vs `pread` (no cache at all):** pread p99 41 µs beats Loom p99 82 µs *on this device* because the virtio disk is fast enough that a 64 KiB direct read costs about what Loom's miss costs, and Loom's p99 lands in its miss tail (hit rate 77 %). On a device with real latency the gap inverts; that must be measured, not asserted. Loom did 52,584 ops/s to pread's 33,648 (1.56x) because 77 % of its reads never touched the device.
- **vs `mmap`:** same-ish p50 (6.1 µs vs Loom's hit path), but p99 **3.15 ms** and 7,395 ops/s vs Loom's 52,584 (7.1x), because mmap faults per 4 KiB page and the kernel's readahead/eviction is not tuned to this access pattern. Its RAM use: **344 MiB of page cache** for a 256 MiB "budget" it never knew about — and that number was still climbing when the run ended. Loom's stayed at 258 MiB.

Not shown by this run: macOS behaviour (F_NOCACHE hint, phys_footprint, compressor), a region larger than a real 8 GiB budget, a real SSD/HDD latency profile, behaviour of any kind under memory pressure. The macOS CI job runs the same proof at test scale; the real-scale macOS run belongs to a machine with the disk in question.
