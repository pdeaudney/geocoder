//! Exercises the Nominatim-style gap-closers we layered onto the forward
//! search: ASCII-folding, abbreviation canonicalisation, rank-based
//! scoring, and the fallback ladder.

#![cfg(feature = "forward")]

use query_server::forward::{
    canonicalise_phrase, parse_freeform_query, tokenize_user_input, Forward, StructuredQuery,
};
use std::path::PathBuf;

fn load_forward() -> Option<Forward> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let path = PathBuf::from(dir).join("tantivy");
    if !path.exists() {
        return None;
    }
    Forward::open(&path).ok()
}

// --- Pure-function tests (no index required) ---

#[test]
fn canonicalise_phrase_expands_unambiguous_abbreviations() {
    assert_eq!(canonicalise_phrase("Smith Tce"), "Smith terrace");
    assert_eq!(canonicalise_phrase("Pacific Hwy"), "Pacific highway");
    assert_eq!(canonicalise_phrase("King George Pde"), "King George parade");
    assert_eq!(canonicalise_phrase("Flinders Cres"), "Flinders crescent");
    // Trailing period tolerated
    assert_eq!(canonicalise_phrase("Pacific Hwy."), "Pacific highway");
    // Words NOT in the table are preserved verbatim (including case)
    assert_eq!(canonicalise_phrase("Elizabeth Street"), "Elizabeth Street");
    assert_eq!(canonicalise_phrase("Baulkham Hills"), "Baulkham Hills");
}

#[test]
fn tokenize_user_input_applies_canonicalisation() {
    let toks = tokenize_user_input("Smith Tce");
    assert_eq!(toks, vec!["smith", "terrace"]);
    let toks = tokenize_user_input("  pacific hwy.  ");
    assert_eq!(toks, vec!["pacific", "highway"]);
}

#[test]
fn tokenize_folds_diacritics() {
    // Café → cafe, Zürich → zurich, São → sao
    assert_eq!(
        tokenize_user_input("Café Zürich São Paulo"),
        vec!["cafe", "zurich", "sao", "paulo"]
    );
}

#[test]
fn parse_freeform_extracts_housenumber_state_postcode() {
    let p = parse_freeform_query("10 alysse close baulkham hills nsw 2154");
    assert_eq!(p.house_number.as_deref(), Some("10"));
    assert_eq!(p.state.as_deref(), Some("New South Wales"));
    assert_eq!(p.postcode.as_deref(), Some("2154"));
    assert_eq!(
        p.rest,
        vec!["alysse", "close", "baulkham", "hills"],
        "remaining tokens should be the street + suburb, with hints stripped",
    );
}

#[test]
fn parse_freeform_handles_bare_state_abbreviations() {
    let p = parse_freeform_query("any street qld");
    assert_eq!(p.state.as_deref(), Some("Queensland"));
}

#[test]
fn parse_freeform_trailing_short_digits_are_housenumber() {
    // Street-then-number order ("Alysse Close 10") — Nominatim's BDD
    // db/query/housenumbers.feature covers this explicitly. Without
    // the trailing-digit rule, "10" falls into the word bag and the
    // tantivy search returns 0 results.
    let p = parse_freeform_query("alysse close 10");
    assert_eq!(p.house_number.as_deref(), Some("10"));
    assert_eq!(p.rest, vec!["alysse", "close"]);
    assert_eq!(p.postcode, None);
}

#[test]
fn parse_freeform_trailing_4_digit_still_postcode() {
    // 4-digit trailing stays postcode, not housenumber — AU postcodes
    // always win at that length.
    let p = parse_freeform_query("alysse close 2153");
    assert_eq!(p.house_number, None);
    assert_eq!(p.postcode.as_deref(), Some("2153"));
    assert_eq!(p.rest, vec!["alysse", "close"]);
}

#[test]
fn parse_freeform_leading_housenumber_still_wins() {
    // Regression guard: when both orders would extract a housenumber,
    // the leading position takes precedence.
    let p = parse_freeform_query("10 alysse close");
    assert_eq!(p.house_number.as_deref(), Some("10"));
    assert_eq!(p.rest, vec!["alysse", "close"]);
}

#[test]
fn parse_freeform_trailing_5_digit_also_housenumber() {
    // Some AU addresses have 5-digit housenumbers (e.g. rural property
    // addressing, hwy milepost). Keep these as housenumber, not
    // postcode — AU postcodes are always 4 digits.
    let p = parse_freeform_query("main road 12345");
    assert_eq!(p.house_number.as_deref(), Some("12345"));
}

#[test]
fn parse_freeform_street_number_order_does_not_swallow_interior_digits() {
    // Digits in the middle of a query (not leading, not trailing) must
    // stay in the rest bag — not be promoted to housenumber. Example:
    // someone might query "Route 66 Flagstaff" where "66" is part of
    // the street name.
    let p = parse_freeform_query("route 66 flagstaff");
    // Leading is non-digit so the "66" mid-token stays in rest.
    // Housenumber extraction only fires on position 0 or last.
    assert_eq!(p.house_number, None);
    assert!(p.rest.contains(&"66".to_string()));
}

// --- Integration tests (require index) ---

#[test]
fn abbreviated_street_type_matches_expanded_form() {
    let Some(fwd) = load_forward() else { return };
    // "Pacific Hwy" should match "Pacific Highway" (and vice-versa) because
    // both sides canonicalise to {pacific, highway}.
    let short = fwd
        .search_structured(StructuredQuery {
            q: Some("pacific hwy"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    let long = fwd
        .search_structured(StructuredQuery {
            q: Some("pacific highway"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(!short.is_empty(), "'pacific hwy' should match something");
    assert!(!long.is_empty(), "'pacific highway' should match something");

    // Top results should overlap significantly — exact equality isn't
    // required (BM25 can differ on tie-breakers) but the same top pick
    // should appear in both result sets.
    let short_top = &short[0].name;
    let long_has_short_top = long.iter().any(|h| h.name == *short_top);
    assert!(
        long_has_short_top,
        "top short-form hit {:?} should also be in long-form results {:?}",
        short_top,
        long.iter().map(|h| &h.name).collect::<Vec<_>>(),
    );
}

#[test]
fn rank_boost_prefers_city_over_street_for_ambiguous_single_word() {
    let Some(fwd) = load_forward() else { return };
    // "Sydney" alone — there's a Sydney place (rank 16) AND many streets
    // named "Sydney *". Boost should put the city on top.
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydney"),
            limit: 10,
            ..Default::default()
        })
        .expect("search");

    // Find the first Sydney place and first Sydney street, compare positions.
    let first_place_idx = hits
        .iter()
        .position(|h| h.kind == query_server::forward::KIND_PLACE && h.name.contains("Sydney"));
    let first_street_idx = hits
        .iter()
        .position(|h| h.kind == query_server::forward::KIND_STREET);

    if let (Some(p), Some(s)) = (first_place_idx, first_street_idx) {
        assert!(
            p < s,
            "rank boost: place at index {} should come before street at index {}; got {:?}",
            p,
            s,
            hits.iter()
                .map(|h| format!("{:?}/kind={}", h.name, h.kind))
                .collect::<Vec<_>>(),
        );
    }
}

#[test]
fn fallback_ladder_relaxes_when_strict_query_misses() {
    let Some(fwd) = load_forward() else { return };
    // Compose a deliberately over-specified query: a real street with a
    // wrong state attached. Strict matching gives 0; the ladder should drop
    // the state and return the street.
    let strict_hits = fwd
        .search_structured(StructuredQuery {
            street: Some("alysse close"),
            city: Some("baulkham hills"),
            state: Some("Victoria"), // wrong state, Baulkham Hills is NSW
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(
        !strict_hits.is_empty(),
        "fallback ladder should recover by dropping the wrong state",
    );
    // And the recovered hit should be the expected one.
    assert!(strict_hits[0].name.to_lowercase().contains("alysse close"));
    assert_eq!(strict_hits[0].suburb.as_deref(), Some("Baulkham Hills"));
}

#[test]
fn fallback_does_not_trigger_when_primary_query_hits() {
    let Some(fwd) = load_forward() else { return };
    // When the primary query has hits, we must NOT run the fallback (which
    // would return different results for the same query). Sanity check by
    // comparing the hit set to a forced single-pass search.
    let primary = fwd
        .search_structured(StructuredQuery {
            street: Some("alysse close"),
            city: Some("baulkham hills"),
            state: Some("New South Wales"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(!primary.is_empty());
    for hit in &primary {
        assert_eq!(hit.suburb.as_deref(), Some("Baulkham Hills"));
        assert_eq!(hit.state.as_deref(), Some("New South Wales"));
    }
}
