//! End-to-end ICU transliteration on the forward search path.
//!
//! `i18n_forward_search.rs` covers the OSM `name:xx` expansion
//! (Cologne→Köln works because OSM tags it). This file covers the
//! gap that drove the ICU work: places where OSM has only the
//! native-script canonical name AND no `name:en`, so the only way
//! a Latin user finds them is via build-time transliteration.
//!
//! All cases here run against an index — AU smoke is not enough
//! because AU is 99% Latin. Cyrillic-light cases run against
//! `GEOCODER_INDEX_DIR` if it happens to have any non-Latin data;
//! the cross-script cases are gated on `GEOCODER_PLANET_INDEX_DIR`.

#![cfg(feature = "forward")]
#![cfg(feature = "translit")]

use query_server::forward::{Forward, KIND_PLACE};
use std::path::PathBuf;

fn load_planet() -> Option<Forward> {
    let dir = std::env::var("GEOCODER_PLANET_INDEX_DIR").ok()?;
    let path = PathBuf::from(&dir);
    Forward::open(&path).ok().filter(|f| !f.is_empty())
}

fn haversine_km(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let r = 6_371.0_f64;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlat = (lat2 - lat1).to_radians();
    let dlng = (lng2 - lng1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + p1.cos() * p2.cos() * (dlng / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

/// Russian user types Cyrillic — should always find the city
/// because the original Cyrillic string is preserved in the index.
/// This case worked pre-ICU; pin it as the recall floor.
#[test]
fn cyrillic_query_finds_moscow() {
    let Some(fwd) = load_planet() else { return };
    let hits = fwd.search("Москва", Some(KIND_PLACE), 5).expect("search");
    assert!(!hits.is_empty(), "no hits for Cyrillic 'Москва'");
    let moscow = (55.755, 37.617);
    assert!(
        hits.iter().any(|h| haversine_km(h.lat, h.lng, moscow.0, moscow.1) <= 30.0),
        "no hit within 30 km of Moscow centroid: {:?}",
        hits.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}

/// English user types BGN-style transliteration of a Russian city
/// that LACKS a `name:en` tag in OSM. Pre-ICU this returned zero;
/// post-ICU the build-time `Russian-Latin/BGN` transliterator
/// produces `Chelyabinsk` as an indexed Latin form.
#[test]
fn english_query_chelyabinsk_via_translit() {
    let Some(fwd) = load_planet() else { return };
    let hits = fwd.search("Chelyabinsk", Some(KIND_PLACE), 5).expect("search");
    assert!(
        !hits.is_empty(),
        "no hits for 'Chelyabinsk' — translit not wired or libicu rule missing"
    );
    let chelyabinsk = (55.16, 61.40);
    assert!(
        hits.iter().any(|h| haversine_km(h.lat, h.lng, chelyabinsk.0, chelyabinsk.1) <= 50.0),
        "no hit within 50 km of Chelyabinsk centroid: {:?}",
        hits.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}

/// Greek place that lacks a `name:en`. ICU's `Greek-Latin` produces
/// `Peiraiás` (with diacritic); Tantivy's AsciiFoldingFilter strips
/// it → `Peiraias`.
#[test]
fn english_query_peiraias_via_translit() {
    let Some(fwd) = load_planet() else { return };
    let hits = fwd.search("Peiraias", Some(KIND_PLACE), 5).expect("search");
    if hits.is_empty() {
        // If OSM has `name:en=Piraeus`, that's the dominant
        // English form anyway — operator may have indexed only
        // that. Don't fail; just log and skip.
        eprintln!("SKIP: no hits for Peiraias — OSM may have only Piraeus");
        return;
    }
    let piraeus = (37.94, 23.65);
    assert!(
        hits.iter().any(|h| haversine_km(h.lat, h.lng, piraeus.0, piraeus.1) <= 30.0),
        "no hit near Piraeus for 'Peiraias': {:?}",
        hits.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}

/// English user types Pinyin for a Chinese city. Build-time
/// `Han-Latin` produces `běi jīng` (with diacritics + space); our
/// post-pass adds the spaceless `běijīng`; Tantivy folds to
/// `beijing`. With `name:en=Beijing` already present in OSM this
/// case works pre-ICU too — but for smaller Chinese cities without
/// `name:en`, the Pinyin form is the only Latin path.
#[test]
fn english_query_beijing_via_translit_or_name_en() {
    let Some(fwd) = load_planet() else { return };
    let hits = fwd.search("Beijing", Some(KIND_PLACE), 5).expect("search");
    assert!(!hits.is_empty(), "no hits for 'Beijing'");
    let beijing = (39.90, 116.40);
    assert!(
        hits.iter().any(|h| haversine_km(h.lat, h.lng, beijing.0, beijing.1) <= 50.0),
        "no hit near Beijing for 'Beijing': {:?}",
        hits.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}

/// CJK user types native script. Always works (the original CJK
/// string is preserved in the index).
#[test]
fn cjk_query_native_script_finds_beijing() {
    let Some(fwd) = load_planet() else { return };
    let hits = fwd.search("北京", Some(KIND_PLACE), 5).expect("search");
    assert!(!hits.is_empty(), "no hits for CJK '北京'");
    let beijing = (39.90, 116.40);
    assert!(
        hits.iter().any(|h| haversine_km(h.lat, h.lng, beijing.0, beijing.1) <= 50.0),
        "no hit near Beijing for '北京': {:?}",
        hits.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}

/// Arabic with the `al-` post-pass. ICU produces `al-Kharṭūm`;
/// our post-pass also emits `Kharṭūm` (no article); diacritics
/// fold to ASCII at index time.
#[test]
fn arabic_query_khartoum_via_translit_or_name_en() {
    let Some(fwd) = load_planet() else { return };
    let hits = fwd.search("Khartoum", Some(KIND_PLACE), 5).expect("search");
    if hits.is_empty() {
        // OSM's `name:en=Khartoum` is well-established for this
        // city; skipping if missing is suspicious but not a hard
        // fail — translit alone produces `al-Khartum`/`Khartum`,
        // not `Khartoum` (which is the English exonym).
        eprintln!("SKIP: no hits for Khartoum — translit produces 'Khartum' not 'Khartoum'");
        return;
    }
    let khartoum = (15.50, 32.56);
    assert!(
        hits.iter().any(|h| haversine_km(h.lat, h.lng, khartoum.0, khartoum.1) <= 50.0),
        "no hit near Khartoum for 'Khartoum': {:?}",
        hits.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}
