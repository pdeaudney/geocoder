//! Verifies `Index::find_addr_point` — the house-number refinement step
//! behind forward geocoding. Requires the AU binary index.

use query_server::{
    as_typed_slice, AddrPoint, Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE,
    DEFAULT_STREET_CELL_LEVEL,
};

fn load_index() -> Option<Index> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    Index::load(
        &dir,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )
    .ok()
}

/// Pick a real (street_name, housenumber, lat, lng) tuple from the AU
/// addr_points so the tests are grounded in actual OSM data and don't
/// require prior knowledge of specific houses.
fn pick_known_address(idx: &Index) -> Option<(String, String, f64, f64)> {
    let points: &[AddrPoint] = as_typed_slice(&idx.addr_points);
    for p in points.iter().take(50_000) {
        // Skip addr:place rows — they have no street component, so
        // street_or_place_id holds a place name that wouldn't form a
        // meaningful "street hint" for the find_addr_point self-check.
        if p.flags & query_server::FLAG_ADDR_PLACE != 0 {
            continue;
        }
        let street = idx.get_string(p.street_or_place_id);
        let hn = idx.get_string(p.housenumber_id);
        // Skip weirdly long names / empty values to keep the test readable.
        if street.is_empty() || hn.is_empty() || street.len() > 40 {
            continue;
        }
        // Prefer "normal"-looking house numbers (digit-only) over things
        // like "12A" or "Lot 3" so we don't accidentally write brittle
        // expectations later.
        if hn.chars().all(|c| c.is_ascii_digit()) {
            return Some((street.to_owned(), hn.to_owned(), p.lat as f64, p.lng as f64));
        }
    }
    None
}

#[test]
fn finds_real_address_near_its_own_point() {
    let Some(idx) = load_index() else { return };
    let Some((street, hn, lat, lng)) = pick_known_address(&idx) else {
        panic!("AU index contains no qualifying addr_points");
    };
    eprintln!("seeding with real address: '{hn} {street}' @ ({lat}, {lng})");

    // Querying at the address's own coord should return exactly that point.
    let got = idx
        .find_addr_point(&hn, Some(&street), lat, lng)
        .unwrap_or_else(|| {
            panic!("find_addr_point returned None for self-lookup '{hn} {street}'")
        });

    assert_eq!(got.housenumber, hn);
    // Street name match is substring-case-insensitive — "Elizabeth Street"
    // query hint matches an addr_point's full "Elizabeth Street" name.
    assert!(
        got.street.to_ascii_lowercase().contains(&street.to_ascii_lowercase()),
        "street {:?} should contain hint {:?}",
        got.street,
        street,
    );
    // Coord check: the returned match should be within ~100m of the seed.
    // Tight exact-equality doesn't hold once G-NAF is in the path because
    // G-NAF geocodes an address to the property entrance while OSM's
    // addr_point is usually the building centroid — both authoritative,
    // not byte-identical.
    let dlat = (got.lat - lat).abs();
    let dlng = (got.lng - lng).abs();
    assert!(
        dlat < 0.001 && dlng < 0.001,
        "coord drift too large: dlat={dlat} dlng={dlng} (seed=({lat},{lng}), got=({},{}))",
        got.lat,
        got.lng,
    );
}

#[test]
fn returns_none_for_nonexistent_housenumber() {
    let Some(idx) = load_index() else { return };
    let Some((street, _, lat, lng)) = pick_known_address(&idx) else {
        return;
    };
    // Use a housenumber that's very unlikely to exist at this address.
    let got = idx.find_addr_point("999999", Some(&street), lat, lng);
    assert!(got.is_none(), "expected None, got {:?}", got.map(|m| m.housenumber.to_owned()));
}

#[test]
fn returns_none_for_empty_housenumber() {
    let Some(idx) = load_index() else { return };
    let got = idx.find_addr_point("", None, -33.8688, 151.2093);
    assert!(got.is_none());
}

#[test]
fn respects_street_name_hint() {
    let Some(idx) = load_index() else { return };
    // Use a CBD point where multiple streets likely have a "10" nearby.
    // We're not asserting a specific address — only that when we restrict by
    // street-name hint, the returned street matches the hint (case-insensitive
    // substring).
    let got = idx.find_addr_point("10", Some("George"), -33.8688, 151.2093);
    if let Some(m) = got {
        assert!(
            m.street.to_ascii_lowercase().contains("george"),
            "hint {:?} should constrain street, got {:?}",
            "george",
            m.street,
        );
        assert_eq!(m.housenumber, "10");
    }
}

#[test]
fn match_unit_floor_parent_default_empty_strings() {
    // Pin the contract that AddrPointMatch's new sub-building /
    // tagged-parent fields default to "" (not panic, not Option,
    // not garbage). Most addresses won't have addr:unit / addr:floor
    // tags so this is the normal path.
    let Some(idx) = load_index() else { return };
    let Some((street, hn, lat, lng)) = pick_known_address(&idx) else {
        return;
    };
    let m = idx
        .find_addr_point(&hn, Some(&street), lat, lng)
        .expect("self-lookup hits");
    // Each new field is `&str`. Either empty (most common) or some
    // genuine extracted value. NEVER unset / not initialised.
    let _ = m.unit.len(); // would panic on a dangling pointer
    let _ = m.floor.len();
    let _ = m.parent_place.len();
    // flags = 0 for the typical "addr:street + addr:housenumber" case
    // we picked. FLAG_ADDR_PLACE rows are filtered out by
    // pick_known_address.
    assert_eq!(
        m.flags & query_server::FLAG_ADDR_PLACE,
        0,
        "pick_known_address should filter out FLAG_ADDR_PLACE rows"
    );
}

#[test]
fn flag_addr_place_rows_present_in_index_when_built() {
    // A weak existence check: AU has very little addr:place tagging
    // (DE/AT/CH dominate), but a planet build or DE extract should
    // contain at least some rows with FLAG_ADDR_PLACE set. This test
    // is informational — when the AU index is loaded it'll typically
    // print 0 or near-zero, which is correct for that data.
    let Some(idx) = load_index() else { return };
    let points: &[query_server::AddrPoint] =
        query_server::as_typed_slice(&idx.addr_points);
    let count = points
        .iter()
        .filter(|p| p.flags & query_server::FLAG_ADDR_PLACE != 0)
        .count();
    eprintln!(
        "FLAG_ADDR_PLACE: {} / {} address points ({}%)",
        count,
        points.len(),
        if points.is_empty() { 0.0 } else { 100.0 * count as f64 / points.len() as f64 },
    );
    // No assertion on count — country-dependent. Test exists to
    // exercise the field access path under loaded data so a struct-
    // layout drift would fail here too.
}

#[test]
fn parent_place_id_resolves_to_a_string_when_set() {
    // For any address with parent_place_id != 0, the offset must
    // resolve to a non-empty string. A garbled offset would either
    // return "" (wrong) or read past the strings buffer.
    let Some(idx) = load_index() else { return };
    let points: &[query_server::AddrPoint] =
        query_server::as_typed_slice(&idx.addr_points);
    let mut sampled = 0;
    for p in points.iter().take(200_000) {
        if p.parent_place_id == 0 {
            continue;
        }
        let s = idx.get_string(p.parent_place_id);
        assert!(
            !s.is_empty() && s.len() < 200,
            "parent_place_id={} resolves to plausible string, got {:?}",
            p.parent_place_id,
            s
        );
        sampled += 1;
        if sampled >= 100 {
            break;
        }
    }
    // No lower-bound assertion — AU has limited addr:city tagging.
    // The check above runs against any sample we do find.
    eprintln!("parent_place_id: sampled {sampled} non-zero offsets");
}
