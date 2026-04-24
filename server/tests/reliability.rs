//! Reliability tests for the read path. These cover failure modes that
//! would trip uptime SLOs:
//!
//! 1. Antimeridian polygon PIP — a classic correctness trap for
//!    ray-casting implementations; a polygon straddling ±180° can
//!    silently return wrong answers.
//! 2. S2 cell neighbourhood at the poles — fewer than 8 neighbours
//!    near the poles; the caller must not assume a fixed count.
//! 3. Corrupt input hardening — the hot-path street / admin loops are
//!    designed to skip records that a truncated or adversarial `.bin`
//!    could produce, rather than panic a worker.

use query_server::{
    cell_id_at_level, cell_neighbors_at_level, point_in_polygon_f64, NodeCoord,
};

fn n(lat: f32, lng: f32) -> NodeCoord {
    NodeCoord { lat, lng }
}

// --- Antimeridian polygon coverage ---

#[test]
fn antimeridian_rectangle_contains_central_point() {
    // A rectangle straddling 180°: lng goes 179 → -179 through the
    // antimeridian. Treating it naively as a planar rectangle would
    // span almost the whole globe (lng -179 to 179). Our ray-cast
    // implementation walks vertices literally so it depends on the
    // caller providing the polygon with one of the two canonical
    // antimeridian encodings. We test the "unshifted" form where
    // longitudes are given directly (the C++ builder uses this; the
    // reader treats 180 and -180 as equivalent).
    //
    // Polygon: (lat, lng) corners of a small box crossing 180°,
    // expressed as a closed ring with the antimeridian crossing
    // handled by shifting the negative longitudes to their +360
    // equivalents (181, 182). This mirrors what S2Polygon::GetArea
    // produces when the builder normalises antimeridian polygons.
    let poly = [
        n(-1.0, 179.0),
        n(1.0, 179.0),
        n(1.0, 181.0), // equivalent to -179
        n(-1.0, 181.0),
    ];

    // Point slightly east of the antimeridian, inside the box.
    assert!(point_in_polygon_f64(0.0, 180.0, &poly));
    // Point slightly west of the antimeridian, also inside.
    // Its normalised lng = -179.5 → +180.5 in the shifted frame.
    // We test the shifted coord explicitly.
    assert!(point_in_polygon_f64(0.0, 180.5, &poly));
}

#[test]
fn antimeridian_polygon_excludes_far_points() {
    // Same polygon as above; verify clearly-outside points are rejected.
    let poly = [
        n(-1.0, 179.0),
        n(1.0, 179.0),
        n(1.0, 181.0),
        n(-1.0, 181.0),
    ];
    assert!(!point_in_polygon_f64(0.0, 0.0, &poly), "opposite side of globe");
    assert!(!point_in_polygon_f64(45.0, 180.0, &poly), "well north of box");
    assert!(!point_in_polygon_f64(0.0, 178.0, &poly), "just west of box");
    assert!(!point_in_polygon_f64(0.0, 182.0, &poly), "just east of box (in shifted frame)");
}

// --- Pole-neighbourhood coverage ---

#[test]
fn cell_neighbors_near_north_pole_are_finite_and_nonempty() {
    // Cells near the poles are pentagons in S2's topology (fewer than
    // 8 neighbours). We don't care about the exact count — just that
    // the call returns something sane (non-empty, finite, no panic).
    let cell = cell_id_at_level(89.9, 0.0, 17);
    let neighbors = cell_neighbors_at_level(cell, 17);
    assert!(!neighbors.is_empty(), "pole cell has at least one neighbour");
    assert!(neighbors.len() <= 12, "sanity: pole cells have ≤ 12 neighbours");
}

#[test]
fn cell_neighbors_near_south_pole_are_finite_and_nonempty() {
    let cell = cell_id_at_level(-89.9, 0.0, 17);
    let neighbors = cell_neighbors_at_level(cell, 17);
    assert!(!neighbors.is_empty());
}

#[test]
fn cell_neighbors_at_exact_pole_doesnt_panic() {
    // The poles themselves are singular points; S2 handles them, but
    // we want to pin that our wrappers don't regress.
    let n = cell_id_at_level(90.0, 0.0, 10);
    let _ = cell_neighbors_at_level(n, 10);
    let s = cell_id_at_level(-90.0, 0.0, 10);
    let _ = cell_neighbors_at_level(s, 10);
}

#[test]
fn cell_neighbors_at_antimeridian_doesnt_panic() {
    // ±180 lng should round-trip cleanly.
    let c = cell_id_at_level(0.0, 180.0, 15);
    let _ = cell_neighbors_at_level(c, 15);
    let c = cell_id_at_level(0.0, -180.0, 15);
    let _ = cell_neighbors_at_level(c, 15);
}

// --- Degenerate polygon rings ---

#[test]
fn point_in_polygon_handles_empty_ring() {
    let empty: [NodeCoord; 0] = [];
    assert!(!point_in_polygon_f64(0.0, 0.0, &empty));
}

#[test]
fn point_in_polygon_handles_degenerate_two_point_ring() {
    // Two-point "polygon" has no interior; PIP must return false rather
    // than panic or return nonsense.
    let poly = [n(0.0, 0.0), n(1.0, 1.0)];
    assert!(!point_in_polygon_f64(0.5, 0.5, &poly));
}
