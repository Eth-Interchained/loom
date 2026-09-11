//! The first experiment.
//!
//! `run` executes the proof described in the README, step by step, against a
//! real pool file on a real device, and returns a report in which every
//! number is a measurement. It fails on the first false claim. It never
//! rounds up.
//!
//! The same code runs at sandbox scale (a few GiB) and at the real scale
//! (128 GiB arena / 8 GiB budget); only the parameters change.

use crate::aligned::AlignedBuf;
use crate::arena::{CreateOptions, Loom, OpenOptions};
use crate::error::{LoomError, Result};
use crate::footprint::{self, Footprint};
use crate::io::Backing;
use crate::pattern;
use crate::stats::{fmt_bytes, fmt_ns, LatencyHist};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ProveConfig {
    pub pool: PathBuf,
    pub size: u64,
    pub budget: u64,
    pub block_size: u32,
    pub region: u64,
    pub seed: u64,
    /// Operations in the skewed-access workload.
    pub ops: u64,
    pub baselines: bool,
    pub keep_pool: bool,
    /// Allow region < 4x budget (the run is then not a meaningful proof).
    pub allow_small: bool,
}

/// Fixed allowance for everything in the process that is not Loom's frames
/// or metadata: the binary, stacks, the two scratch buffers, histograms,
/// allocator slack. Generous on purpose; it is stated in the report.
pub const PROCESS_SLACK: u64 = 48 << 20;

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub passed: bool,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    pub footprint_start: Option<Footprint>,
    pub footprint_max: Option<Footprint>,
    pub footprint_bound: u64,
    pub hot_bytes: u64,
    pub metadata_bytes: u64,
    pub region_bytes: u64,
    pub write_a_secs: f64,
    pub read_a_secs: f64,
    pub disk_allocated_after_a: u64,
    pub write_b_secs: f64,
    pub sync_secs: f64,
    pub read_b_secs: f64,
    pub tamper_detected: bool,
    pub tamper_restored_ok: bool,
    pub hit_p50_ns: Option<u64>,
    pub hit_p99_ns: Option<u64>,
    pub miss_p50_ns: Option<u64>,
    pub miss_p99_ns: Option<u64>,
    pub skew_loom: Option<WorkloadResult>,
    pub skew_pread: Option<WorkloadResult>,
    pub skew_mmap: Option<WorkloadResult>,
}

#[derive(Debug, Clone)]
pub struct WorkloadResult {
    pub label: String,
    pub ops: u64,
    pub secs: f64,
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    pub hit_rate: Option<f64>,
    pub footprint_after: Option<Footprint>,
}

impl WorkloadResult {
    pub fn render(&self) -> String {
        format!(
            "{:<10} {} ops in {:.2}s ({:.0} ops/s)  p50≤{} p99≤{} max={}{}{}",
            self.label,
            self.ops,
            self.secs,
            self.ops as f64 / self.secs.max(1e-9),
            fmt_ns(self.p50_ns),
            fmt_ns(self.p99_ns),
            fmt_ns(self.max_ns),
            match self.hit_rate {
                Some(h) => format!("  hit rate {:.1}%", h * 100.0),
                None => String::new(),
            },
            match self.footprint_after {
                Some(f) => format!("  footprint after: {f}"),
                None => String::new(),
            }
        )
    }
}

struct Ctx<'a> {
    out: &'a mut dyn Write,
    report: Report,
    fp_max: Option<Footprint>,
    /// Start of the pass currently in progress, and when we last reported it.
    pass_started: Option<Instant>,
    last_progress: Option<Instant>,
}

/// How often a long pass reports progress. A pass over a large region on a
/// slow device takes minutes; with no output at all, slow is
/// indistinguishable from hung — which cost a real debugging round on the
/// first hardware run.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

impl<'a> Ctx<'a> {
    fn say(&mut self, s: impl AsRef<str>) {
        let _ = writeln!(self.out, "{}", s.as_ref());
    }

    /// Mark the start of a long pass, so `progress` has something to measure
    /// against.
    fn begin_pass(&mut self) {
        let now = Instant::now();
        self.pass_started = Some(now);
        self.last_progress = Some(now);
    }

    /// Report progress at most every [`PROGRESS_INTERVAL`]. `done`/`total` are
    /// blocks; the rate is derived from bytes actually moved so far.
    fn progress(&mut self, label: &str, done: u64, total: u64, block_size: u64) {
        let (Some(started), Some(last)) = (self.pass_started, self.last_progress) else {
            return;
        };
        let now = Instant::now();
        if now.duration_since(last) < PROGRESS_INTERVAL {
            return;
        }
        self.last_progress = Some(now);
        let secs = now.duration_since(started).as_secs_f64();
        let bytes = done * block_size;
        let rate = bytes as f64 / secs.max(1e-9);
        let eta = if done > 0 {
            let remaining = (total - done) as f64 * (secs / done as f64);
            format!("  eta {:.0}s", remaining)
        } else {
            String::new()
        };
        let _ = writeln!(
            self.out,
            "      … {label}: {done}/{total} blocks ({:.0}%), {}/s, {:.0}s elapsed{eta}",
            (done as f64 / total as f64) * 100.0,
            fmt_bytes(rate as u64),
            secs
        );
        let _ = self.out.flush();
    }
    fn fail(&mut self, s: impl Into<String>) {
        let s = s.into();
        let _ = writeln!(self.out, "  FAIL: {s}");
        self.report.failures.push(s);
    }
    fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        let _ = writeln!(self.out, "  note: {s}");
        self.report.notes.push(s);
    }
    fn sample_footprint(&mut self) {
        if let Ok(f) = footprint::current() {
            match self.fp_max {
                Some(m) if m.footprint >= f.footprint => {}
                _ => self.fp_max = Some(f),
            }
        }
    }
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Deterministic skewed block sequence: `hot_frac` of accesses land in the
/// first `hot_blocks` blocks, the rest uniformly over `total_blocks`.
fn skew_sequence(seed: u64, ops: u64, hot_blocks: u64, total_blocks: u64) -> Vec<u64> {
    let mut st = seed ^ 0x9E37_79B9_7F4A_7C15 | 1;
    (0..ops)
        .map(|_| {
            let r = xorshift(&mut st);
            if r % 100 < 90 {
                (r >> 8) % hot_blocks
            } else {
                (r >> 8) % total_blocks
            }
        })
        .collect()
}

pub fn run(cfg: &ProveConfig, out: &mut dyn Write) -> Report {
    let mut cx = Ctx {
        out,
        report: Report::default(),
        fp_max: None,
        pass_started: None,
        last_progress: None,
    };
    match run_inner(cfg, &mut cx) {
        Ok(()) => {}
        Err(e) => cx.fail(format!("aborted: {e}")),
    }
    let mut report = cx.report;
    report.footprint_max = cx.fp_max;
    report.passed = report.failures.is_empty();
    report
}

fn run_inner(cfg: &ProveConfig, cx: &mut Ctx) -> Result<()> {
    let bs = cfg.block_size as u64;
    if cfg.region % bs != 0 {
        return Err(LoomError::Invalid(format!(
            "region {} must be a multiple of block size {}",
            cfg.region, bs
        )));
    }
    let region_blocks = cfg.region / bs;
    cx.report.region_bytes = cfg.region;

    cx.say(format!(
        "LOOM PROVE  pool={}  arena={}  budget={}  block={}  region={}  seed={}",
        cfg.pool.display(),
        fmt_bytes(cfg.size),
        fmt_bytes(cfg.budget),
        fmt_bytes(bs),
        fmt_bytes(cfg.region),
        cfg.seed
    ));
    if cfg.region < 4 * cfg.budget {
        if cfg.allow_small {
            cx.note(
                "region < 4x budget: this run exercises the code, it does not prove the thesis",
            );
        } else {
            return Err(LoomError::Invalid(format!(
                "region {} is less than 4x budget {}; pass --allow-small to run anyway",
                fmt_bytes(cfg.region),
                fmt_bytes(cfg.budget)
            )));
        }
    }

    // Step 0: witness baseline before Loom allocates anything.
    let fp0 = match footprint::current() {
        Ok(f) => f,
        Err(e) => {
            cx.fail(format!(
                "cannot measure process footprint: {e} — the budget has no witness"
            ));
            return Ok(());
        }
    };
    cx.report.footprint_start = Some(fp0);
    cx.say(format!("[0] process footprint before Loom: {fp0}"));

    // Step 1: create sparse pool.
    if cfg.pool.exists() {
        return Err(LoomError::Invalid(format!(
            "{} already exists; refusing to overwrite",
            cfg.pool.display()
        )));
    }
    let t = Instant::now();
    let mut loom = Loom::create(
        &cfg.pool,
        CreateOptions {
            capacity: cfg.size,
            block_size: cfg.block_size,
            budget: cfg.budget,
        },
    )?;
    let info = loom.info()?;
    cx.report.hot_bytes = info.hot_bytes_allocated;
    cx.report.metadata_bytes = info.metadata_bytes;
    cx.say(format!(
        "[1] pool created in {:.2?}: {} blocks of {}, frames {} ({} hot), metadata {}, on-disk {} , I/O: {}",
        t.elapsed(),
        info.nblocks,
        fmt_bytes(bs),
        info.frame_count,
        fmt_bytes(info.hot_bytes_allocated),
        fmt_bytes(info.metadata_bytes),
        fmt_bytes(info.disk_allocated),
        info.cache_mode
    ));
    if info.disk_allocated > 16 << 20 {
        cx.fail(format!(
            "fresh pool occupies {} on disk; expected a sparse file",
            fmt_bytes(info.disk_allocated)
        ));
    }
    let bound = fp0.footprint + info.hot_bytes_allocated + info.metadata_bytes + PROCESS_SLACK;
    cx.report.footprint_bound = bound;
    cx.say(format!(
        "    footprint bound = start {} + hot {} + metadata {} + slack {} = {}",
        fmt_bytes(fp0.footprint),
        fmt_bytes(info.hot_bytes_allocated),
        fmt_bytes(info.metadata_bytes),
        fmt_bytes(PROCESS_SLACK),
        fmt_bytes(bound)
    ));

    // Step 2: allocate the region.
    let region = loom.alloc(cfg.region)?;
    cx.say(format!(
        "[2] region {} allocated: {} = {} blocks = {:.1}x the hot budget",
        region.id,
        fmt_bytes(region.len),
        region_blocks,
        cfg.region as f64 / cfg.budget as f64
    ));

    let mut scratch = AlignedBuf::zeroed(bs as usize);
    let mut readback = AlignedBuf::zeroed(bs as usize);

    // Step 3: write pattern A.
    let t = Instant::now();
    cx.begin_pass();
    for b in 0..region_blocks {
        pattern::fill(cfg.seed, b'A', b, &mut scratch);
        loom.write(region, b * bs, &scratch)?;
        if b % 256 == 0 {
            cx.sample_footprint();
            cx.progress("write A", b, region_blocks, bs);
        }
    }
    cx.sample_footprint();
    cx.report.write_a_secs = t.elapsed().as_secs_f64();
    let after_a = loom.info()?;
    cx.report.disk_allocated_after_a = after_a.disk_allocated;
    cx.say(format!(
        "[3] wrote pattern A: {} in {:.2}s ({}/s); resident {} of {} frames; on-disk now {}",
        fmt_bytes(cfg.region),
        cx.report.write_a_secs,
        fmt_bytes((cfg.region as f64 / cx.report.write_a_secs) as u64),
        loom.resident_blocks(),
        info.frame_count,
        fmt_bytes(after_a.disk_allocated)
    ));
    if loom.resident_blocks() > info.frame_count {
        cx.fail("resident blocks exceed frame count (impossible; accounting bug)");
    }
    // Data must have gone to disk: at least everything that cannot still be in frames.
    let must_be_on_disk = cfg.region.saturating_sub(info.hot_bytes_allocated);
    if after_a.disk_allocated < must_be_on_disk {
        cx.fail(format!(
            "only {} on disk after writing {} with {} hot; evicted data is not on the backing store",
            fmt_bytes(after_a.disk_allocated),
            fmt_bytes(cfg.region),
            fmt_bytes(info.hot_bytes_allocated)
        ));
    }

    // Step 4: bounded footprint (checked continuously; reported here).
    check_bound(cx, "after write A");

    // Step 5: evict everything, destroy stale frame memory, read A back.
    loom.evict_all()?;
    let scribbled = loom.debug_scribble_free_frames(0xEE);
    if loom.resident_blocks() != 0 {
        cx.fail(format!(
            "{} blocks still resident after evict_all",
            loom.resident_blocks()
        ));
    }
    cx.say(format!(
        "[5] evicted all; scribbled {scribbled} free frames with 0xEE; resident = {}",
        loom.resident_blocks()
    ));
    let hits_before = loom.stats().hits;
    let t = Instant::now();
    cx.begin_pass();
    let mut mismatches = 0u64;
    let mut first_bad: Option<(u64, usize)> = None;
    for b in 0..region_blocks {
        pattern::fill(cfg.seed, b'A', b, &mut scratch);
        loom.read(region, b * bs, &mut readback)?;
        if let Some(pos) = pattern::first_mismatch(&scratch, &readback) {
            mismatches += 1;
            if first_bad.is_none() {
                first_bad = Some((b, pos));
            }
        }
        if b % 256 == 0 {
            cx.sample_footprint();
            cx.progress("read A", b, region_blocks, bs);
        }
    }
    cx.sample_footprint();
    cx.report.read_a_secs = t.elapsed().as_secs_f64();
    cx.say(format!(
        "    read A back: {} blocks in {:.2}s ({}/s), {} mismatching blocks, hits during pass: {}",
        region_blocks,
        cx.report.read_a_secs,
        fmt_bytes((cfg.region as f64 / cx.report.read_a_secs) as u64),
        mismatches,
        loom.stats().hits - hits_before
    ));
    if mismatches > 0 {
        let (b, pos) = first_bad.unwrap();
        cx.fail(format!(
            "pattern A corrupted after eviction: {mismatches} blocks wrong, first at block {b} byte {pos}"
        ));
    }
    check_bound(cx, "after read A");

    // Step 6: Loom's own counters so far. The sequential passes are 100%
    // misses by construction (every block touched exactly once), so the hit
    // histogram is empty here; the hit/miss split is captured after the
    // skewed workload in step 9.
    let st = loom.stats().clone();
    cx.say("[6] Loom internal counters after the sequential passes (0 hits is expected):");
    cx.say(format!(
        "    {}",
        st.render().trim_end().replace('\n', "\n    ")
    ));

    // Step 7: write B, sync, close, reopen, verify B.
    let t = Instant::now();
    cx.begin_pass();
    for b in 0..region_blocks {
        pattern::fill(cfg.seed, b'B', b, &mut scratch);
        loom.write(region, b * bs, &scratch)?;
        if b % 256 == 0 {
            cx.sample_footprint();
            cx.progress("write B", b, region_blocks, bs);
        }
    }
    cx.report.write_b_secs = t.elapsed().as_secs_f64();
    let t = Instant::now();
    loom.sync()?;
    cx.report.sync_secs = t.elapsed().as_secs_f64();
    let tamper_block = loom.block_of_region_offset(region, (region_blocks / 2) * bs);
    let tamper_off = loom.backing_offset_of_block(tamper_block);
    loom.close()?;
    // Write A landed in sparse holes (the filesystem had to allocate an
    // extent per block); write B overwrote the same, now-allocated region.
    // Identical work otherwise, so the ratio isolates the cost of allocation
    // — on a copy-on-write filesystem it also exposes whether "overwrite"
    // really means overwrite.
    cx.say(format!(
        "[7] wrote pattern B over the allocated region: {} in {:.2}s ({}/s) vs write A into sparse holes {:.2}s ({}/s) = {:.2}x",
        fmt_bytes(cfg.region),
        cx.report.write_b_secs,
        fmt_bytes((cfg.region as f64 / cx.report.write_b_secs.max(1e-9)) as u64),
        cx.report.write_a_secs,
        fmt_bytes((cfg.region as f64 / cx.report.write_a_secs.max(1e-9)) as u64),
        cx.report.write_a_secs / cx.report.write_b_secs.max(1e-9)
    ));
    cx.say(format!(
        "    sync (writeback + tables + full fsync) took {:.2}s; closed",
        cx.report.sync_secs
    ));
    let mut loom = Loom::open(
        &cfg.pool,
        OpenOptions {
            budget: Some(cfg.budget),
        },
    )?;
    let regs = loom.regions();
    let region = match regs.first() {
        Some(r) if *r == region => *r,
        other => {
            cx.fail(format!(
                "region table after reopen: {other:?}, expected {region:?}"
            ));
            return Ok(());
        }
    };
    let t = Instant::now();
    cx.begin_pass();
    let mut mismatches = 0u64;
    let mut first_bad = None;
    for b in 0..region_blocks {
        pattern::fill(cfg.seed, b'B', b, &mut scratch);
        loom.read(region, b * bs, &mut readback)?;
        if let Some(pos) = pattern::first_mismatch(&scratch, &readback) {
            mismatches += 1;
            if first_bad.is_none() {
                first_bad = Some((b, pos));
            }
        }
        if b % 256 == 0 {
            cx.sample_footprint();
            cx.progress("read B", b, region_blocks, bs);
        }
    }
    cx.report.read_b_secs = t.elapsed().as_secs_f64();
    cx.say(format!(
        "    reopened; read B back: {} blocks in {:.2}s ({}/s), {} mismatching",
        region_blocks,
        cx.report.read_b_secs,
        fmt_bytes((cfg.region as f64 / cx.report.read_b_secs.max(1e-9)) as u64),
        mismatches
    ));
    // Read A and read B do identical work over identical offsets. A large
    // gap means the pool's physical layout changed under us between them —
    // on a copy-on-write filesystem, rewriting a block relocates its extent,
    // so Loom's identity mapping (logical block n at data_off + n*bs) stops
    // corresponding to physical locality. Surfaced, not averaged away.
    if cx.report.read_a_secs > 0.0 {
        let ratio = cx.report.read_b_secs / cx.report.read_a_secs;
        if ratio >= 1.5 || ratio <= 0.67 {
            cx.note(format!(
                "read B is {:.2}x read A over the same offsets ({:.2}s vs {:.2}s) — the pool's physical layout changed between the passes (copy-on-write relocation on rewrite, or device-side housekeeping). Loom's logical order no longer matches physical order.",
                ratio, cx.report.read_b_secs, cx.report.read_a_secs
            ));
        }
    }
    if mismatches > 0 {
        let (b, pos) = first_bad.unwrap();
        cx.fail(format!(
            "pattern B lost across reopen: {mismatches} blocks wrong, first at block {b} byte {pos}"
        ));
    }
    check_bound(cx, "after read B");
    loom.close()?;

    // Step 8: tamper — flip one byte on disk; detection AND false-positive check.
    tamper_byte(&cfg.pool, tamper_off + 4321, |b| b ^ 0x40)?;
    {
        let mut loom = Loom::open(
            &cfg.pool,
            OpenOptions {
                budget: Some(cfg.budget),
            },
        )?;
        match loom.read(region, (region_blocks / 2) * bs, &mut readback) {
            Err(LoomError::Corrupt { block, .. }) if block == tamper_block => {
                cx.report.tamper_detected = true;
                cx.say(format!(
                    "[8] flipped one byte in block {tamper_block} on disk: Loom returned Corrupt for exactly that block"
                ));
            }
            Err(e) => cx.fail(format!("tamper produced the wrong error: {e}")),
            Ok(()) => cx.fail(format!(
                "tampered block {tamper_block} read back WITHOUT error — corruption undetected"
            )),
        }
        loom.close()?;
    }
    tamper_byte(&cfg.pool, tamper_off + 4321, |b| b ^ 0x40)?;
    {
        let mut loom = Loom::open(
            &cfg.pool,
            OpenOptions {
                budget: Some(cfg.budget),
            },
        )?;
        pattern::fill(cfg.seed, b'B', region_blocks / 2, &mut scratch);
        match loom.read(region, (region_blocks / 2) * bs, &mut readback) {
            Ok(()) if pattern::first_mismatch(&scratch, &readback).is_none() => {
                cx.report.tamper_restored_ok = true;
                cx.say("    restored the byte: block reads clean again (no false positive)");
            }
            Ok(()) => cx.fail("restored block read without error but with wrong bytes"),
            Err(e) => cx.fail(format!("restored block still errors: {e}")),
        }

        // Step 9: skewed workload — Loom, then baselines on the same file.
        let total_blocks = region_blocks;
        let hot_blocks = ((loom.frame_count() as u64) * 8 / 10).clamp(1, total_blocks);
        let seq = skew_sequence(cfg.seed, cfg.ops, hot_blocks, total_blocks);
        cx.say(format!(
            "[9] skewed workload: {} reads of {} blocks; 90% within the first {} blocks ({}), 10% uniform over {}",
            cfg.ops,
            fmt_bytes(bs),
            hot_blocks,
            fmt_bytes(hot_blocks * bs),
            fmt_bytes(cfg.region)
        ));
        loom.evict_all()?; // start cold, like the baselines
        let base_hits = loom.stats().hits;
        let base_miss = loom.stats().misses;
        let mut hist = LatencyHist::new();
        let t = Instant::now();
        cx.begin_pass();
        for (i, &b) in seq.iter().enumerate() {
            let t1 = Instant::now();
            loom.read(region, b * bs, &mut readback)?;
            hist.record(t1.elapsed());
            if i % 256 == 0 {
                cx.progress("skew (loom)", i as u64, cfg.ops, bs);
            }
        }
        let secs = t.elapsed().as_secs_f64();
        cx.sample_footprint();
        let h = loom.stats().hits - base_hits;
        let m = loom.stats().misses - base_miss;
        let wr = WorkloadResult {
            label: "loom".into(),
            ops: cfg.ops,
            secs,
            p50_ns: hist.percentile_ns(0.5).unwrap_or(0),
            p99_ns: hist.percentile_ns(0.99).unwrap_or(0),
            max_ns: hist.max_ns(),
            hit_rate: if h + m > 0 {
                Some(h as f64 / (h + m) as f64)
            } else {
                None
            },
            footprint_after: footprint::current().ok(),
        };
        cx.say(format!("    {}", wr.render()));
        cx.report.skew_loom = Some(wr);
        let st = loom.stats().clone();
        cx.report.hit_p50_ns = st.hit_latency.percentile_ns(0.5);
        cx.report.hit_p99_ns = st.hit_latency.percentile_ns(0.99);
        cx.report.miss_p50_ns = st.miss_latency.percentile_ns(0.5);
        cx.report.miss_p99_ns = st.miss_latency.percentile_ns(0.99);
        cx.say("    Loom internal latency across the whole run, hit and miss kept separate:");
        cx.say(format!("      hit:  {}", st.hit_latency.summary()));
        cx.say(format!("      miss: {}", st.miss_latency.summary()));
        check_bound(cx, "after skew workload");
        let offsets: Vec<u64> = seq
            .iter()
            .map(|&b| loom.backing_offset_of_block(loom.block_of_region_offset(region, b * bs)))
            .collect();
        let data_end = loom
            .backing_offset_of_block(loom.block_of_region_offset(region, (total_blocks - 1) * bs))
            + bs;
        loom.close()?;

        if cfg.baselines {
            // Baseline 1: raw cache-bypassing pread of the same blocks.
            let backing = Backing::open(&cfg.pool, false)?;
            let mut hist = LatencyHist::new();
            let t = Instant::now();
            cx.begin_pass();
            for (i, &off) in offsets.iter().enumerate() {
                let t1 = Instant::now();
                backing.pread_exact(&mut readback, off)?;
                hist.record(t1.elapsed());
                if i % 256 == 0 {
                    cx.progress("skew (pread)", i as u64, cfg.ops, bs);
                }
            }
            let wr = WorkloadResult {
                label: "pread".into(),
                ops: cfg.ops,
                secs: t.elapsed().as_secs_f64(),
                p50_ns: hist.percentile_ns(0.5).unwrap_or(0),
                p99_ns: hist.percentile_ns(0.99).unwrap_or(0),
                max_ns: hist.max_ns(),
                hit_rate: None,
                footprint_after: footprint::current().ok(),
            };
            cx.say(format!(
                "    {}  (every read goes to the device; no cache)",
                wr.render()
            ));
            cx.report.skew_pread = Some(wr);
            drop(backing);

            // Baseline 2: plain mmap of the pool file; the kernel page cache is the "hot tier".
            match mmap_workload(&cfg.pool, data_end, &offsets, bs as usize, &mut readback) {
                Ok(wr) => {
                    cx.say(format!(
                        "    {}  (kernel decides residency; RAM use is whatever the page cache took)",
                        wr.render()
                    ));
                    cx.report.skew_mmap = Some(wr);
                }
                Err(e) => cx.note(format!("mmap baseline skipped: {e}")),
            }
        }
    }

    if !cfg.keep_pool {
        std::fs::remove_file(&cfg.pool).map_err(|e| LoomError::Io {
            op: "remove_file",
            ctx: cfg.pool.display().to_string(),
            source: e,
        })?;
    }
    Ok(())
}

fn check_bound(cx: &mut Ctx, when: &str) {
    if let Some(m) = cx.fp_max {
        let bound = cx.report.footprint_bound;
        if m.footprint > bound {
            cx.fail(format!(
                "footprint exceeded bound {when}: max {} > bound {} (Loom holds arena bytes it did not account for, or the slack is wrong)",
                fmt_bytes(m.footprint),
                fmt_bytes(bound)
            ));
        } else {
            cx.say(format!(
                "[4] footprint {when}: max {} ≤ bound {}  ({} headroom){}",
                fmt_bytes(m.footprint),
                fmt_bytes(bound),
                fmt_bytes(bound - m.footprint),
                match m.compressed {
                    Some(c) if c > 0 =>
                        format!("  OS has compressed {} of this process", fmt_bytes(c)),
                    _ => String::new(),
                }
            ));
        }
    }
}

fn tamper_byte(path: &std::path::Path, off: u64, f: impl Fn(u8) -> u8) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let ctx = || format!("{} @{}", path.display(), off);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| LoomError::Io {
            op: "open(tamper)",
            ctx: ctx(),
            source: e,
        })?;
    file.seek(SeekFrom::Start(off)).map_err(|e| LoomError::Io {
        op: "seek",
        ctx: ctx(),
        source: e,
    })?;
    let mut b = [0u8; 1];
    file.read_exact(&mut b).map_err(|e| LoomError::Io {
        op: "read",
        ctx: ctx(),
        source: e,
    })?;
    b[0] = f(b[0]);
    file.seek(SeekFrom::Start(off)).map_err(|e| LoomError::Io {
        op: "seek",
        ctx: ctx(),
        source: e,
    })?;
    file.write_all(&b).map_err(|e| LoomError::Io {
        op: "write",
        ctx: ctx(),
        source: e,
    })?;
    file.sync_all().map_err(|e| LoomError::Io {
        op: "fsync",
        ctx: ctx(),
        source: e,
    })?;
    Ok(())
}

#[cfg(unix)]
fn mmap_workload(
    path: &std::path::Path,
    map_len: u64,
    offsets: &[u64],
    bs: usize,
    out: &mut [u8],
) -> std::result::Result<WorkloadResult, String> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    // SAFETY: fd is valid; we map read-only, shared, from offset 0 (always
    // page-aligned) for map_len bytes which is within the file's length.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len as usize,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(format!("mmap: {}", std::io::Error::last_os_error()));
    }
    let mut hist = LatencyHist::new();
    let t = Instant::now();
    for &off in offsets {
        let t1 = Instant::now();
        // SAFETY: off + bs <= map_len by construction (offsets come from
        // blocks inside the region); the mapping is readable for its whole
        // length; `out` is a distinct, writable, bs-byte buffer.
        unsafe {
            std::ptr::copy_nonoverlapping((p as *const u8).add(off as usize), out.as_mut_ptr(), bs);
        }
        hist.record(t1.elapsed());
    }
    let secs = t.elapsed().as_secs_f64();
    let fp = footprint::current().ok();
    // SAFETY: p/map_len are exactly what mmap returned.
    unsafe { libc::munmap(p, map_len as usize) };
    Ok(WorkloadResult {
        label: "mmap".into(),
        ops: offsets.len() as u64,
        secs,
        p50_ns: hist.percentile_ns(0.5).unwrap_or(0),
        p99_ns: hist.percentile_ns(0.99).unwrap_or(0),
        max_ns: hist.max_ns(),
        hit_rate: None,
        footprint_after: fp,
    })
}

#[cfg(not(unix))]
fn mmap_workload(
    _path: &std::path::Path,
    _map_len: u64,
    _offsets: &[u64],
    _bs: usize,
    _out: &mut [u8],
) -> std::result::Result<WorkloadResult, String> {
    Err("mmap baseline not implemented on this platform".into())
}

impl Report {
    /// The success sentence, filled with measurements — or the failures.
    pub fn verdict(&self) -> String {
        if self.passed {
            format!(
                "PROOF PASSED. Loom created a logical arena region of {} over a hot budget of {} ({:.1}x larger), \
kept the process footprint at or below {} (max observed {}), evicted and promoted every block correctly \
(patterns A and B verified byte-for-byte, B across a close/reopen), detected a single flipped byte on disk \
and cleared when it was restored, and measured the cost: hit p50≤{} / miss p50≤{}{}.",
                fmt_bytes(self.region_bytes),
                fmt_bytes(self.hot_bytes),
                self.region_bytes as f64 / self.hot_bytes.max(1) as f64,
                fmt_bytes(self.footprint_bound),
                self.footprint_max
                    .map(|f| fmt_bytes(f.footprint))
                    .unwrap_or_else(|| "unknown".into()),
                self.hit_p50_ns.map(fmt_ns).unwrap_or_else(|| "n/a".into()),
                self.miss_p50_ns.map(fmt_ns).unwrap_or_else(|| "n/a".into()),
                match (&self.skew_loom, &self.skew_pread, &self.skew_mmap) {
                    (Some(l), Some(p), Some(m)) => format!(
                        "; skewed workload p99: loom≤{} vs pread≤{} vs mmap≤{}",
                        fmt_ns(l.p99_ns),
                        fmt_ns(p.p99_ns),
                        fmt_ns(m.p99_ns)
                    ),
                    (Some(l), Some(p), None) => format!(
                        "; skewed workload p99: loom≤{} vs pread≤{}",
                        fmt_ns(l.p99_ns),
                        fmt_ns(p.p99_ns)
                    ),
                    _ => String::new(),
                }
            )
        } else {
            let mut s = String::from("PROOF FAILED:\n");
            for f in &self.failures {
                s.push_str("  - ");
                s.push_str(f);
                s.push('\n');
            }
            s
        }
    }
}
