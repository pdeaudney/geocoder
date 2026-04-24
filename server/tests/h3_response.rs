//! End-to-end checks for the H3 response-enrichment feature.
//!
//! The unit tests in `h3_cell::tests` cover cell maths and the parser.
//! This file pins the contract at the layer callers actually see:
//! the JSON shape on the `Address` struct, the serde-skip behaviour
//! when H3 is absent, and the deterministic ordering of multi-res
//! maps.

use query_server::h3_cell::{build_h3_map, parse_h3_res};
use query_server::{Address, AddressDetails};
use std::collections::BTreeMap;

fn empty_address() -> Address<'static> {
    Address {
        display_name: None,
        address: AddressDetails::default(),
        confidence: None,
        h3: None,
    }
}

#[test]
fn address_without_h3_omits_field_entirely() {
    // Serde skip_serializing_if on Option::is_none — confirms we aren't
    // leaking a `"h3": null` to callers who never asked for the field.
    let addr = empty_address();
    let json = serde_json::to_string(&addr).expect("serialises");
    assert!(!json.contains("\"h3\""), "no h3 key when None: {json}");
}

#[test]
fn address_with_single_resolution_renders_as_map() {
    let mut addr = empty_address();
    addr.h3 = build_h3_map(-33.87, 151.21, &[9]);
    let json = serde_json::to_string(&addr).expect("serialises");
    // Schema promise: always a map, even for a single resolution.
    // Clients reading `.h3["9"]` don't need a variant check.
    assert!(
        json.contains("\"h3\":{\"9\":\""),
        "single-res still renders as a map keyed by resolution: {json}"
    );
}

#[test]
fn address_with_multi_resolution_renders_sorted_keys() {
    let mut addr = empty_address();
    // Deliberately unsorted input; BTreeMap + serde preserve key order
    // in the JSON output.
    addr.h3 = build_h3_map(-33.87, 151.21, &[12, 7, 9]);
    let json = serde_json::to_string(&addr).expect("serialises");
    // BTreeMap sorts strings lexicographically, so "12" < "7" < "9".
    // This is documented quirky-but-deterministic: clients should read
    // by key, not by position, but the order itself is stable.
    let i12 = json.find("\"12\"").expect("has 12");
    let i7 = json.find("\"7\"").expect("has 7");
    let i9 = json.find("\"9\"").expect("has 9");
    assert!(i12 < i7 && i7 < i9, "keys appear in BTreeMap order: {json}");
}

#[test]
fn h3_cell_ids_roundtrip_through_json() {
    // Serialised JSON → parsed back → the cell ID is still a valid H3
    // cell per h3o. Guards against accidental transformation (e.g.
    // uppercasing) at the boundary.
    let mut addr = empty_address();
    addr.h3 = build_h3_map(-33.8568, 151.2153, &[9]);
    let json = serde_json::to_string(&addr).expect("serialises");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parseable");
    let cell = v["h3"]["9"].as_str().expect("h3.9 is a string");
    // Roundtrip through h3o — the library can only parse a well-formed cell.
    let parsed: h3o::CellIndex = cell.parse().expect("h3o can parse our output");
    assert_eq!(parsed.resolution(), h3o::Resolution::Nine);
}

#[test]
fn parse_h3_res_rejects_resolution_16() {
    // The HTTP layer calls parse_h3_res; a bad value must produce an
    // error string the 400 path can surface. Spot-checking the out-of-
    // range case here keeps the public contract pinned.
    let err = parse_h3_res("16").expect_err("16 is out of range");
    assert!(err.contains("16"));
}

#[test]
fn parse_h3_res_rejects_more_than_four_resolutions() {
    // MAX_RESOLUTIONS is 4; anything longer should bounce at parse
    // time so the request never reaches the enrichment path.
    let err = parse_h3_res("1,2,3,4,5").expect_err("too many");
    assert!(err.contains("max") || err.contains("too many"));
}

#[test]
fn antimeridian_coord_produces_valid_cell() {
    // ±180° longitude should not panic and should yield a cell; this
    // is the edge case most likely to surprise a client hitting the
    // API with data derived from maritime or Pacific sources.
    let map = build_h3_map(0.0, 180.0, &[9]).expect("has a cell");
    assert_eq!(map.len(), 1);
    let cell = map.get("9").expect("resolution 9 present");
    assert_eq!(cell.len(), 15, "H3 cell IDs are 15 hex chars");
    assert!(cell.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn pole_coord_produces_valid_cell() {
    // Cells near the poles are pentagons, not hexagons, but h3o
    // returns a valid cell ID and the wire format is identical to
    // hex-shaped cells. Clients should not care about the shape.
    let map = build_h3_map(90.0, 0.0, &[5]).expect("north pole cell");
    let cell = map.get("5").expect("resolution 5 present");
    assert_eq!(cell.len(), 15);
    assert!(cell.starts_with('8'));
}

#[test]
fn empty_resolutions_yields_none() {
    // Downstream callers use `None` as the "skip enrichment" signal —
    // pin it so we don't accidentally regress to `Some(empty_map)`
    // which would render as `"h3":{}` in the JSON.
    let map: Option<BTreeMap<String, String>> = build_h3_map(-33.87, 151.21, &[]);
    assert!(map.is_none());
}
