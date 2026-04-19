//! Exercises the FST exact-match fast-path used by `/search`. Validates
//! that a simple freeform query matching an FST key resolves in ~µs-land
//! via the FST, while structured and non-matching queries still go
//! through tantivy.

#![cfg(feature = "forward")]

use query_server::autocomplete::Autocomplete;
use std::path::PathBuf;
use std::time::Instant;

fn load() -> Option<Autocomplete> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    Autocomplete::open(&PathBuf::from(dir)).ok().flatten()
}

#[test]
fn exact_sydney_hits_the_fst() {
    let Some(a) = load() else { return };
    let hit = a.exact_match(b"au", "sydney").expect("FST should have Sydney");
    assert!(hit.name.to_ascii_lowercase().contains("sydney"));
    // Prominence boost picked the highest-rank Sydney record; should be
    // place (rank 16-19), not a street (rank 26).
    assert!(hit.rank <= 19, "expected place-level rank, got {}", hit.rank);
}

#[test]
fn exact_miss_returns_none() {
    let Some(a) = load() else { return };
    assert!(a.exact_match(b"au", "not_a_real_place_name_xyz").is_none());
}

#[test]
fn too_short_prefix_skips_fst() {
    let Some(a) = load() else { return };
    // Under the min-prefix length. We'd rather fall through to tantivy
    // than pick the FST's arbitrary "a" match.
    assert!(a.exact_match(b"au", "a").is_none());
}

#[test]
fn fast_path_latency_under_20us() {
    let Some(a) = load() else { return };
    // Warm caches
    for _ in 0..5 {
        let _ = a.exact_match(b"au", "sydney");
    }
    let runs = 1000;
    let t0 = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(a.exact_match(b"au", "sydney"));
    }
    let per = t0.elapsed() / runs;
    eprintln!("FST exact_match('sydney') avg: {per:?}");
    assert!(per.as_micros() < 20, "FST fast-path should be <20µs, got {per:?}");
}
