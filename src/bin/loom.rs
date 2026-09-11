//! `loom` — init / info / prove. Three subcommands, hand-parsed. No UX
//! beyond what the experiment needs.

use loom::prove::{self, ProveConfig};
use loom::stats::fmt_bytes;
use loom::{CreateOptions, Loom, OpenOptions, DEFAULT_BLOCK_SIZE, GIB};
use std::path::PathBuf;
use std::process::exit;

const USAGE: &str = "\
loom — storage-backed logical memory arena with a bounded RAM footprint

USAGE
  loom init  <pool> --size <bytes> [--budget <bytes>] [--block <bytes>]
  loom info  <pool>
  loom faultin --pool <path> --arena <bytes> --budget <bytes> [--extent <bytes>]
  loom prove --pool <path> --size <bytes> --budget <bytes> [--region <bytes>]
             [--block <bytes>] [--seed <n>] [--ops <n>] [--prefetch <blocks>]
             [--no-baselines] [--keep] [--allow-small]

SIZES accept K/M/G/T suffixes (binary): 64K, 512M, 16G, 1T.

init    create a sparse pool file. Nothing is resident until it is used.
info    print the pool's geometry and what opening it would allocate.
faultin hand a RAW POINTER over an arena larger than the RAM budget and
        walk it with ordinary dereferences — no Loom API calls. Reports
        faults, peak residency and the warm-pass cost. This is the
        transparent-memory demonstration; runs on macOS and Linux.

prove   run the first experiment against a real device and print a verdict
        in which every number is a measurement. Exit 0 = passed, 1 = failed.
        Default region = 3/4 of --size. Region must be >= 4x budget.

--prefetch <blocks>  speculative read depth once a sequential run is seen.
                     0 disables it. Default 16 (one 1 MiB read per batch at
                     the default block size). Run with 0 and without to see
                     what it is actually worth on your device.

Physical RAM is NOT consulted: the proof is about Loom's own footprint vs
its budget. Choose --budget below your RAM and --region above it yourself.";

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1u64 << 20),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1u64 << 30),
        Some('T') | Some('t') => (&s[..s.len() - 1], 1u64 << 40),
        _ => (s, 1u64),
    };
    let n: f64 = num
        .parse()
        .map_err(|_| format!("bad size '{s}' (examples: 64K, 512M, 16G)"))?;
    if n < 0.0 {
        return Err(format!("negative size '{s}'"));
    }
    Ok((n * mult as f64) as u64)
}

struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

fn parse_args(argv: &[String]) -> Args {
    let mut positional = Vec::new();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if let Some(name) = a.strip_prefix("--") {
            let takes_value = !matches!(name, "no-baselines" | "keep" | "allow-small" | "help");
            if takes_value {
                let v = argv.get(i + 1).cloned();
                flags.push((name.to_string(), v));
                i += 2;
            } else {
                flags.push((name.to_string(), None));
                i += 1;
            }
        } else {
            positional.push(a.clone());
            i += 1;
        }
    }
    Args { positional, flags }
}

impl Args {
    fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_deref())
    }
    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }
    fn size(&self, name: &str) -> Result<Option<u64>, String> {
        match self.flag(name) {
            Some(v) => parse_size(v).map(Some),
            None => match self.has(name) {
                true => Err(format!("--{name} needs a value")),
                false => Ok(None),
            },
        }
    }
}

fn die(msg: &str, code: i32) -> ! {
    eprintln!("loom: {msg}");
    exit(code)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" {
        println!("{USAGE}");
        exit(if argv.is_empty() { 2 } else { 0 });
    }
    let cmd = argv[0].as_str();
    let args = parse_args(&argv[1..]);
    if args.has("help") {
        println!("{USAGE}");
        exit(0);
    }
    match cmd {
        "init" => cmd_init(&args),
        "info" => cmd_info(&args),
        "prove" => cmd_prove(&args),
        "faultin" => cmd_faultin(&args),
        other => die(&format!("unknown command '{other}'\n\n{USAGE}"), 2),
    }
}

fn cmd_init(args: &Args) {
    let pool = args
        .positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| die("init: pool path required", 2));
    let size = args
        .size("size")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or_else(|| die("init: --size required", 2));
    let block = args
        .size("block")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or(DEFAULT_BLOCK_SIZE as u64);
    let budget = args
        .size("budget")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or_else(|| (size / 16).max(block));
    let size = size.div_ceil(block) * block;
    let loom = Loom::create(
        &pool,
        CreateOptions {
            capacity: size,
            block_size: block as u32,
            budget,
        },
    )
    .unwrap_or_else(|e| die(&format!("init: {e}"), 1));
    let info = loom
        .info()
        .unwrap_or_else(|e| die(&format!("init: {e}"), 1));
    println!("Loom pool created\n");
    print_info(&info);
    loom.close()
        .unwrap_or_else(|e| die(&format!("init: close: {e}"), 1));
}

fn cmd_info(args: &Args) {
    let pool = args
        .positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| die("info: pool path required", 2));
    let loom =
        Loom::open(&pool, OpenOptions::default()).unwrap_or_else(|e| die(&format!("info: {e}"), 1));
    let info = loom
        .info()
        .unwrap_or_else(|e| die(&format!("info: {e}"), 1));
    print_info(&info);
    for r in loom.regions() {
        println!(
            "  region {:>4}  offset {:>14}  len {}",
            r.id,
            r.offset,
            fmt_bytes(r.len)
        );
    }
    match loom::footprint::current() {
        Ok(f) => println!("Process footprint now: {f}"),
        Err(e) => println!("Process footprint: unknown ({e})"),
    }
}

fn print_info(info: &loom::Info) {
    println!("Backing:        {}", info.path);
    println!(
        "Capacity:       {}  ({} blocks of {})",
        fmt_bytes(info.capacity),
        info.nblocks,
        fmt_bytes(info.block_size as u64)
    );
    println!(
        "Hot budget:     {}  → {} frames, {} allocated when open",
        fmt_bytes(info.budget),
        info.frame_count,
        fmt_bytes(info.hot_bytes_allocated)
    );
    println!(
        "Metadata:       {} in RAM when open",
        fmt_bytes(info.metadata_bytes)
    );
    println!(
        "Regions:        {} ({} of arena allocated)",
        info.regions,
        fmt_bytes(info.allocated_arena_bytes)
    );
    println!("On disk now:    {}", fmt_bytes(info.disk_allocated));
    println!("I/O mode:       {}", info.cache_mode);
}

fn cmd_prove(args: &Args) {
    let pool = args
        .flag("pool")
        .map(PathBuf::from)
        .unwrap_or_else(|| die("prove: --pool required", 2));
    let size = args
        .size("size")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or_else(|| die("prove: --size required", 2));
    let budget = args
        .size("budget")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or_else(|| die("prove: --budget required", 2));
    let block = args
        .size("block")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or(DEFAULT_BLOCK_SIZE as u64);
    let region = args
        .size("region")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or(size / 4 * 3);
    let round = |v: u64| v.div_ceil(block) * block;
    let cfg = ProveConfig {
        pool,
        size: round(size),
        budget,
        block_size: block as u32,
        region: round(region),
        seed: args
            .flag("seed")
            .map(|s| s.parse().unwrap_or_else(|_| die("bad --seed", 2)))
            .unwrap_or(7),
        ops: args
            .flag("ops")
            .map(|s| s.parse().unwrap_or_else(|_| die("bad --ops", 2)))
            .unwrap_or(20_000),
        baselines: !args.has("no-baselines"),
        keep_pool: args.has("keep"),
        allow_small: args.has("allow-small"),
        prefetch_depth: args
            .flag("prefetch")
            .map(|v| v.parse().unwrap_or_else(|_| die("bad --prefetch", 2))),
    };
    if cfg.size < GIB {
        eprintln!(
            "loom: note: arena under 1 GiB — fine for exercising the code, small for a proof"
        );
    }
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let report = prove::run(&cfg, &mut lock);
    drop(lock);
    println!();
    println!("{}", report.verdict());
    exit(if report.passed { 0 } else { 1 });
}

/// The transparent-memory demonstration: a raw pointer over an arena larger
/// than the RAM budget, walked with ordinary dereferences.
///
/// Every number printed is measured. The pattern is regenerated for
/// comparison rather than stored, so verifying an 80 GiB arena does not
/// require 80 GiB of reference data.
fn cmd_faultin(args: &Args) {
    use loom::faultin_signal::{SignalFaultArena, SignalFaultOptions};
    use loom::pattern;

    let pool = args
        .flag("pool")
        .map(PathBuf::from)
        .unwrap_or_else(|| die("faultin: --pool required", 2));
    let arena_len = args
        .size("arena")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or_else(|| die("faultin: --arena required", 2));
    let budget = args
        .size("budget")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or_else(|| die("faultin: --budget required", 2));
    let extent = args
        .size("extent")
        .unwrap_or_else(|e| die(&e, 2))
        .unwrap_or(loom::faultin_signal::DEFAULT_EXTENT as u64);

    if arena_len <= budget {
        die(
            "faultin: --arena must be LARGER than --budget, or the demo proves nothing",
            2,
        );
    }
    if pool.exists() {
        die(
            &format!(
                "faultin: {} already exists; refusing to overwrite",
                pool.display()
            ),
            2,
        );
    }

    let block = extent; // one block per extent keeps the mapping simple
    let arena_len = arena_len.div_ceil(extent) * extent;
    let capacity = (arena_len + 8 * block).div_ceil(block) * block;

    // The fault arena's mapped pages ARE the cache. Loom underneath it is
    // only the I/O path, so its frame pool must be SMALL — otherwise the two
    // tiers stack and the process uses twice the stated budget. (Measured:
    // a 128 MiB arena budget with a 128 MiB Loom pool produced a 257.8 MiB
    // footprint. Two caches for one budget.)
    const LOOM_STAGING_EXTENTS: u64 = 4;
    let loom_budget = LOOM_STAGING_EXTENTS * block;

    println!(
        "LOOM FAULTIN  pool={}  arena={}  extent={}",
        pool.display(),
        fmt_bytes(arena_len),
        fmt_bytes(extent)
    );
    println!(
        "  RAM: {} mapped arena + {} Loom staging = {} total budget",
        fmt_bytes(budget),
        fmt_bytes(loom_budget),
        fmt_bytes(budget + loom_budget)
    );
    match loom::footprint::current() {
        Ok(f) => println!("[0] process footprint before Loom: {f}"),
        Err(e) => println!("[0] process footprint: unknown ({e})"),
    }

    // Build the pool and lay down a verifiable pattern.
    let mut l = Loom::create_with(
        &pool,
        CreateOptions {
            capacity,
            block_size: block as u32,
            budget: loom_budget,
        },
        OpenOptions {
            budget: Some(loom_budget),
            // The arena reads one extent at a time and never sequentially
            // from Loom's point of view, so speculation would be waste.
            prefetch_depth: Some(0),
        },
    )
    .unwrap_or_else(|e| die(&format!("faultin: create: {e}"), 1));
    let region = l
        .alloc(arena_len)
        .unwrap_or_else(|e| die(&format!("faultin: alloc: {e}"), 1));
    let nblk = arena_len / block;
    println!(
        "[1] seeding {} blocks of {} with a deterministic pattern…",
        nblk,
        fmt_bytes(block)
    );
    let t = std::time::Instant::now();
    let mut buf = vec![0u8; block as usize];
    for b in 0..nblk {
        pattern::fill(99, b'X', b, &mut buf);
        l.write(region, b * block, &buf)
            .unwrap_or_else(|e| die(&format!("faultin: seed write: {e}"), 1));
        if b % 8192 == 0 && b > 0 {
            let secs = t.elapsed().as_secs_f64();
            println!(
                "      … {b}/{nblk} blocks, {}/s, eta {:.0}s",
                fmt_bytes(((b * block) as f64 / secs) as u64),
                (nblk - b) as f64 * (secs / b as f64)
            );
        }
    }
    l.sync()
        .unwrap_or_else(|e| die(&format!("faultin: sync: {e}"), 1));
    println!(
        "    seeded {} in {:.1}s ({}/s)",
        fmt_bytes(arena_len),
        t.elapsed().as_secs_f64(),
        fmt_bytes((arena_len as f64 / t.elapsed().as_secs_f64()) as u64)
    );

    // Hand out the pointer.
    let fa = SignalFaultArena::new(
        l,
        region,
        SignalFaultOptions {
            len: arena_len as usize,
            extent: extent as usize,
            resident_budget: budget as usize,
        },
    )
    .unwrap_or_else(|e| die(&format!("faultin: install: {e}"), 1));
    let ptr = fa.as_ptr();
    println!(
        "[2] arena mapped at {:p}: {} addressable, {} may be resident ({:.1}x over budget)",
        ptr,
        fmt_bytes(arena_len),
        fmt_bytes(fa.budget_bytes() as u64),
        arena_len as f64 / fa.budget_bytes() as f64
    );

    // --- read every byte through the pointer, verifying ---
    println!("[3] reading the whole arena through the raw pointer, verifying every byte…");
    let t = std::time::Instant::now();
    let mut expect = vec![0u8; block as usize];
    let mut bad = 0u64;
    for b in 0..nblk {
        pattern::fill(99, b'X', b, &mut expect);
        // SAFETY: [ptr + b*block, +block) is inside the arena; faults are
        // resolved by the installed handler.
        let got =
            unsafe { std::slice::from_raw_parts(ptr.add((b * block) as usize), block as usize) };
        if pattern::first_mismatch(&expect, got).is_some() {
            bad += 1;
            if bad == 1 {
                println!("    MISMATCH at block {b}");
            }
        }
        if b % 8192 == 0 && b > 0 {
            let secs = t.elapsed().as_secs_f64();
            println!(
                "      … {b}/{nblk} blocks, {}/s, eta {:.0}s",
                fmt_bytes(((b * block) as f64 / secs) as u64),
                (nblk - b) as f64 * (secs / b as f64)
            );
        }
    }
    let read_secs = t.elapsed().as_secs_f64();
    println!(
        "    read {} in {:.1}s ({}/s), {} mismatching blocks",
        fmt_bytes(arena_len),
        read_secs,
        fmt_bytes((arena_len as f64 / read_secs) as u64),
        bad
    );
    let st = fa.stats();
    println!("    {}", st.render(fa.extent()));

    // --- warm pass over what fits, to show a hit costs nothing ---
    let warm_bytes = fa.budget_bytes().min(arena_len as usize);
    // SAFETY: inside the arena.
    let warm = unsafe { std::slice::from_raw_parts(ptr, warm_bytes) };
    let before = fa.stats();
    let t = std::time::Instant::now();
    let mut acc = 0u64;
    for c in warm.chunks(4096) {
        acc = acc.wrapping_add(c[0] as u64);
    }
    let cold_warmup = t.elapsed().as_secs_f64();
    let mid = fa.stats();
    let t = std::time::Instant::now();
    for c in warm.chunks(4096) {
        acc = acc.wrapping_add(c[0] as u64);
    }
    let warm_secs = t.elapsed().as_secs_f64();
    let after = fa.stats();
    std::hint::black_box(acc);
    println!(
        "[4] warm pass over {}: first {:.3}ms, second {:.3}ms; load faults {} -> {} -> {}",
        fmt_bytes(warm_bytes as u64),
        cold_warmup * 1e3,
        warm_secs * 1e3,
        before.load_faults,
        mid.load_faults,
        after.load_faults
    );
    if after.load_faults == mid.load_faults {
        println!("    the second pass took ZERO further faults — a hit runs no Loom code at all");
    } else {
        println!(
            "    the second pass took {} further faults — the working set does not fit the budget",
            after.load_faults - mid.load_faults
        );
    }

    match loom::footprint::current() {
        Ok(f) => {
            let bound = budget + loom_budget;
            println!("[5] process footprint after: {f}");
            println!(
                "    against a combined budget of {} -> {}",
                fmt_bytes(bound),
                if f.footprint <= bound + (48 << 20) {
                    "within bound (+48 MiB process slack)".to_string()
                } else {
                    format!(
                        "OVER by {} — something is holding arena bytes twice",
                        fmt_bytes(f.footprint - bound)
                    )
                }
            );
        }
        Err(e) => println!("[5] process footprint: unknown ({e})"),
    }

    let st = fa.stats();
    let ok = bad == 0 && st.failures == 0;
    println!();
    if ok {
        println!(
            "RESULT: addressed {} of memory through a plain pointer while never mapping more \nthan {}. Every byte verified. {} faults, {} evictions, {} writebacks, 0 failures.",
            fmt_bytes(arena_len),
            fmt_bytes((st.peak_resident_extents * fa.extent()) as u64),
            st.load_faults,
            st.evictions,
            st.writebacks
        );
    } else {
        println!(
            "RESULT: FAILED — {bad} mismatching blocks, {} handler failures{}",
            st.failures,
            if st.failures > 0 {
                format!(
                    " (first at {:#x}, errno {})",
                    st.first_failure_addr, st.first_failure_errno
                )
            } else {
                String::new()
            }
        );
    }
    drop(fa);
    if !args.has("keep") {
        let _ = std::fs::remove_file(&pool);
    }
    exit(if ok { 0 } else { 1 });
}
