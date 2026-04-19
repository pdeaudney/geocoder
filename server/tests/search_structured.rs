//! Verifies #20: multi-field structured forward geocoding resolves the
//! "10 Alysse Close Baulkham Hills NSW" kind of query that the single-field
//! tantivy index could not.

#![cfg(feature = "forward")]

use query_server::forward::{Forward, Hit, StructuredQuery, KIND_STREET};
use std::path::PathBuf;

fn load_forward() -> Option<Forward> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let path = PathBuf::from(dir).join("tantivy");
    if !path.exists() {
        return None;
    }
    Forward::open(&path).ok()
}

fn dump(label: &str, hits: &[Hit]) {
    eprintln!("\n{label} ({} hits)", hits.len());
    for (i, h) in hits.iter().take(5).enumerate() {
        eprintln!(
            "  {i}. name={:?} suburb={:?} state={:?} score={:.2} ({:.4}, {:.4})",
            h.name, h.suburb, h.state, h.score, h.lat, h.lng
        );
    }
}

#[test]
fn freeform_alysse_close_baulkham_hills_nsw_finds_the_street() {
    let Some(fwd) = load_forward() else { return };

    // The core regression: this freeform query should now resolve.
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("10 alysse close baulkham hills nsw"),
            kind: Some(KIND_STREET),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    dump("freeform 10 alysse close baulkham hills nsw", &hits);

    let top = hits
        .first()
        .expect("expected at least one hit for alysse close in baulkham hills");
    assert!(
        top.name.to_lowercase().contains("alysse close"),
        "top hit {:?} should be Alysse Close",
        top.name
    );
    assert_eq!(
        top.suburb.as_deref(),
        Some("Baulkham Hills"),
        "top hit's suburb should be Baulkham Hills",
    );
    assert_eq!(top.state.as_deref(), Some("New South Wales"));
}

#[test]
fn structured_fields_compose_correctly() {
    let Some(fwd) = load_forward() else { return };

    let hits = fwd
        .search_structured(StructuredQuery {
            street: Some("alysse close"),
            city: Some("baulkham hills"),
            state: Some("new south wales"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    dump("structured alysse close / baulkham hills / NSW", &hits);

    assert!(!hits.is_empty());
    let top = &hits[0];
    assert!(top.name.to_lowercase().contains("alysse close"));
    assert_eq!(top.suburb.as_deref(), Some("Baulkham Hills"));
}

#[test]
fn main_street_name_disambiguates_by_suburb() {
    let Some(fwd) = load_forward() else { return };

    // "Main Street" exists in many AU suburbs — adding a suburb filter must
    // narrow the result set to just that suburb's record. We don't pin the
    // exact suburb (OSM may not have "Main Street" in every expected
    // suburb), but we do assert the suburb context is honoured when set.
    let unfiltered = fwd
        .search_structured(StructuredQuery {
            street: Some("main street"),
            limit: 10,
            ..Default::default()
        })
        .expect("search");
    dump("main street (unfiltered)", &unfiltered);

    let lismore = fwd
        .search_structured(StructuredQuery {
            street: Some("main street"),
            city: Some("lismore"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    dump("main street in Lismore", &lismore);

    // Unfiltered should have more candidates than the suburb-filtered view.
    // Not a strict >, because there might be exactly 1 "Main Street" in OSM
    // AU — but typically there are many.
    assert!(unfiltered.len() >= lismore.len());
    for hit in &lismore {
        if let Some(suburb) = hit.suburb.as_deref() {
            assert!(
                suburb.to_lowercase().contains("lismore"),
                "suburb filter returned non-Lismore hit: {:?}",
                hit,
            );
        }
    }
}

#[test]
fn country_code_filter_restricts_to_au() {
    let Some(fwd) = load_forward() else { return };
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydney"),
            country_code: Some("AU"),
            limit: 10,
            ..Default::default()
        })
        .expect("search");
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(hit.country_code.as_deref(), Some("AU"), "{:?}", hit);
    }
}
