//! Criterion benchmarks for `/autocomplete` against a real FST index.
//!
//! Requires `GEOCODER_INDEX_DIR` to point at a directory containing
//! `fst_<cc>.{fst,bin}` + `fst_<cc>_strings.bin` (and/or the unified
//! variants). When unset the bench exits cleanly so `cargo bench` is
//! still valid on a machine without the index.
//!
//! Run:
//!   GEOCODER_INDEX_DIR=/abs/path/to/data/index cargo bench --bench autocomplete
//!
//! Bench groups, in order of "where the recent perf work shows up":
//!
//! - `fst_starts_with` — prefix walks of varying breadth. Broad
//!   prefixes (1–2 chars) walk thousands of FST keys; this is the
//!   path the heap-deferred-string-hydration win targets. Wide
//!   prefixes pre-optimisation allocated one `String` per visited
//!   candidate; post-optimisation, only the surviving `limit`
//!   entries materialise names.
//!
//! - `fst_exact_match` — the `/search` fast-path. One FST get + one
//!   record fetch + one string hydration. Should be sub-µs.
//!
//! - `fst_starts_with_any` — country-agnostic prefix walk
//!   (iterates every loaded per-country FST). Used by `/search` when
//!   no country_code hint was provided.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use query_server::autocomplete::Autocomplete;
use std::hint::black_box;
use std::path::Path;

/// Country code we benchmark against. AU is the canonical local-dev
/// dataset; if a different country is loaded, fall back to the first
/// available.
const PREFERRED_CC: [u8; 2] = *b"au";

/// Mix of prefix lengths chosen to cover every cost regime:
///   - 1 char: pathological broad walk (capped at FST_WALK_CAP=10k)
///   - 2 char: still broad, common typeahead state
///   - 3 char: typical mid-typing state
///   - full word: narrow walk, mostly exercises post-walk sort
const PREFIX_FIXTURES: &[(&str, &str)] = &[
    ("very_broad_1char",   "s"),
    ("broad_2char",        "sy"),
    ("medium_3char",       "syd"),
    ("medium_4char",       "sydn"),
    ("narrow_full_word",   "sydney"),
    ("narrow_two_words",   "bondi beach"),
    ("rural_specific",     "wagga wagga"),
    ("street_token",       "elizabeth"),
];

const EXACT_MATCH_FIXTURES: &[(&str, &str)] = &[
    ("exact_sydney",      "sydney"),
    ("exact_melbourne",   "melbourne"),
    ("exact_bondi_beach", "bondi beach"),
];

fn load_autocomplete() -> Option<(Autocomplete, [u8; 2])> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let path = Path::new(&dir);
    let auto = match Autocomplete::open(path) {
        Ok(Some(a)) => a,
        Ok(None) => {
            eprintln!("SKIP: no FST autocomplete files under {}", dir);
            return None;
        }
        Err(e) => {
            eprintln!("WARN: failed to load Autocomplete from {}: {}", dir, e);
            return None;
        }
    };
    // Pick AU if present, else the first loaded country. The whole
    // bench keys off whatever cc we picked.
    let countries = auto.countries();
    let cc = if countries.iter().any(|c| c == &PREFERRED_CC) {
        PREFERRED_CC
    } else {
        match countries.first().copied() {
            Some(c) => c,
            None => {
                eprintln!("SKIP: Autocomplete loaded but reports zero countries");
                return None;
            }
        }
    };
    eprintln!("Benchmarking autocomplete against country {:?}", std::str::from_utf8(&cc).unwrap_or("??"));
    Some((auto, cc))
}

fn bench_starts_with(c: &mut Criterion) {
    let Some((auto, cc)) = load_autocomplete() else {
        return;
    };

    // limit=10 is the default the HTTP handler uses; vary to show the
    // heap-allocation win scales with limit.
    for limit in [5, 10, 50] {
        let mut group = c.benchmark_group(format!("fst_starts_with/limit={limit}"));
        group.throughput(Throughput::Elements(1));
        for (name, prefix) in PREFIX_FIXTURES {
            group.bench_with_input(
                BenchmarkId::new("starts_with", name),
                &(*prefix, limit),
                |b, (prefix, limit)| {
                    b.iter(|| {
                        let hits = auto.search(&cc, black_box(prefix), *limit);
                        black_box(hits);
                    })
                },
            );
        }
        group.finish();
    }
}

fn bench_exact_match(c: &mut Criterion) {
    let Some((auto, cc)) = load_autocomplete() else {
        return;
    };
    let mut group = c.benchmark_group("fst_exact_match");
    group.throughput(Throughput::Elements(1));
    for (name, q) in EXACT_MATCH_FIXTURES {
        group.bench_with_input(BenchmarkId::new("exact_match", name), q, |b, q| {
            b.iter(|| {
                let hit = auto.exact_match(&cc, black_box(q));
                black_box(hit);
            })
        });
    }
    group.finish();
}

fn bench_starts_with_any(c: &mut Criterion) {
    let Some((auto, _)) = load_autocomplete() else {
        return;
    };
    // Country-agnostic walk — exercises every loaded per-country FST.
    // With a single-country dev index this is just one walk; on a
    // worldwide index it scales linearly with countries loaded.
    let mut group = c.benchmark_group("fst_starts_with_any");
    group.throughput(Throughput::Elements(1));
    for (name, prefix) in &[
        ("any_broad_1char", "s"),
        ("any_medium_3char", "syd"),
        ("any_full_word", "sydney"),
    ] {
        group.bench_with_input(BenchmarkId::new("any", name), prefix, |b, prefix| {
            b.iter(|| {
                let hits = auto.search_any(black_box(prefix), 10);
                black_box(hits);
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_starts_with, bench_exact_match, bench_starts_with_any);
criterion_main!(benches);
