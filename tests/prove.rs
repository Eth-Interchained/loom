//! The first experiment at test scale. Same code path as `loom prove`;
//! only the numbers are small so CI finishes in seconds. A passing run here
//! proves the algorithm; the README's real-scale run proves the thesis.

use loom::prove::{run, ProveConfig};
use loom::MIB;

fn pool_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("loom-prove-{}-{}.pool", std::process::id(), name));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn prove_passes_at_test_scale() {
    let cfg = ProveConfig {
        pool: pool_path("small"),
        size: 256 * MIB,
        budget: 8 * MIB,
        block_size: 64 * 1024,
        region: 160 * MIB,
        seed: 42,
        ops: 3000,
        baselines: true,
        keep_pool: false,
        allow_small: false,
    };
    let mut out = Vec::new();
    let report = run(&cfg, &mut out);
    let text = String::from_utf8_lossy(&out);
    println!("{text}");
    println!("{}", report.verdict());
    assert!(report.passed, "{}", report.verdict());
    assert!(report.tamper_detected);
    assert!(report.tamper_restored_ok);
    assert!(report.skew_loom.is_some());
    assert!(report.skew_pread.is_some());
    // Hits must be measurably cheaper than misses or the tier is not a tier.
    let hit = report.hit_p50_ns.expect("hit samples");
    let miss = report.miss_p50_ns.expect("miss samples");
    assert!(hit < miss, "hit p50 {hit}ns not below miss p50 {miss}ns");
    assert!(
        !cfg.pool.exists(),
        "pool should be removed when keep_pool=false"
    );
}

#[test]
fn prove_refuses_meaningless_geometry() {
    let cfg = ProveConfig {
        pool: pool_path("tiny"),
        size: 64 * MIB,
        budget: 32 * MIB,
        block_size: 64 * 1024,
        region: 48 * MIB,
        seed: 1,
        ops: 10,
        baselines: false,
        keep_pool: false,
        allow_small: false,
    };
    let mut out = Vec::new();
    let report = run(&cfg, &mut out);
    assert!(!report.passed);
    assert!(
        report
            .failures
            .iter()
            .any(|f| f.contains("less than 4x budget")),
        "{:?}",
        report.failures
    );
}
