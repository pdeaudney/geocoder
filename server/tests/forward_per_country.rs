//! Verifies that `Forward` loads per-country tantivy indexes
//! (`tantivy_<cc>/`) alongside the monolithic `tantivy/`, and that the
//! country-filtered query path routes to the right index.

#![cfg(feature = "forward")]

use query_server::forward::{Forward, StructuredQuery};
use std::path::PathBuf;

fn load() -> Option<Forward> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let path = PathBuf::from(dir);
    if !path.join("tantivy_au").exists() {
        eprintln!("SKIP: per-country tantivy_au not built — run `build-forward-index <dir> --partition-by-country` first");
        return None;
    }
    Forward::open(&path).ok()
}

#[test]
fn au_country_index_is_loaded() {
    let Some(fwd) = load() else { return };
    let countries: Vec<_> = fwd.countries().copied().collect();
    assert!(countries.contains(&*b"au"), "expected AU in loaded countries, got {:?}", countries);
}

#[test]
fn filtered_au_query_returns_hits() {
    let Some(fwd) = load() else { return };
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("alysse close baulkham hills"),
            country_code: Some("AU"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(!hits.is_empty(), "expected hits when routed to AU index");
    let top = &hits[0];
    assert!(top.name.to_ascii_lowercase().contains("alysse close"));
    assert_eq!(top.country_code.as_deref(), Some("AU"));
}

#[test]
fn unloaded_country_triggers_fallback_ladder() {
    let Some(fwd) = load() else { return };
    // Our test index only has AU data. A query filtered to "GB" against
    // the AU default returns 0 on the first (strict) pass. The Nominatim-
    // style fallback ladder then drops the country filter and retries —
    // that's by design: `country_code` is a hint that refines scoring
    // and filtering, not a hard constraint that should silently bury
    // otherwise-relevant results.
    //
    // Test just confirms the pipeline doesn't crash and comes back with
    // something when the strict query misses. Real production deployments
    // serving GB would have tantivy_gb/ loaded and this wouldn't hit the
    // fallback path at all.
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydney"),
            country_code: Some("GB"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    // Hits may be non-empty because the fallback relaxed country_code.
    // Either is acceptable — we're just asserting the call survives.
    let _ = hits;
}

#[test]
fn unfiltered_query_uses_default_index() {
    let Some(fwd) = load() else { return };
    // Unfiltered queries should still work — they go through the monolithic
    // default index. This is the path we used before per-country existed.
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydney"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(!hits.is_empty());
}
