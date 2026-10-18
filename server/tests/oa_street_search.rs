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
            s.housenumber => "3577",
            s.housenumber_raw => "3577",
            s.unit => "",
            s.unit_raw => "",
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

    for q in ["3577 W 37 St", "3577 West 37 Street"] {
        let name = top_name(
            &fwd,
            StructuredQuery {
                q: Some(q),
                limit: 5,
                ..Default::default()
            },
        );
        assert_eq!(name.as_deref(), Some("West 37 Street"), "q={q}");
    }
}
