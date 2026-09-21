//! Verifies that `Forward` loads per-country tantivy indexes
//! (`tantivy_<cc>/`) alongside the monolithic `tantivy/`, and that the
//! country-filtered query path routes to the right index.

#![cfg(feature = "forward")]

use query_server::forward::{Forward, StructuredQuery};
use std::path::PathBuf;

fn load() -> Option<Forward> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let path = PathBuf::from(dir);
    assert!(
        path.exists(),
        "GEOCODER_INDEX_DIR does not exist: {}",
        path.display()
    );
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
    assert!(
        countries.contains(&*b"au"),
        "expected AU in loaded countries, got {:?}",
        countries
    );
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
fn country_filter_never_returns_another_country() {
    let Some(fwd) = load() else { return };
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydney"),
            country_code: Some("GB"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(hits.iter().all(|h| h.country_code.as_deref() == Some("GB")));
}

#[test]
fn unfiltered_query_fans_out_when_no_default_index_exists() {
    let Some(fwd) = load() else { return };
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydney"),
            limit: 1,
            ..Default::default()
        })
        .expect("search");
    assert!(!hits.is_empty());
    assert_eq!(hits[0].country_code.as_deref(), Some("AU"), "{hits:?}");
}
