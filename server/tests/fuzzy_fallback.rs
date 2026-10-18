//! Verifies spelling correction: queries that hit zero because of a typo
//! retry with each rare word replaced by a common indexed term within one
//! or two edits, and postcodes that pick the right country shard.

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

fn has_country(fwd: &Forward, cc: &[u8; 2]) -> bool {
    fwd.countries().any(|c| c == cc)
}

#[test]
fn typos_in_an_address_still_resolve() {
    let Some(fwd) = load() else { return };
    if !has_country(&fwd, b"us") {
        eprintln!("SKIP: no US shard");
        return;
    }
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("3577 Jakson Stret San Fransisco"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    let top = hits.first().expect("corrected query finds the address");
    assert_eq!(top.housenumber.as_deref(), Some("3577"));
    assert!(
        top.name.to_ascii_lowercase().contains("jackson"),
        "top: {}",
        top.name
    );
}

#[test]
fn correction_never_drops_a_correctly_typed_word() {
    let Some(fwd) = load() else { return };
    if has_country(&fwd, b"jp") {
        eprintln!("SKIP: index has Japan");
        return;
    }
    // "sendai" corrects to "senda", but no result also names "japan".
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sendai, japan"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(
        hits.is_empty(),
        "expected no guess, got {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
}

#[test]
fn postcode_picks_country_and_city_for_a_poi() {
    let Some(fwd) = load() else { return };
    if !has_country(&fwd, b"us") || !has_country(&fwd, b"au") {
        eprintln!("SKIP: needs US and AU shards");
        return;
    }
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("starbucks 10010"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    let top = hits.first().expect("starbucks near 10010");
    assert_eq!(top.country_code.as_deref(), Some("US"));
    assert_eq!(top.suburb.as_deref(), Some("New York"));
}
