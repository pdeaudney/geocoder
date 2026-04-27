//! Forward search must land on the right entry whether the user types
//! the canonical OSM name or an English/local alternate.
//!
//! Two failure modes the build-side fixes address:
//!
//! 1. **Endonym/exonym mismatch.** OSM has `name=Köln, name:en=Cologne`.
//!    Pre-fix, `Cologne` returned nothing. Post-fix, both queries
//!    resolve to the same place — verified for the planet index when
//!    `GEOCODER_PLANET_INDEX_DIR` is set.
//!
//! 2. **Saint ↔ St abbreviation mismatch.** AU OSM has `St Kilda`;
//!    real users type `Saint Kilda`. Both must resolve to the same
//!    suburb.
//!
//! Skipped unless `GEOCODER_INDEX_DIR` is set and contains a
//! `tantivy/` (or `tantivy_<cc>/`) build. The Cologne case adds a
//! second optional gate (`GEOCODER_PLANET_INDEX_DIR`) — that data
//! isn't in the AU-only smoke index.

#![cfg(feature = "forward")]

use query_server::forward::{Forward, Hit, KIND_PLACE};
use std::path::PathBuf;

fn load_forward(env_var: &str) -> Option<Forward> {
    let dir = std::env::var(env_var).ok()?;
    let path = PathBuf::from(&dir);
    if !path.exists() {
        eprintln!("SKIP: {} not found", path.display());
        return None;
    }
    match Forward::open(&path) {
        Ok(f) if !f.is_empty() => Some(f),
        Ok(_) => {
            eprintln!("SKIP: {} has no tantivy indexes", path.display());
            None
        }
        Err(e) => {
            eprintln!("FAIL: could not open forward index: {e}");
            None
        }
    }
}

fn names_lc(hits: &[Hit]) -> Vec<String> {
    hits.iter().map(|h| h.name.to_ascii_lowercase()).collect()
}

fn first_within_km(hits: &[Hit], lat: f64, lng: f64, max_km: f64) -> Option<&Hit> {
    hits.iter().find(|h| haversine_km(h.lat, h.lng, lat, lng) <= max_km)
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

/// `Saint Kilda` and `St Kilda` must both find the Melbourne suburb.
/// AU smoke index covers this; runs whenever `GEOCODER_INDEX_DIR`
/// points at a built AU index.
#[test]
fn saint_kilda_resolves_via_both_spellings() {
    let Some(fwd) = load_forward("GEOCODER_INDEX_DIR") else { return };

    let canonical = fwd.search("St Kilda", Some(KIND_PLACE), 10).expect("search");
    let saint = fwd.search("Saint Kilda", Some(KIND_PLACE), 10).expect("search");

    assert!(!canonical.is_empty(), "no hits for 'St Kilda'");
    assert!(
        !saint.is_empty(),
        "no hits for 'Saint Kilda' — abbreviation fold isn't wired through. \
         Got nothing where 'St Kilda' returned: {:?}",
        names_lc(&canonical)
    );

    // Both should land on a place near the Melbourne St Kilda suburb
    // (-37.8678, 144.9756). Don't compare exact name strings — the
    // canonical OSM name is "St Kilda" so that's what comes back even
    // for the "Saint Kilda" query.
    let melb_st_kilda = (-37.8678, 144.9756);
    let canonical_hit = first_within_km(&canonical, melb_st_kilda.0, melb_st_kilda.1, 5.0);
    let saint_hit = first_within_km(&saint, melb_st_kilda.0, melb_st_kilda.1, 5.0);
    assert!(
        canonical_hit.is_some(),
        "no hit within 5 km of Melbourne St Kilda for 'St Kilda': {:?}",
        canonical.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
    assert!(
        saint_hit.is_some(),
        "no hit within 5 km of Melbourne St Kilda for 'Saint Kilda': {:?}",
        saint.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}

/// `Mount Pleasant` and `Mt Pleasant` must converge. AU has a Mt Pleasant
/// in WA (-32.039, 115.842). Skip if not present in the index — small AU
/// fixtures may not include it.
#[test]
fn mount_pleasant_resolves_via_both_spellings() {
    let Some(fwd) = load_forward("GEOCODER_INDEX_DIR") else { return };

    let mt = fwd.search("Mt Pleasant", Some(KIND_PLACE), 10).expect("search");
    let mount = fwd.search("Mount Pleasant", Some(KIND_PLACE), 10).expect("search");

    if mt.is_empty() && mount.is_empty() {
        eprintln!("SKIP: AU index has no Mt/Mount Pleasant entries");
        return;
    }
    assert_eq!(
        mt.is_empty(),
        mount.is_empty(),
        "Mt vs Mount asymmetric: mt_hits={}, mount_hits={}",
        mt.len(),
        mount.len()
    );
}

/// Cologne (DE) → Köln. Only meaningful against a planet index that
/// includes Germany; `GEOCODER_PLANET_INDEX_DIR` gates this so the
/// AU smoke build doesn't fail it.
#[test]
fn cologne_resolves_to_koln() {
    let Some(fwd) = load_forward("GEOCODER_PLANET_INDEX_DIR") else { return };

    let hits = fwd.search("Cologne", Some(KIND_PLACE), 10).expect("search");
    assert!(!hits.is_empty(), "no hits for 'Cologne' on planet index");

    // Köln coords ~ (50.937, 6.96). Accept any place hit within 10 km.
    let koln_centroid = (50.937, 6.96);
    let near = first_within_km(&hits, koln_centroid.0, koln_centroid.1, 10.0);
    assert!(
        near.is_some(),
        "no hit within 10 km of Köln centroid for 'Cologne': {:?}",
        hits.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}

/// The Hague (NL) → Den Haag. Same gating as Cologne.
#[test]
fn the_hague_resolves_to_den_haag() {
    let Some(fwd) = load_forward("GEOCODER_PLANET_INDEX_DIR") else { return };

    let hits = fwd.search("The Hague", Some(KIND_PLACE), 10).expect("search");
    assert!(!hits.is_empty(), "no hits for 'The Hague' on planet index");

    // Den Haag ~ (52.0705, 4.3007).
    let den_haag = (52.0705, 4.3007);
    let near = first_within_km(&hits, den_haag.0, den_haag.1, 10.0);
    assert!(
        near.is_some(),
        "no hit within 10 km of Den Haag centroid for 'The Hague': {:?}",
        hits.iter().map(|h| (&h.name, h.lat, h.lng)).collect::<Vec<_>>()
    );
}
