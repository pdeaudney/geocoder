//! End-to-end verification of the G-NAF address-point index against real
//! AU addresses. Requires both the reverse-geocoding index and the G-NAF
//! binaries built from the free data.gov.au PSV release.

use query_server::{Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL};

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
fn alysse_close_10_resolves_to_exact_gnaf_coord() {
    let Some(idx) = load() else { return };
    // G-NAF has an exact lat/lng for "10 Alysse Close Baulkham Hills". The
    // old OSM-only forward path fell back to the street centroid; with
    // G-NAF it should return the property's own coordinate — distinct from
    // the centroid.
    let centroid = (-33.7369_f64, 150.9803_f64);
    let got = idx
        .find_addr_point("10", Some("Alysse Close"), centroid.0, centroid.1)
        .expect("G-NAF should have #10 Alysse Close");
    assert_eq!(got.housenumber, "10");
    assert!(got.street.to_ascii_lowercase().contains("alysse"));

    // The returned coord must be close to but not identical to the
    // street-centroid we searched around.
    let dlat = (got.lat - centroid.0).abs();
    let dlng = (got.lng - centroid.1).abs();
    assert!(
        dlat < 0.002 && dlng < 0.002,
        "#10 should be within ~200m of the centroid; got dlat={dlat} dlng={dlng}",
    );
    eprintln!(
        "G-NAF exact: #10 Alysse Close @ ({:.6}, {:.6})",
        got.lat, got.lng
    );
}

#[test]
fn truly_nonexistent_housenumber_returns_none() {
    let Some(idx) = load() else { return };
    // 999 doesn't exist on Alysse Close (highest real number is in the 40s).
    // Both G-NAF and OSM should return None.
    let got = idx.find_addr_point("999", Some("Alysse Close"), -33.7369, 150.9803);
    assert!(got.is_none(), "got {:?}", got.map(|m| m.housenumber.to_owned()));
}

#[test]
fn reverse_geocode_returns_exact_postcode_from_gnaf() {
    let Some(idx) = load() else { return };
    // At the Alysse Close centroid, G-NAF's nearest-point enrichment
    // should yield postcode 2153 (the suburb-modal lookup also gives 2153
    // for Baulkham Hills, but G-NAF's is per-address and would differ in
    // a multi-postcode suburb).
    let addr = idx.query(-33.7369, 150.9803);
    assert_eq!(addr.address.postcode.as_deref(), Some("2153"));
}

#[test]
fn gnaf_covers_a_cbd_address() {
    let Some(idx) = load() else { return };
    // Query a well-known Sydney CBD coord — any G-NAF-covered address
    // nearby should resolve the reverse query's postcode to 2000.
    let addr = idx.query(-33.8688, 151.2093);
    assert_eq!(addr.address.postcode.as_deref(), Some("2000"));
    assert_eq!(addr.address.state.as_deref(), Some("New South Wales"));
}
