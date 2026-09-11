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
  loom prove --pool <path> --size <bytes> --budget <bytes> [--region <bytes>]
             [--block <bytes>] [--seed <n>] [--ops <n>] [--no-baselines]
             [--keep] [--allow-small]

SIZES accept K/M/G/T suffixes (binary): 64K, 512M, 16G, 1T.

init    create a sparse pool file. Nothing is resident until it is used.
info    print the pool's geometry and what opening it would allocate.
prove   run the first experiment against a real device and print a verdict
        in which every number is a measurement. Exit 0 = passed, 1 = failed.
        Default region = 3/4 of --size. Region must be >= 4x budget.

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
