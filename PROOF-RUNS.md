# Recorded proof runs

Every number below is copied verbatim from `loom prove` output. Nothing is rounded.

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

Reading it honestly:

- **Bounded footprint:** max 258.5 MiB against a 256 MiB budget + 1.6 MiB metadata. The 48 MiB slack was not needed; ~1 MiB of it was used.
- **Correctness:** 0 mismatching blocks in 3 × 98,304 blocks (A after eviction+scribble, B after close/reopen), tamper detected on exactly the flipped block, no false positive after restore.
- **Tiering cost:** hit p50 ≤ 47 ns (map lookup + 64 KiB memcpy), miss p50 ≤ 65.5 µs (O_DIRECT read + XXH3 verify). The tier is a tier: three orders of magnitude between hit and miss.
- **vs `pread` (no cache at all):** pread p99 41 µs beats Loom p99 82 µs *on this device* because the virtio disk is fast enough that a 64 KiB direct read costs about what Loom's miss costs, and Loom's p99 lands in its miss tail (hit rate 77 %). On a device with real latency the gap inverts; that must be measured, not asserted. Loom did 52,584 ops/s to pread's 33,648 (1.56x) because 77 % of its reads never touched the device.
- **vs `mmap`:** same-ish p50 (6.1 µs vs Loom's hit path), but p99 **3.15 ms** and 7,395 ops/s vs Loom's 52,584 (7.1x), because mmap faults per 4 KiB page and the kernel's readahead/eviction is not tuned to this access pattern. Its RAM use: **344 MiB of page cache** for a 256 MiB "budget" it never knew about — and that number was still climbing when the run ended. Loom's stayed at 258 MiB.

Not shown by this run: macOS behaviour (F_NOCACHE hint, phys_footprint, compressor), a region larger than a real 8 GiB budget, a real SSD/HDD latency profile, behaviour of any kind under memory pressure. The macOS CI job runs the same proof at test scale; the real-scale macOS run belongs to a machine with the disk in question.
