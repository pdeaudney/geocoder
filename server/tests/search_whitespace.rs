#![cfg(feature = "forward")]

use query_server::forward::{
    canonicalise_phrase, Forward, ForwardSchema, StructuredQuery, KIND_ADDRESS,
};
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index};

fn fixture() -> (tempfile::TempDir, Forward) {
    let dir = tempfile::tempdir().unwrap();
    let s = ForwardSchema::build();
    let index = Index::create_in_dir(dir.path(), s.schema.clone()).unwrap();
    index.tokenizers().register(
        "geocoder",
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(AsciiFoldingFilter)
            .filter(LowerCaser)
            .build(),
    );
    let mut writer = index.writer(15_000_000).unwrap();
    for (street, number, unit, country, lat) in [
        ("Keats Street", "12", "3", "AU", -33.0),
        ("New Brunswick Drive", "12", "3", "AU", -33.1),
        ("École Road", "12", "3", "AU", -33.2),
        ("New Port Lane", "12", "3", "AU", -33.3),
        ("Newport Lane", "12", "3", "AU", -33.4),
        ("Newport Lane", "99", "3", "AU", -33.4),
        ("New Portlane", "99", "3", "AU", -33.5),
        ("Foreign Road", "12", "3", "US", 40.0),
        ("Orchard Road Apt 4", "12", "", "AU", -33.6),
        ("Orchard Road 2001", "12", "", "AU", -33.7),
        ("Orchard Road NSW", "12", "", "AU", -33.8),
    ]
    .into_iter()
    .chain((1..=5).map(|n| ("Poplar Road", "88", "", "AU", -34.0 - f64::from(n) * 0.001)))
    {
        writer.add_document(doc!(s.name => canonicalise_phrase(street), s.name_raw => street,
            s.housenumber => number, s.housenumber_raw => number, s.unit => unit, s.unit_raw => unit,
            s.postcode => "2000", s.suburb => "Testville", s.state => "State", s.country_code => country,
            s.kind => KIND_ADDRESS, s.rank => 26u64, s.lat => lat, s.lng => 151.0)).unwrap();
    }
    writer.commit().unwrap();
    let forward = Forward::open(dir.path()).unwrap();
    (dir, forward)
}

fn query(street: &str) -> StructuredQuery<'_> {
    StructuredQuery {
        street: Some(street),
        housenumber: Some("12"),
        unit: Some("3"),
        city: Some("Testville"),
        state: Some("State"),
        postcode: Some("2000"),
        country_code: Some("AU"),
        kind: Some(KIND_ADDRESS),
        limit: 5,
        ..Default::default()
    }
}

#[test]
fn repairs_a_split_or_joined_word_with_all_address_constraints() {
    let (_dir, fwd) = fixture();
    for (typed, expected) in [
        ("Ke ats Street", "Keats Street"),
        ("NewBrunswick Drive", "New Brunswick Drive"),
        ("Éc ole Road", "École Road"),
    ] {
        let hits = fwd.search_structured(query(typed)).unwrap();
        assert_eq!(hits[0].name, expected);
        assert_eq!(hits[0].housenumber.as_deref(), Some("12"));
        assert_eq!(hits[0].unit.as_deref(), Some("3"));
        assert_eq!(hits[0].country_code.as_deref(), Some("AU"));
    }
}

#[test]
fn preserves_an_existing_complete_address_instead_of_joining_its_name() {
    let (_dir, fwd) = fixture();
    let hits = fwd.search_structured(query("New Port Lane")).unwrap();
    assert_eq!(hits[0].name, "New Port Lane");
}

#[test]
fn ambiguous_repairs_and_full_candidate_pages_are_declined_even_at_limit_one() {
    let (_dir, fwd) = fixture();
    let hits = fwd
        .search_structured(StructuredQuery {
            housenumber: Some("99"),
            limit: 1,
            ..query("New Port Lane")
        })
        .unwrap();
    assert!(
        hits.iter().all(|h| h.housenumber.as_deref() != Some("99")),
        "{hits:?}"
    );
    let hits = fwd
        .search_structured(StructuredQuery {
            housenumber: Some("88"),
            unit: None,
            limit: 1,
            ..query("Pop lar Road")
        })
        .unwrap();
    assert!(hits.is_empty(), "{hits:?}");
}

#[test]
fn repairs_never_relax_house_unit_postcode_city_state_or_country() {
    let (_dir, fwd) = fixture();
    for q in [
        StructuredQuery {
            housenumber: Some("13"),
            ..query("Ke ats Street")
        },
        StructuredQuery {
            unit: Some("4"),
            ..query("Ke ats Street")
        },
        StructuredQuery {
            postcode: Some("2001"),
            ..query("Ke ats Street")
        },
        StructuredQuery {
            city: Some("Othercity"),
            ..query("Ke ats Street")
        },
        StructuredQuery {
            state: Some("Otherstate"),
            ..query("Ke ats Street")
        },
        query("For eign Road"),
    ] {
        let hits = fwd.search_structured(q).unwrap();
        assert!(hits.is_empty(), "{hits:?}");
    }
}

#[test]
fn missing_context_and_mixed_street_queries_do_not_enter_repair() {
    let (_dir, fwd) = fixture();
    for q in [
        StructuredQuery {
            city: None,
            postcode: None,
            ..query("Ke ats Street")
        },
        StructuredQuery {
            q: Some("Ke ats"),
            ..query("Ke ats Street")
        },
    ] {
        assert!(fwd.search_structured(q).unwrap().is_empty());
    }
}

fn freeform(text: &str) -> StructuredQuery<'_> {
    StructuredQuery {
        q: Some(text),
        country_code: Some("AU"),
        limit: 5,
        ..Default::default()
    }
}

#[test]
fn repairs_freeform_street_spans_and_preserves_house_unit_and_context() {
    let (_dir, fwd) = fixture();
    for (text, expected) in [
        (
            "Unit 3, 12 Ke ats Street, Testville, State, 2000",
            "Keats Street",
        ),
        (
            "3/12 NewBrunswick Drive, Testville, State, 2000",
            "New Brunswick Drive",
        ),
        ("#3, 12 Éc ole Road, Testville, State, 2000", "École Road"),
    ] {
        let hits = fwd.search_structured(freeform(text)).unwrap();
        assert!(!hits.is_empty(), "{text}");
        assert_eq!(hits[0].name, expected);
        assert_eq!(hits[0].housenumber.as_deref(), Some("12"));
        assert_eq!(hits[0].unit.as_deref(), Some("3"));
        assert_eq!(hits[0].postcode.as_deref(), Some("2000"));
        assert_eq!(hits[0].suburb.as_deref(), Some("Testville"));
    }
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("12 Ke ats Street"),
            street: None,
            housenumber: None,
            ..query("unused")
        })
        .unwrap();
    assert_eq!(hits[0].name, "Keats Street");
}

#[test]
fn freeform_repairs_decline_conflicts_unexplained_sections_and_ambiguous_boundaries() {
    let (_dir, fwd) = fixture();
    for q in [
        freeform("Unit 3, 13 Ke ats Street, Testville, State, 2000"),
        freeform("Unit 4, 12 Ke ats Street, Testville, State, 2000"),
        freeform("Unit 3, 12 Ke ats Street, Othercity, State, 2000"),
        freeform("Unit 3, 12 Ke ats Street, Testville, Otherstate, 2000"),
        freeform("Unit 3, 12 Ke ats Street, Testville, State, 2001"),
        freeform("Unit 3, 12 Ke ats Street, Testville, State, 2000, Extra"),
        freeform("3/12 Ke ats Street Testville State 2000"),
        freeform("3/12 Ke ats Street"),
        freeform("Unit 3, 12 Ke ats Street, Testville Extra, State, 2000"),
        freeform("Unit 3, 12 For eign Road, Testville, State, 2000"),
        freeform("12 Or chard Road Apt 4, Testville, State, 2000"),
        freeform("12 Or chard Road 2001, Testville, State"),
        freeform("12 Or chard Road NSW, Testville, State, 2000"),
        StructuredQuery {
            housenumber: Some("13"),
            ..freeform("3/12 Ke ats Street, Testville, State, 2000")
        },
        StructuredQuery {
            unit: Some("4"),
            ..freeform("3/12 Ke ats Street, Testville, State, 2000")
        },
        StructuredQuery {
            city: Some("Othercity"),
            ..freeform("3/12 Ke ats Street, Testville, State, 2000")
        },
        StructuredQuery {
            postcode: Some("2001"),
            ..freeform("3/12 Ke ats Street, Testville, State, 2000")
        },
    ] {
        let hits = fwd.search_structured(q.clone()).unwrap();
        assert!(hits.is_empty(), "{q:?}: {hits:?}");
    }
}

#[test]
fn freeform_preserves_valid_names_and_declines_competing_repairs_and_full_pages() {
    let (_dir, fwd) = fixture();
    let hits = fwd
        .search_structured(freeform("3/12 New Port Lane, Testville, State, 2000"))
        .unwrap();
    assert_eq!(hits[0].name, "New Port Lane");
    for text in [
        "3/99 New Port Lane, Testville, State, 2000",
        "88 Pop lar Road, Testville, State, 2000",
    ] {
        let hits = fwd
            .search_structured(StructuredQuery {
                limit: 1,
                ..freeform(text)
            })
            .unwrap();
        assert!(hits.is_empty(), "{text}: {hits:?}");
    }
}
