# AGENTS.md — how to build, test and extend Loom without reading prose

## Build / test (deterministic, no network after the first `cargo fetch`)

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test                      # unit tests + the proof at test scale (~2 s)
cargo test --release            # same, optimized (latency numbers meaningful)
cargo build --release && ./target/release/loom --help
```

Cross-check the macOS backend from Linux without a Mac:

```sh
rustup target add x86_64-apple-darwin aarch64-apple-darwin
cargo check --all-targets --target x86_64-apple-darwin
cargo check --all-targets --target aarch64-apple-darwin
```

Real-device proof (exit 0 pass / 1 fail; last line is the verdict):

```sh
./target/release/loom prove --pool <path on the device> --size 8G --budget 256M --region 6G
```

## Layout — every module exists because the proof needs it

| Path | Owns |
|---|---|
| `src/aligned.rs` | 4 KiB-aligned buffers; alignment enforced by type (cache bypass depends on it) |
| `src/io.rs` | the backing file: `open` (+`F_NOCACHE` / `O_DIRECT`), `pread_exact`, `pwrite_all`, `set_len`, `sizes`, `sync` (`F_FULLFSYNC` / `fdatasync`) |
| `src/superblock.rs` | on-disk layout: superblock, region table, checksum table, identity block mapping |
| `src/frames.rs` | the hot tier: fixed frame pool, CLOCK victim selection, dirty/ref bits, `scribble_free` proof hook |
| `src/checksum.rs` | XXH3-64 per block; `0` = never written |
| `src/arena.rs` | `Loom`: create/open, `alloc`, `read`, `write`, `with_slice`(`_mut`) zero-copy borrows, `sync`, `evict_all`, `close`; the miss path (victim → writeback → load → verify) and the prefetch batcher |
| `src/footprint.rs` | the budget's witness: `phys_footprint`/`compressed`/`resident_size` (macOS), `RssAnon+RssShmem`/`RssFile`/`VmRSS` (Linux) |
| `src/stats.rs` | counters + log-scale latency histograms; hit and miss never merged |
| `src/pattern.rs` | deterministic block content for proofs (regenerated, never stored) |
| `src/faultin.rs` | **Linux only.** Transparent fault-in over `userfaultfd` (MISSING\|WP): raw pointer, handler thread, exact dirty tracking, bounded residency |
| `src/prove.rs` | the experiment; `run(&ProveConfig, &mut dyn Write) -> Report` |
| `src/bin/loom.rs` | `init` / `info` / `prove`; hand-parsed args |
| `tests/prove.rs` | the experiment at test scale (256 MiB arena / 8 MiB budget) |

## Invariants (a PR that breaks one is wrong even if tests pass)

1. Arena bytes live in RAM **only** inside `FramePool`. No other struct may hold block data.
2. Every byte to/from the backing file goes through an `AlignedBuf` at a block-aligned offset.
3. Hit and miss latencies are recorded to separate histograms. Never average them.
4. A checksum mismatch on load returns `LoomError::Corrupt { block, .. }` and leaves the frame free and zeroed. Never return the bytes.
5. `footprint::current()` returns `None` for a field the platform cannot report. Never `0`.
6. No fallback from cache-bypassing I/O to cached I/O. Refuse with the errno.
7. Every `unsafe` block has a `// SAFETY:` comment naming why it is sound.
8. `prove` fails on the first false claim; the verdict sentence contains only measured values.
9. `FramePool::take_free_or_clean` **detaches** the frame before returning and reports the outgoing block; the caller must clear that block's map entry. Not tidiness — a caller claiming several frames in a loop would otherwise be handed the same still-attached frame twice as the CLOCK hand wraps, mapping two blocks to one frame and silently returning the wrong bytes. Regression-tested at both the unit and scan level.
10. Speculative (prefetch) loads may never write: free or clean frames only, never clobber a resident block, zero-fill a never-written block rather than trusting the disk, and never surface a checksum error for a block the caller did not ask for.
11. An unresolvable fault leaves the faulting thread **blocked** and records the address and reason in `FaultStats::failures`. Waking it onto bytes we failed to load would hand it silent garbage; a hang that says why beats corruption that doesn't.
12. `FaultArena::drop` flushes dirty extents **before** stopping the handler and unmapping, and joins the handler before `munmap` — unmapping under a live handler would be a use-after-free of the mapping.
13. An option a caller passes must reach the object they get. `Loom::create` delegates to `create_with`; substituting defaults on the create path made a "prefetch disabled" measurement report prefetch running.

## Extending

- New residency policy → replace `choose_victim` in `frames.rs`; nothing else changes.
- New backend (e.g. Linux `io_uring`) → new `cfg` arm in `io.rs`; keep the `Backing` API.
- New on-disk fields → bump `FORMAT_VERSION` in `superblock.rs`; `decode` must reject older/newer.
- Level 2 (`pin` handles) belongs in `arena.rs` as a guard type that increments a pin count `choose_victim` must respect.
- macOS fault-in: add a `faultin_mach.rs` with the same `FaultArena` surface. Save and CHAIN the previous exception ports (`task_get_exception_ports` first) — swallowing another subsystem's `EXC_BAD_ACCESS` silently breaks crash reporting and `lldb` for the whole process.
- `userfaultfd` needs `CAP_SYS_PTRACE` when `vm.unprivileged_userfaultfd=0` (the default on many kernels, including this sandbox's 6.18). Deployment detail, not a code one.

## Release

Single crate `loom-arena` (binary `loom`, library `loom`). Bump `Cargo.toml` version; tag `v*`. No registry publish is configured yet — deliberately.
