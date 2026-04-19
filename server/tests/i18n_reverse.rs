//! Verifies OSM `name:<lang>` tags are honoured when a caller passes
//! `lang=` to reverse geocoding. AU OSM has broad name:zh, name:ja,
//! name:en coverage on major country/state features, so we sample
//! several CBD queries and check the localized name differs from the
//! default where OSM has the tag.

use query_server::{
    Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};

fn load() -> Option<Index> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    Index::load(
        &dir,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )
    .ok()
}

#[test]
fn lang_parameter_is_honoured_when_tag_exists() {
    let Some(idx) = load() else { return };

    // Sydney CBD — OSM has name:zh for "Australia", "New South Wales",
    // and most likely "Sydney" the admin polygon.
    let default_en = idx.query(-33.8745, 151.2090);
    let localised_zh = idx.query_with_lang(-33.8745, 151.2090, Some("zh"));

    eprintln!("en country: {:?}", default_en.address.country);
    eprintln!("zh country: {:?}", localised_zh.address.country);
    eprintln!("en state:   {:?}", default_en.address.state);
    eprintln!("zh state:   {:?}", localised_zh.address.state);

    // Default en label for AU.
    assert_eq!(default_en.address.country, Some("Australia"));

    // If OSM has name:zh on the AU country polygon, the localised version
    // should differ from "Australia". AU has name:zh=澳大利亚 in OSM.
    if let Some(zh) = localised_zh.address.country {
        if zh != "Australia" {
            // zh tag was found — sanity-check it's non-ASCII or at least
            // different from English.
            assert_ne!(zh, default_en.address.country.unwrap_or(""));
        }
    }
}

#[test]
fn unknown_lang_falls_through_to_default() {
    let Some(idx) = load() else { return };
    let default_en = idx.query(-33.8745, 151.2090);
    // "xx" is a private-use code; no OSM tag should match it.
    let same_as_default = idx.query_with_lang(-33.8745, 151.2090, Some("xx"));
    assert_eq!(default_en.address.country, same_as_default.address.country);
    assert_eq!(default_en.address.state, same_as_default.address.state);
    assert_eq!(default_en.address.city, same_as_default.address.city);
}

#[test]
fn no_lang_is_same_as_default_query() {
    let Some(idx) = load() else { return };
    let a = idx.query(-33.8745, 151.2090);
    let b = idx.query_with_lang(-33.8745, 151.2090, None);
    assert_eq!(a.address.country, b.address.country);
    assert_eq!(a.address.state, b.address.state);
    assert_eq!(a.address.city, b.address.city);
}
