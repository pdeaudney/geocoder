//! Builds the tantivy forward-geocoding index from an existing reverse-
//! geocoding binary index.
//!
//! Usage:
//!   build-forward-index <reverse-index-dir> [<tantivy-dir>]
//!   build-forward-index <reverse-index-dir> --partition-by-country
//!
//! Monolithic mode (default): writes a single `tantivy/` directory. Fine
//! for deployments serving one or two countries.
//!
//! Partitioned mode: writes `tantivy_<cc>/` per country into the reverse-
//! index dir. Smaller, faster per-query dispatch; ideal for worldwide
//! deployments. `Forward::open` picks up both layouts automatically.

// mimalloc global allocator — see build-pipeline-perf-plan stage 2.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use query_server::forward;
use std::path::PathBuf;
use std::time::Instant;

/// Per-stage timing — see build-pipeline-perf-plan stage 6.
struct Stage { name: &'static str, start: Instant }
impl Stage { fn new(name: &'static str) -> Self { Self { name, start: Instant::now() } } }
impl Drop for Stage { fn drop(&mut self) { eprintln!("[stage] {}: {:.3}s", self.name, self.start.elapsed().as_secs_f64()); } }

fn main() {
    let _total = Stage::new("total");
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "Usage: {} <reverse-index-dir> [<tantivy-dir>] [--partition-by-country] [--tantivy-heap-mb N]",
            args.first().map(String::as_str).unwrap_or("build-forward-index")
        );
        std::process::exit(2);
    }

    let source = PathBuf::from(&args[1]);
    let partition = args.iter().any(|a| a == "--partition-by-country");

    // --tantivy-heap-mb sets the per-IndexWriter heap budget. Default
    // 512 MB is sized for AU's ~100 MB tantivy dataset to fit in one
    // in-memory segment with headroom. Planet operators should bump
    // this — empirical sweet spot lives near the dataset's working
    // set size; see docs/performance/tantivy-heap-2026-04-25.md.
    let heap_mb: usize = args
        .iter()
        .position(|a| a == "--tantivy-heap-mb")
        .and_then(|p| args.get(p + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    let heap_bytes = heap_mb.saturating_mul(1024 * 1024);

    let t0 = std::time::Instant::now();
    let result: Result<(), String> = if partition {
        eprintln!("Building per-country tantivy indexes under {} (heap={} MB)", source.display(), heap_mb);
        forward::build_partitioned_with_heap(&source, &source, heap_bytes).map(|stats| {
            let mut countries: Vec<_> = stats.keys().collect();
            countries.sort();
            for cc in countries {
                let s = &stats[cc];
                eprintln!(
                    "  tantivy_{}{}: {} places + {} streets",
                    cc[0] as char, cc[1] as char, s.places, s.streets
                );
            }
            eprintln!("Built {} country indexes in {:.1}s", stats.len(), t0.elapsed().as_secs_f64());
        })
    } else {
        let dest = args
            .iter()
            .skip(2)
            .find(|a| !a.starts_with("--"))
            .map(PathBuf::from)
            .unwrap_or_else(|| source.join("tantivy"));
        eprintln!("Building monolithic forward index: {} -> {} (heap={} MB)", source.display(), dest.display(), heap_mb);
        forward::build_with_heap(&source, &dest, heap_bytes).map(|stats| {
            eprintln!(
                "Indexed {} places + {} streets in {:.1}s",
                stats.places,
                stats.streets,
                t0.elapsed().as_secs_f64()
            );
        })
    };

    if let Err(e) = result {
        eprintln!("Build failed: {e}");
        std::process::exit(1);
    }
}
