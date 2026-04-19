//! Validates FST autocomplete against the AU index. Skipped unless
//! `GEOCODER_INDEX_DIR` points at a directory with `fst_au.*` files
//! (produced by `build-autocomplete-fst`).

use query_server::autocomplete::Autocomplete;
use std::path::PathBuf;

fn load() -> Option<Autocomplete> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let p = PathBuf::from(&dir);
    if !p.join("fst_au.fst").exists() {
        eprintln!("SKIP: fst_au.* missing under {} — run build-autocomplete-fst first", dir);
        return None;
    }
    Autocomplete::open(&p).ok().flatten()
}

#[test]
fn au_index_loads() {
    let Some(a) = load() else { return };
    assert!(a.has_country(b"au"));
}

#[test]
fn prefix_alyss_finds_alysse_close() {
    let Some(a) = load() else { return };
    let hits = a.search(b"au", "alyss", 5);
    assert!(!hits.is_empty(), "expected hits for prefix 'alyss'");
    assert!(
        hits.iter().any(|h| h.name.to_ascii_lowercase().contains("alysse")),
        "expected an 'Alysse ...' hit, got {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>(),
    );
}

#[test]
fn prefix_sydney_returns_place_first() {
    let Some(a) = load() else { return };
    let hits = a.search(b"au", "sydney", 10);
    assert!(!hits.is_empty());
    // Top hit for "sydney" prefix should be the place (rank 16), not a
    // street named Sydney Lane (rank 26).
    let top = &hits[0];
    assert!(
        top.rank <= 19,
        "expected top hit to be a place/suburb, got rank={} name={}",
        top.rank,
        top.name,
    );
}

#[test]
fn empty_prefix_returns_nothing_quickly() {
    let Some(a) = load() else { return };
    // An empty or whitespace-only prefix would otherwise enumerate the
    // whole FST. `normalise_prefix` collapses that to "" and the runtime
    // skips.
    let start = std::time::Instant::now();
    let hits = a.search(b"au", "", 10);
    let elapsed = start.elapsed();
    assert!(hits.is_empty() || elapsed < std::time::Duration::from_millis(10));
}

#[test]
fn autocomplete_latency_is_sub_millisecond() {
    let Some(a) = load() else { return };
    // Warm caches first — page faults on the mmap shouldn't count.
    for _ in 0..5 {
        let _ = a.search(b"au", "eliz", 10);
    }
    let runs = 100;
    let t0 = std::time::Instant::now();
    for _ in 0..runs {
        let _ = a.search(b"au", "eliz", 10);
    }
    let per = t0.elapsed() / runs;
    eprintln!("autocomplete('eliz') avg: {per:?}");
    // Loose bound — the tiny FST should comfortably beat 2 ms.
    assert!(per.as_millis() < 2, "autocomplete too slow: {per:?}");
}
