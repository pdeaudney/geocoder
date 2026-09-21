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

use query_server::{forward, manifest};
use serde_json::json;
use std::path::PathBuf;
use std::time::Instant;

/// Per-stage timing — see build-pipeline-perf-plan stage 6.
struct Stage {
    name: &'static str,
    start: Instant,
}
impl Stage {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        eprintln!(
            "[stage] {}: {:.3}s",
            self.name,
            self.start.elapsed().as_secs_f64()
        );
    }
}

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
    let heap_flag_pos = args.iter().position(|a| a == "--tantivy-heap-mb");
    let heap_mb: usize = heap_flag_pos
        .and_then(|p| args.get(p + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    let heap_bytes = heap_mb.saturating_mul(1024 * 1024);

    // Indices in args that are flag values, not positional. The dest-
    // path parser below skips these so e.g. `--tantivy-heap-mb 50`
    // doesn't make `50` look like a positional dest.
    let consumed: std::collections::HashSet<usize> = heap_flag_pos
        .map(|p| std::collections::HashSet::from([p, p + 1]))
        .unwrap_or_default();

    let dest = args
        .iter()
        .enumerate()
        .skip(2)
        .find(|(i, a)| !a.starts_with("--") && !consumed.contains(i))
        .map(|(_, a)| PathBuf::from(a))
        .unwrap_or_else(|| {
            if partition {
                source.clone()
            } else {
                source.join("tantivy")
            }
        });

    let t0 = std::time::Instant::now();
    let result: Result<serde_json::Value, String> = if partition {
        eprintln!(
            "Building per-country tantivy indexes under {} (heap={} MB)",
            dest.display(),
            heap_mb
        );
        forward::build_partitioned_with_heap(&source, &dest, heap_bytes).map(|stats| {
            let mut countries: Vec<_> = stats.keys().collect();
            countries.sort();
            let mut total_places: u64 = 0;
            let mut total_streets: u64 = 0;
            let mut total_addresses: u64 = 0;
            let mut by_country = serde_json::Map::new();
            for cc in countries {
                let s = &stats[cc];
                eprintln!(
                    "  tantivy_{}{}: {} places + {} streets/POIs + {} addresses",
                    cc[0] as char, cc[1] as char, s.places, s.streets, s.addresses
                );
                total_places += s.places as u64;
                total_streets += s.streets as u64;
                total_addresses += s.addresses as u64;
                let key = format!("{}{}", cc[0] as char, cc[1] as char);
                by_country.insert(
                    key,
                    json!({ "places": s.places, "streets": s.streets, "addresses": s.addresses }),
                );
            }
            eprintln!(
                "Built {} country indexes in {:.1}s",
                stats.len(),
                t0.elapsed().as_secs_f64()
            );
            json!({
                "layout": "per-country",
                "country_count": stats.len(),
                "total_places": total_places,
                "total_streets": total_streets,
                "total_addresses": total_addresses,
                "tantivy_heap_mb": heap_mb,
                "build_seconds": t0.elapsed().as_secs_f64(),
                "by_country": by_country,
            })
        })
    } else {
        eprintln!(
            "Building monolithic forward index: {} -> {} (heap={} MB)",
            source.display(),
            dest.display(),
            heap_mb
        );
        forward::build_with_heap(&source, &dest, heap_bytes).map(|stats| {
            eprintln!(
                "Indexed {} places + {} streets/POIs + {} addresses in {:.1}s",
                stats.places,
                stats.streets,
                stats.addresses,
                t0.elapsed().as_secs_f64()
            );
            json!({
                "layout": "monolithic",
                "dest": dest.display().to_string(),
                "places": stats.places,
                "streets": stats.streets,
                "addresses": stats.addresses,
                "tantivy_heap_mb": heap_mb,
                "build_seconds": t0.elapsed().as_secs_f64(),
            })
        })
    };

    match result {
        Err(e) => {
            eprintln!("Build failed: {e}");
            std::process::exit(1);
        }
        Ok(extra) => {
            if let Err(e) =
                manifest::write(if partition { &dest } else { &source }, "forward", extra)
            {
                eprintln!("warning: failed to write manifest_forward.json: {e}");
            }
        }
    }
}
