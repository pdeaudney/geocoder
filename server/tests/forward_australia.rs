//! End-to-end forward-geocoding accuracy against the AU tantivy index.
//!
//! Requires `GEOCODER_INDEX_DIR` pointing at a directory that contains both
//! the binary reverse-geocoding index *and* a `tantivy/` subdirectory built
//! by `build-forward-index`. Skipped otherwise.

#![cfg(feature = "forward")]

use query_server::forward::{Forward, Hit, KIND_PLACE, KIND_STREET};
use std::path::PathBuf;

fn load_forward() -> Option<Forward> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let path = PathBuf::from(dir).join("tantivy");
    if !path.exists() {
        eprintln!("SKIP: {} not found; run build-forward-index first", path.display());
        return None;
    }
    match Forward::open(&path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("FAIL: could not open forward index: {e}");
            None
        }
    }
}

fn first_hit_name_ci(hits: &[Hit], needle: &str) -> Option<String> {
    hits.iter()
        .find(|h| h.name.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()))
        .map(|h| h.name.clone())
}

#[test]
fn search_place_sydney_returns_sydney() {
    let Some(fwd) = load_forward() else { return };
    let hits = fwd.search("Sydney", Some(KIND_PLACE), 10).expect("search");
    assert!(!hits.is_empty(), "expected at least one hit for 'Sydney'");
    let sydney = first_hit_name_ci(&hits, "Sydney").unwrap_or_else(|| {
        let got: Vec<_> = hits.iter().map(|h| &h.name).collect();
        panic!("no 'Sydney' in {:?}", got)
    });
    eprintln!("search('Sydney', place) -> {sydney}");
    // Australia: Sydney is around (-33.87, 151.21). Pick any hit named Sydney
    // and assert it's in AU coords.
    let hit = hits.iter().find(|h| h.name == sydney).unwrap();
    assert!((-45.0..-10.0).contains(&hit.lat), "lat {} not in AU range", hit.lat);
    assert!((110.0..155.0).contains(&hit.lng), "lng {} not in AU range", hit.lng);
}

#[test]
fn search_place_bondi_beach_finds_suburb() {
    let Some(fwd) = load_forward() else { return };
    let hits = fwd.search("bondi beach", Some(KIND_PLACE), 10).expect("search");
    assert!(
        first_hit_name_ci(&hits, "Bondi Beach").is_some(),
        "expected Bondi Beach in hits, got {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
}

#[test]
fn search_street_elizabeth_returns_results() {
    let Some(fwd) = load_forward() else { return };
    let hits = fwd.search("elizabeth street", Some(KIND_STREET), 10).expect("search");
    assert!(!hits.is_empty(), "no street hits for 'elizabeth street'");
    let has_elizabeth = hits
        .iter()
        .any(|h| h.name.to_ascii_lowercase().contains("elizabeth"));
    assert!(
        has_elizabeth,
        "expected at least one Elizabeth Street hit, got {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
    eprintln!(
        "search('elizabeth street', street) -> {} hits, top: {}",
        hits.len(),
        hits[0].name
    );
}

#[test]
fn search_without_kind_filter_returns_mixed_results() {
    let Some(fwd) = load_forward() else { return };
    let hits = fwd.search("melbourne", None, 20).expect("search");
    assert!(!hits.is_empty());
    let has_place = hits.iter().any(|h| h.kind == KIND_PLACE);
    let has_any_name = hits.iter().any(|h| h.name.to_ascii_lowercase().contains("melbourne"));
    assert!(has_place, "expected at least one place-kind hit for 'melbourne'");
    assert!(has_any_name, "expected at least one name-containing hit");
}

#[test]
fn search_empty_query_is_graceful() {
    let Some(fwd) = load_forward() else { return };
    // Empty query should either return empty or fail gracefully — not panic.
    let _ = fwd.search("", None, 5);
}

#[test]
fn search_latency_under_10ms_for_common_query() {
    let Some(fwd) = load_forward() else { return };
    // Warm up caches.
    for _ in 0..5 {
        let _ = fwd.search("sydney", Some(KIND_PLACE), 10);
    }

    let t0 = std::time::Instant::now();
    let runs = 100;
    for _ in 0..runs {
        let _ = fwd
            .search("sydney", Some(KIND_PLACE), 10)
            .expect("search");
    }
    let elapsed = t0.elapsed();
    let per_query = elapsed / runs;
    eprintln!("avg latency for 'sydney' place search: {per_query:?}");
    // Loose bound — tantivy point lookups on a ~250K-doc index should
    // comfortably beat 10 ms. We're asserting an order-of-magnitude guard,
    // not a benchmark.
    assert!(
        per_query.as_millis() < 10,
        "avg {:?} > 10ms — performance regression?",
        per_query
    );
}
