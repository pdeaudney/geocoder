//! OpenAddresses US text is expanded at build time ("JACKSON ST" ->
//! "Jackson Street"). Users still type "St", so forward search must match
//! the expanded value from either form.
//!
//! The document mirrors the fields `forward::address_doc` writes for an
//! OpenAddresses point, with the name run through the same OA normaliser
//! and `canonicalise_phrase` the build uses.

#![cfg(feature = "forward")]

use query_server::forward::{
    canonicalise_phrase, Forward, ForwardSchema, StructuredQuery, KIND_ADDRESS,
};
use query_server::openaddresses::normalise_street;
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index};

fn build_index(dir: &std::path::Path, raw_street: &str) {
    build_address_index(dir, raw_street, "3577", "");
}

fn build_address_index(dir: &std::path::Path, raw_street: &str, number: &str, unit: &str) {
    let s = ForwardSchema::build();
    let index = Index::create_in_dir(dir, s.schema.clone()).expect("create index");
    index.tokenizers().register(
        "geocoder",
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(AsciiFoldingFilter)
            .filter(LowerCaser)
            .build(),
    );
    let street = normalise_street(raw_street);
    let mut writer = index.writer(15_000_000).expect("writer");
    writer
        .add_document(doc!(
            s.name => canonicalise_phrase(&street),
            s.alt_name => "",
            s.name_raw => street.as_ref(),
            s.suburb => canonicalise_phrase("San Francisco"),
            s.state => canonicalise_phrase("California"),
            s.country_code => "US",
            s.kind => KIND_ADDRESS,
            s.category => "",
            s.rank => 25u64,
            s.importance => 0u64,
            s.lat => 37.7648,
            s.lng => -122.4352,
            s.housenumber => number.to_ascii_lowercase(),
            s.housenumber_raw => number,
            s.unit => unit.to_ascii_lowercase(),
            s.unit_raw => unit,
            s.postcode => "94114",
        ))
        .expect("add doc");
    writer.commit().expect("commit");
}

fn top_name(fwd: &Forward, q: StructuredQuery<'_>) -> Option<String> {
    fwd.search_structured(q)
        .expect("search")
        .into_iter()
        .next()
        .map(|h| h.name)
}

#[test]
fn st_and_street_queries_match_an_expanded_oa_street() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_index(dir.path(), "JACKSON ST");
    let fwd = Forward::open(dir.path()).expect("open forward index");

    for q in [
        "3577 Jackson St",
        "3577 Jackson Street",
        "3577 jackson st san francisco",
        "3577 JACKSON ST",
    ] {
        let name = top_name(
            &fwd,
            StructuredQuery {
                q: Some(q),
                limit: 5,
                ..Default::default()
            },
        );
        assert_eq!(name.as_deref(), Some("Jackson Street"), "q={q}");
    }

    for street in ["Jackson St", "Jackson Street"] {
        let name = top_name(
            &fwd,
            StructuredQuery {
                street: Some(street),
                housenumber: Some("3577"),
                limit: 5,
                ..Default::default()
            },
        );
        assert_eq!(name.as_deref(), Some("Jackson Street"), "street={street}");
    }
}

#[test]
fn ordinal_and_directional_queries_match_an_expanded_oa_street() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_index(dir.path(), "W 37 ST");
    let fwd = Forward::open(dir.path()).expect("open forward index");

    for q in ["3577 W 37 St", "3577 West 37 Street", "3577 W 37th St"] {
        let name = top_name(
            &fwd,
            StructuredQuery {
                q: Some(q),
                limit: 5,
                ..Default::default()
            },
        );
        assert_eq!(name.as_deref(), Some("West 37th Street"), "q={q}");
    }
}

#[test]
fn trailing_directional_codes_match_full_word_queries() {
    for (code, full) in [("E", "East"), ("W", "West"), ("NE", "Northeast")] {
        let dir = tempfile::tempdir().unwrap();
        let street = format!("PARKES PL {code}");
        build_index(dir.path(), &street);
        let forward = Forward::open(dir.path()).unwrap();
        for suffix in [code, full] {
            let street_query = format!("Parkes Place {suffix}");
            let freeform = format!("3577 {street_query}");
            for query in [
                StructuredQuery {
                    street: Some(&street_query),
                    housenumber: Some("3577"),
                    limit: 5,
                    ..Default::default()
                },
                StructuredQuery {
                    q: Some(&freeform),
                    limit: 5,
                    ..Default::default()
                },
            ] {
                assert_eq!(
                    top_name(&forward, query),
                    Some(format!("Parkes Place {code}")),
                    "suffix={suffix}"
                );
            }
        }
    }
}

#[test]
fn controlled_house_suffix_unit_range_and_missing_number_searches() {
    for (number, unit) in [("12A", "3B"), ("255-257", "")] {
        let dir = tempfile::tempdir().unwrap();
        build_address_index(dir.path(), "JACKSON ST", number, unit);
        let forward = Forward::open(dir.path()).unwrap();
        let hits = forward
            .search_structured(StructuredQuery {
                street: Some("Jackson Street"),
                housenumber: Some(number),
                unit: (!unit.is_empty()).then_some(unit),
                country_code: Some("US"),
                kind: Some(KIND_ADDRESS),
                limit: 5,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits[0].housenumber.as_deref(), Some(number));
        if !unit.is_empty() {
            assert_eq!(hits[0].unit.as_deref(), Some(unit));
        }
        for (wrong_number, wrong_unit) in [("999999", ""), ("12B", "3B"), (number, "99Z")] {
            let hits = forward
                .search_structured(StructuredQuery {
                    street: Some("Jackson Street"),
                    housenumber: Some(wrong_number),
                    unit: (!wrong_unit.is_empty()).then_some(wrong_unit),
                    country_code: Some("US"),
                    kind: Some(KIND_ADDRESS),
                    limit: 5,
                    ..Default::default()
                })
                .unwrap();
            assert!(
                hits.is_empty(),
                "nonexistent number/suffix/unit must not be an exact address: {hits:?}"
            );
        }
    }
}
