//! Sanity-check the G-NAF-derived postcode lookup. Skipped unless
//! `GEOCODER_INDEX_DIR` points at a directory containing both
//! `postcode_lookup.bin` and `postcode_lookup_strings.bin`.

use query_server::postcode::{normalise_locality, PostcodeLookup};
use std::path::PathBuf;

fn load() -> Option<PostcodeLookup> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    PostcodeLookup::open(&PathBuf::from(dir)).ok().flatten()
}

#[test]
fn normalise_is_stable() {
    assert_eq!(normalise_locality("Baulkham Hills"), "baulkham hills");
    assert_eq!(normalise_locality("  BAULKHAM   HILLS  "), "baulkham hills");
    assert_eq!(normalise_locality("St. Kilda"), "st kilda");
}

#[test]
fn baulkham_hills_nsw_returns_2153() {
    // G-NAF ADDRESS_DETAIL: all addresses with locality "Baulkham Hills"
    // (incl. every house on Alysse Close) are postcode 2153. A common
    // misconception is that it's 2154 — that's Castle Hill (separate
    // locality that happens to adjoin). This test pins the truth.
    let Some(lookup) = load() else { return };
    let pc = lookup.postcode("NSW", "Baulkham Hills");
    assert_eq!(pc, Some("2153"));
}

#[test]
fn major_cbd_localities_resolve() {
    let Some(lookup) = load() else { return };
    // A mix of cities that should be unambiguously indexed. "The" postcode
    // is the modal one per G-NAF — for CBDs this is well-defined.
    let cases = [
        ("NSW", "Sydney", "2000"),
        ("VIC", "Melbourne", "3000"),
        ("QLD", "Brisbane City", "4000"),
        ("SA", "Adelaide", "5000"),
        ("WA", "Perth", "6000"),
        ("TAS", "Hobart", "7000"),
        ("NT", "Darwin City", "0800"),
        // G-NAF doesn't have a locality literally named "Canberra" — the
        // ACT is split into districts like "Canberra Central" which all
        // fall under 2600.
        ("ACT", "Canberra Central", "2601"),
    ];
    for (state, locality, expected) in cases {
        let got = lookup.postcode(state, locality);
        assert_eq!(
            got,
            Some(expected),
            "{state}/{locality}: expected {expected:?}, got {got:?}",
        );
    }
}

#[test]
fn case_insensitivity_and_punctuation() {
    let Some(lookup) = load() else { return };
    let a = lookup.postcode("NSW", "baulkham hills");
    let b = lookup.postcode("nsw", "BAULKHAM HILLS");
    let c = lookup.postcode("NSW", "Baulkham  Hills");
    assert_eq!(a, b);
    assert_eq!(a, c);
    assert!(a.is_some());
}

#[test]
fn unknown_locality_returns_none() {
    let Some(lookup) = load() else { return };
    assert!(lookup.postcode("NSW", "Not A Real Suburb Anywhere").is_none());
    assert!(lookup.postcode("", "Sydney").is_none());
    assert!(lookup.postcode("NSW", "").is_none());
}
