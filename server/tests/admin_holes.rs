//! Admin polygon holes against a real index.
//!
//! build-index bridges inner rings into the stored admin ring, so a point
//! inside a hole must not resolve to the polygon that owns the hole.
//! Requires `GEOCODER_INDEX_DIR`; skipped when it is unset. A fixture whose
//! country is not in the index is skipped, but a fixture whose owner polygon
//! no longer resolves at its in-area point fails, so OSM edits that move a
//! hole cannot turn this test into a silent pass.

use query_server::{
    as_typed_slice, AdminPolygon, AdminResult, Index, DEFAULT_ADMIN_CELL_LEVEL,
    DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};

struct HoleFixture {
    owner: &'static str,
    level: u8,
    country_code: &'static [u8; 2],
    /// A point inside the owner, away from its holes.
    in_area: (f64, f64),
    /// A point inside one of the owner's holes.
    in_hole: (f64, f64),
}

const FIXTURES: &[HoleFixture] = &[
    // Yulara (town) is cut out of the Macdonnell Region LGA.
    HoleFixture {
        owner: "Macdonnell Region",
        level: 6,
        country_code: b"AU",
        in_area: (-23.94, 132.78),
        in_hole: (-25.206908, 130.970057),
    },
    // Nhulunbuy is cut out of the East Arnhem Region LGA.
    HoleFixture {
        owner: "East Arnhem Region",
        level: 6,
        country_code: b"AU",
        in_area: (-12.25, 136.89),
        in_hole: (-12.178638, 136.778306),
    },
    // One of Tulsa's many unincorporated holes, on the Osage County side.
    HoleFixture {
        owner: "Tulsa",
        level: 8,
        country_code: b"US",
        in_area: (36.1540, -95.9928),
        in_hole: (36.219525, -96.034293),
    },
];

fn load_index() -> Option<Index> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    Some(
        Index::load(
            &dir,
            DEFAULT_STREET_CELL_LEVEL,
            DEFAULT_ADMIN_CELL_LEVEL,
            DEFAULT_SEARCH_DISTANCE,
        )
        .expect("load reverse index"),
    )
}

/// (name, admin_level) of every polygon the lookup picked.
fn picked<'a>(idx: &'a Index, admin: &AdminResult<'_>) -> Vec<(&'a str, u8)> {
    let polys: &[AdminPolygon] = as_typed_slice(&idx.admin_polygons);
    [
        admin.country_poly_id,
        admin.state_poly_id,
        admin.county_poly_id,
        admin.city_poly_id,
    ]
    .into_iter()
    .flatten()
    .map(|id| {
        let poly = &polys[id as usize];
        (idx.get_string(poly.name_id), poly.admin_level)
    })
    .collect()
}

#[test]
fn point_in_admin_hole_does_not_resolve_to_hole_owner() {
    let Some(idx) = load_index() else { return };
    let mut checked = 0;
    for f in FIXTURES {
        let owner = (f.owner, f.level);
        let in_area = idx.find_admin(f.in_area.0, f.in_area.1);
        if in_area.country_code != Some(*f.country_code) {
            eprintln!("skip {}: country not in this index", f.owner);
            continue;
        }
        assert!(
            picked(&idx, &in_area).contains(&owner),
            "precondition: {owner:?} must resolve at {:?}, got {:?}",
            f.in_area,
            picked(&idx, &in_area)
        );
        let in_hole = idx.find_admin(f.in_hole.0, f.in_hole.1);
        assert!(
            !picked(&idx, &in_hole).contains(&owner),
            "{:?} is in a hole of {owner:?} but resolved to it: {:?}",
            f.in_hole,
            picked(&idx, &in_hole)
        );
        checked += 1;
    }
    eprintln!("checked {checked} hole fixtures");
}
