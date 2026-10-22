//! Country merges must preserve proximity and exact matches in either index layout.
#![cfg(feature = "forward")]

use query_server::forward::{
    canonicalise_phrase, BiasCoord, Forward, ForwardSchema, StructuredQuery, KIND_PLACE,
    KIND_STREET,
};
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index};

fn fixture(partitioned: bool) -> (tempfile::TempDir, Forward) {
    let dir = tempfile::tempdir().unwrap();
    let s = ForwardSchema::build();
    let documents = [
        (
            "US",
            vec![
                doc!(s.name => "Cambridge", s.name_raw => "Cambridge", s.suburb => "Cambridge",
                s.state => "Massachusetts", s.kind => KIND_PLACE, s.rank => 16u64,
                s.importance => 200u64, s.lat => 42.3656, s.lng => -71.104),
                doc!(s.name => canonicalise_phrase("West 26th Street"), s.name_raw => "West 26th Street",
                s.suburb => "New York", s.state => "New York", s.kind => KIND_STREET,
                s.rank => 26u64, s.lat => 40.745, s.lng => -73.997),
            ],
        ),
        (
            "GB",
            vec![
                doc!(s.name => "Cambridge", s.name_raw => "Cambridge", s.suburb => "Cambridge",
                s.state => "England", s.kind => KIND_PLACE, s.rank => 16u64,
                s.importance => 255u64, s.lat => 52.2055, s.lng => 0.1187),
            ],
        ),
        (
            "CA",
            vec![
                doc!(s.name => canonicalise_phrase("26th Street West"), s.name_raw => "26th Street West",
                s.suburb => "Saskatoon", s.state => "Saskatchewan", s.kind => KIND_STREET,
                s.rank => 26u64, s.lat => 52.13, s.lng => -106.67),
            ],
        ),
    ];
    let add_country =
        |path: &std::path::Path, countries: Vec<(&str, Vec<tantivy::TantivyDocument>)>| {
            std::fs::create_dir_all(path).unwrap();
            let index = Index::create_in_dir(path, s.schema.clone()).unwrap();
            index.tokenizers().register(
                "geocoder",
                TextAnalyzer::builder(SimpleTokenizer::default())
                    .filter(AsciiFoldingFilter)
                    .filter(LowerCaser)
                    .build(),
            );
            let mut writer = index.writer(15_000_000).unwrap();
            for (cc, docs) in countries {
                for mut document in docs {
                    document.add_text(s.country_code, cc);
                    writer.add_document(document).unwrap();
                }
            }
            writer.commit().unwrap();
        };
    if partitioned {
        for (cc, docs) in documents {
            add_country(
                &dir.path().join(format!("tantivy_{}", cc.to_lowercase())),
                vec![(cc, docs)],
            );
        }
    } else {
        add_country(&dir.path().join("tantivy"), documents.into_iter().collect());
    }
    let forward = Forward::open(dir.path()).unwrap();
    (dir, forward)
}

#[test]
fn country_merge_honours_bias_before_global_importance() {
    let (_dir, forward) = fixture(true);
    for country_code in [None, Some("GB,US,CA")] {
        for (lat, lng, expected) in [(42.36, -71.06, "US"), (51.51, -0.13, "GB")] {
            let hits = forward
                .search_structured(StructuredQuery {
                    q: Some("Cambridge"),
                    country_code,
                    bias: Some(BiasCoord::try_new(lat, lng).unwrap()),
                    limit: 1,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(hits[0].country_code.as_deref(), Some(expected));
        }
    }
}

#[test]
fn country_list_keeps_exact_matches_ahead_of_relaxed_matches() {
    for partitioned in [false, true] {
        let (_dir, forward) = fixture(partitioned);
        for country_code in ["US,CA", " ca, US,us, "] {
            let hits = forward
                .search_structured(StructuredQuery {
                    q: Some("West 26th Street, New York"),
                    country_code: Some(country_code),
                    limit: 5,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(hits.len(), 1, "relaxed Canadian matches must be excluded");
            assert_eq!(hits[0].country_code.as_deref(), Some("US"));
        }
        let hits = forward
            .search_structured(StructuredQuery {
                q: Some("West 26th Street, New York"),
                country_code: Some("CA,NZ"),
                limit: 5,
                ..Default::default()
            })
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits
            .iter()
            .all(|hit| hit.country_code.as_deref() == Some("CA")));
    }
}

#[test]
fn country_list_preserves_country_suffixes() {
    for partitioned in [false, true] {
        let (_dir, forward) = fixture(partitioned);
        let hits = forward
            .search_structured(StructuredQuery {
                q: Some("Cambridge, USA"),
                country_code: Some("GB,US"),
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits[0].country_code.as_deref(), Some("US"));
    }
}
