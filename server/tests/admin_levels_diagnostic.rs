//! Diagnostic: which admin_level polygons fire for various AU coords?
//! Not a pass/fail test — prints the best-by-level admin name for each
//! fixture so we can see whether AU has meaningful level-10 coverage.

use query_server::{
    cell_id_at_level, cell_neighbors_at_level, point_in_polygon_f64, AdminPolygon, Index,
    NodeCoord, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};

fn load_index() -> Option<Index> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    assert!(
        std::path::Path::new(&dir).exists(),
        "GEOCODER_INDEX_DIR does not exist: {dir}"
    );
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

#[test]
fn major_state_polygons_cover_their_capitals() {
    let Some(index) = load_index() else { return };
    for (name, lat, lng, expected) in [
        ("Sydney", -33.8688, 151.2093, "New South Wales"),
        ("Melbourne", -37.8136, 144.9631, "Victoria"),
        ("Brisbane", -27.4698, 153.0251, "Queensland"),
        ("Toronto", 43.6532, -79.3832, "Ontario"),
        ("New York", 40.7580, -73.9855, "New York"),
        ("Miami", 25.7617, -80.1918, "Florida"),
    ] {
        assert_eq!(index.find_admin(lat, lng).state, Some(expected), "{name}");
    }
}

#[test]
fn dump_admin_levels_for_au_fixtures() {
    let Some(index) = load_index() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR unset");
        return;
    };

    // Total polygons by admin_level across the whole index.
    let all_polys: &[AdminPolygon] = unsafe {
        std::slice::from_raw_parts(
            index.admin_polygons.as_ptr() as *const AdminPolygon,
            index.admin_polygons.len() / std::mem::size_of::<AdminPolygon>(),
        )
    };
    let mut by_level = [0u32; 12];
    for p in all_polys {
        let lvl = p.admin_level as usize;
        if lvl < by_level.len() {
            by_level[lvl] += 1;
        }
    }
    eprintln!("\n=== Admin polygon counts by level (Australia index) ===");
    for (i, c) in by_level.iter().enumerate() {
        if *c > 0 {
            eprintln!("  level {i}: {c}");
        }
    }

    // Walk the admin scan for a few fixtures and print every polygon that
    // *contains* the query point, grouped by level.
    let fixtures = [
        ("sydney_cbd", -33.8745, 151.2090),
        ("bondi", -33.8915, 151.2767),
        ("melbourne", -37.8183, 144.9671),
        ("canberra", -35.3080, 149.1245),
        ("coober_pedy", -29.0135, 134.7546),
        ("baulkham_hills", -33.7369, 150.9803),
    ];

    let all_vertices: &[NodeCoord] = unsafe {
        std::slice::from_raw_parts(
            index.admin_vertices.as_ptr() as *const NodeCoord,
            index.admin_vertices.len() / std::mem::size_of::<NodeCoord>(),
        )
    };

    for (name, lat, lng) in fixtures {
        eprintln!("\n=== {name} ({lat}, {lng}) ===");
        let cell = cell_id_at_level(lat, lng, DEFAULT_ADMIN_CELL_LEVEL);
        let neighbors = cell_neighbors_at_level(cell, DEFAULT_ADMIN_CELL_LEVEL);

        // Intentionally NOT dedup'd — we want to see every candidate polygon
        // across the 9-cell window.
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let _ = &mut seen;

        for c in std::iter::once(cell).chain(neighbors) {
            // Replicate the lookup_admin_cell + for_each_entry dance.
            let cells = &index.admin_cells;
            let entries = &index.admin_entries;
            let entry_size = 12usize;
            let count = cells.len() / entry_size;
            let mut lo = 0usize;
            let mut hi = count;
            let mut offset = u32::MAX;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let mid_id = u64::from_le_bytes(
                    cells[mid * entry_size..mid * entry_size + 8]
                        .try_into()
                        .expect("8-byte cell id slice"),
                );
                if mid_id == c {
                    offset = u32::from_le_bytes(
                        cells[mid * entry_size + 8..mid * entry_size + 12]
                            .try_into()
                            .expect("4-byte cell offset slice"),
                    );
                    break;
                } else if mid_id < c {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if offset == u32::MAX {
                continue;
            }

            let o = offset as usize;
            let id_count = u16::from_le_bytes([entries[o], entries[o + 1]]) as usize;
            for i in 0..id_count {
                let raw = u32::from_le_bytes(
                    entries[o + 2 + i * 4..o + 6 + i * 4]
                        .try_into()
                        .expect("4-byte admin entry slice"),
                );
                let is_interior = (raw & 0x80000000) != 0;
                let poly_id = raw & 0x7FFFFFFF;
                // (no dedup — show every candidate)
                let poly = &all_polys[poly_id as usize];
                let verts = &all_vertices[poly.vertex_offset as usize
                    ..poly.vertex_offset as usize + poly.vertex_count as usize];
                let hits = (is_interior && c == cell) || point_in_polygon_f64(lat, lng, verts);
                if hits {
                    let name_str = {
                        let bytes = &index.strings[poly.name_id as usize..];
                        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                        std::str::from_utf8(&bytes[..end])
                            .unwrap_or("?")
                            .to_string()
                    };
                    eprintln!(
                        "  hit  level={:>2}  area={:>10.4}  interior={}  name={}",
                        poly.admin_level, poly.area, is_interior, name_str
                    );
                }
            }
        }

        // Also dump what find_admin + find_place return for this fixture.
        let admin = index.find_admin(lat, lng);
        eprintln!(
            "  find_admin result: country={:?} state={:?} county={:?} city={:?}",
            admin.country, admin.state, admin.county, admin.city
        );
        if let Some(pm) = index.find_place(lat, lng) {
            eprintln!("  find_place result: name={:?} rank={}", pm.name, pm.rank);
        } else {
            eprintln!("  find_place: None");
        }
    }
}
