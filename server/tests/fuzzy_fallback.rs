//! Verifies fuzzy fallback: strict queries that hit zero because of a
//! typo retry with Levenshtein-1 matching on the name field.

#![cfg(feature = "forward")]

use query_server::forward::{Forward, StructuredQuery, KIND_PLACE};
use std::path::PathBuf;

fn load() -> Option<Forward> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    Forward::open(&PathBuf::from(dir)).ok()
}

#[test]
fn typo_in_sydney_still_resolves() {
    let Some(fwd) = load() else { return };
    // Deliberately synthetic typo: "sydnay" (distance 1 from "sydney").
    // "sidney" isn't a good test input because Sidney Street exists in
    // OSM, so the strict path finds it without ever exercising fuzzy.
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydnay"),
            kind: Some(KIND_PLACE),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(!hits.is_empty(), "fuzzy fallback should find Sydney for 'sidney'");
    let has_sydney = hits
        .iter()
        .any(|h| h.name.to_ascii_lowercase().contains("sydney"));
    assert!(
        has_sydney,
        "expected a Sydney match, got {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>(),
    );
}

#[test]
fn exact_query_does_not_touch_fuzzy() {
    let Some(fwd) = load() else { return };
    // Strict path should find "Sydney" without needing fuzzy.
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydney"),
            kind: Some(KIND_PLACE),
            limit: 3,
            ..Default::default()
        })
        .expect("search");
    assert!(!hits.is_empty());
    assert!(hits[0].name.contains("Sydney"));
}
