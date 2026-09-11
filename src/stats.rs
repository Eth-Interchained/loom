//! Counters and latency histograms describing Loom's own behaviour.
//!
//! These are measurements of things Loom did — a hit is a lookup that found
//! a resident frame, a miss is one that had to read the backing store. They
//! are never inferred from machine-wide state. Hit and miss latencies are
//! kept in separate histograms and are never averaged together: a cache hit
//! labelled as disk latency (or vice versa) is exactly the kind of number
//! this project refuses to print.

use std::time::Duration;

/// Log-scale histogram: 4 buckets per power of two from 1 ns to ~2^40 ns.
/// Resolution is ~19% per bucket, which is honest enough for p50/p99 and
/// costs 1.3 KiB regardless of sample count (so the histogram never shows
/// up in the footprint we are measuring).
#[derive(Clone)]
pub struct LatencyHist {
    buckets: [u64; 160],
    count: u64,
    max_ns: u64,
    sum_ns: u128,
}

impl Default for LatencyHist {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHist {
    pub fn new() -> Self {
        LatencyHist {
            buckets: [0; 160],
            count: 0,
            max_ns: 0,
            sum_ns: 0,
        }
    }

    fn bucket_of(ns: u64) -> usize {
        if ns == 0 {
            return 0;
        }
        let lz = 63 - ns.leading_zeros() as u64; // floor(log2)
                                                 // Sub-bucket from the two bits below the leading one.
        let sub = if lz >= 2 {
            (ns >> (lz - 2)) & 0b11
        } else {
            (ns << (2 - lz)) & 0b11
        };
        ((lz * 4 + sub) as usize).min(159)
    }

    /// Upper bound (ns) of a bucket — reported values are "≤ this".
    fn bucket_upper(b: usize) -> u64 {
        let lz = (b / 4) as u64;
        let sub = (b % 4) as u64;
        if lz < 2 {
            return 1u64 << (lz + 1);
        }
        // start of next sub-bucket
        ((4 + sub + 1) << (lz - 2)).saturating_sub(1)
    }

    pub fn record(&mut self, d: Duration) {
        let ns = d.as_nanos().min(u64::MAX as u128) as u64;
        self.buckets[Self::bucket_of(ns)] += 1;
        self.count += 1;
        self.sum_ns += ns as u128;
        if ns > self.max_ns {
            self.max_ns = ns;
        }
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn max_ns(&self) -> u64 {
        self.max_ns
    }

    pub fn mean_ns(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some((self.sum_ns / self.count as u128) as u64)
        }
    }

    /// Percentile as a bucket upper bound in ns. `None` if empty — unknown
    /// is better than zero.
    pub fn percentile_ns(&self, p: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let target = ((self.count as f64) * p).ceil().max(1.0) as u64;
        let mut acc = 0u64;
        for (i, &c) in self.buckets.iter().enumerate() {
            acc += c;
            if acc >= target {
                return Some(Self::bucket_upper(i).min(self.max_ns));
            }
        }
        Some(self.max_ns)
    }

    pub fn summary(&self) -> String {
        match self.count {
            0 => "n=0 (no samples)".to_string(),
            _ => format!(
                "n={} p50≤{} p99≤{} max={} mean={}",
                self.count,
                fmt_ns(self.percentile_ns(0.50).unwrap()),
                fmt_ns(self.percentile_ns(0.99).unwrap()),
                fmt_ns(self.max_ns),
                fmt_ns(self.mean_ns().unwrap()),
            ),
        }
    }
}

pub fn fmt_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1e3)
    } else if ns < 1_000_000_000 {
        format!("{:.2}ms", ns as f64 / 1e6)
    } else {
        format!("{:.2}s", ns as f64 / 1e9)
    }
}

pub fn fmt_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f >= K * K * K * K {
        format!("{:.2} TiB", f / (K * K * K * K))
    } else if f >= K * K * K {
        format!("{:.2} GiB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.1} MiB", f / (K * K))
    } else if f >= K {
        format!("{:.1} KiB", f / K)
    } else {
        format!("{b} B")
    }
}

/// Everything Loom knows about what it has done since open.
#[derive(Clone, Default)]
pub struct Stats {
    /// Lookups that found the block in a frame.
    pub hits: u64,
    /// Lookups that had to bring the block in (from disk or as zeros).
    pub misses: u64,
    /// Misses served as zeros because the block was never written (no I/O).
    pub zero_fills: u64,
    /// Misses that skipped the load because the caller overwrote the whole block.
    pub full_overwrites: u64,
    /// Batched speculative reads issued (one `pread` each).
    pub prefetch_batches: u64,
    /// Blocks brought in speculatively by those batches.
    pub prefetch_blocks: u64,
    /// Speculative blocks that were later actually asked for. The honest
    /// scoreboard for prefetch: `prefetch_used / prefetch_blocks` is how
    /// often the guess was right, and it is never assumed.
    pub prefetch_used: u64,
    /// Blocks the caller borrowed in place, with no copy.
    pub zero_copy: u64,
    /// Frames reclaimed by the CLOCK policy.
    pub evictions: u64,
    /// Dirty frames written to the backing store (on eviction or sync).
    pub writebacks: u64,
    pub bytes_read_backing: u64,
    pub bytes_written_backing: u64,
    /// Latency of a hit: map lookup + frame address, measured inside Loom.
    pub hit_latency: LatencyHist,
    /// Latency of a miss: victim selection + writeback (if dirty) + read + verify.
    pub miss_latency: LatencyHist,
    /// Latency of a single backing-store writeback.
    pub writeback_latency: LatencyHist,
}

impl Stats {
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        if total == 0 {
            None
        } else {
            Some(self.hits as f64 / total as f64)
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "hits: {}  misses: {}  hit rate: {}\n",
            self.hits,
            self.misses,
            match self.hit_rate() {
                Some(r) => format!("{:.2}%", r * 100.0),
                None => "unknown (no lookups)".into(),
            }
        ));
        s.push_str(&format!(
            "  zero-fills: {}  full-overwrites: {}  evictions: {}  writebacks: {}\n",
            self.zero_fills, self.full_overwrites, self.evictions, self.writebacks
        ));
        s.push_str(&format!(
            "  prefetch: {} batches, {} blocks, {} later used ({})  zero-copy borrows: {}\n",
            self.prefetch_batches,
            self.prefetch_blocks,
            self.prefetch_used,
            match self.prefetch_blocks {
                0 => "n/a — prefetch issued nothing".to_string(),
                n => format!(
                    "{:.1}% useful",
                    self.prefetch_used as f64 / n as f64 * 100.0
                ),
            },
            self.zero_copy
        ));
        s.push_str(&format!(
            "  backing read: {}  backing written: {}\n",
            fmt_bytes(self.bytes_read_backing),
            fmt_bytes(self.bytes_written_backing)
        ));
        s.push_str(&format!(
            "  hit latency:       {}\n",
            self.hit_latency.summary()
        ));
        s.push_str(&format!(
            "  miss latency:      {}\n",
            self.miss_latency.summary()
        ));
        s.push_str(&format!(
            "  writeback latency: {}\n",
            self.writeback_latency.summary()
        ));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_monotone() {
        let mut last = 0;
        for b in 0..160 {
            let u = LatencyHist::bucket_upper(b);
            assert!(u >= last, "bucket {b}: {u} < {last}");
            last = u;
        }
    }

    #[test]
    fn value_lands_at_or_below_its_upper_bound() {
        for ns in [
            1u64,
            2,
            3,
            5,
            100,
            1023,
            1024,
            65535,
            1_000_000,
            123_456_789,
        ] {
            let b = LatencyHist::bucket_of(ns);
            assert!(
                ns <= LatencyHist::bucket_upper(b),
                "{ns} > upper {} of bucket {b}",
                LatencyHist::bucket_upper(b)
            );
        }
    }

    #[test]
    fn percentiles_of_known_distribution() {
        let mut h = LatencyHist::new();
        for i in 1..=1000u64 {
            h.record(Duration::from_nanos(i * 1000));
        }
        let p50 = h.percentile_ns(0.5).unwrap();
        let p99 = h.percentile_ns(0.99).unwrap();
        // 500µs and 990µs, within one 19% bucket.
        assert!((400_000..=600_000).contains(&p50), "p50={p50}");
        assert!((900_000..=1_000_000).contains(&p99), "p99={p99}");
        assert_eq!(h.max_ns(), 1_000_000);
        assert!(h.percentile_ns(0.5).is_some());
        assert!(LatencyHist::new().percentile_ns(0.5).is_none());
    }
}
