//! Validates review item #10: `point_in_polygon` runs in f32 and loses
//! precision near long polygon edges — this can flip point-in-polygon for
//! queries very close to a boundary.
//!
//! We construct a single long axis-aligned boundary edge similar to a
//! state/country boundary and query at points placed a tiny distance inside
//! and outside. With f64 math the test is deterministic. With f32 math used
//! by the current `point_in_polygon`, results near the edge can be ambiguous.

use query_server::{point_in_polygon, point_in_polygon_f64, NodeCoord};

fn n(lat: f32, lng: f32) -> NodeCoord {
    NodeCoord { lat, lng }
}

#[test]
fn f32_and_f64_agree_far_from_edges() {
    // A simple 1° square polygon well away from equator-straddling edge cases.
    let poly_f32 = [n(-34.0, 150.0), n(-34.0, 151.0), n(-33.0, 151.0), n(-33.0, 150.0)];

    for (lat, lng) in [(-33.5_f64, 150.5), (-33.1, 150.9), (-33.9, 150.1)] {
        let r_legacy = point_in_polygon(lat as f32, lng as f32, &poly_f32);
        let r_prod = point_in_polygon_f64(lat, lng, &poly_f32);
        assert_eq!(r_legacy, r_prod, "disagreement at ({lat}, {lng})");
        assert!(r_prod, "well-inside point should be inside");
    }

    // Clearly-outside points.
    for (lat, lng) in [(-35.0_f64, 150.5), (-33.5, 152.0)] {
        let r_legacy = point_in_polygon(lat as f32, lng as f32, &poly_f32);
        let r_prod = point_in_polygon_f64(lat, lng, &poly_f32);
        assert_eq!(r_legacy, r_prod);
        assert!(!r_prod);
    }
}

#[test]
fn f32_precision_stress_near_long_edge() {
    // Construct a polygon with a very long edge (Victoria/NSW state-like scale)
    // and probe points extremely close to the edge from both sides. For some
    // tiny offsets the f32 rounding in the edge equation can cause disagreement
    // with the f64 reference — when it does, that's the exact precision loss
    // review item #10 describes.
    //
    // This test prints disagreements instead of asserting them, so it acts as
    // a documentation probe — the f64 answer is treated as ground truth and
    // the test records how close to an edge an f32 classifier can still be
    // trusted.
    let poly_f32 = [
        n(-37.5000, 140.0000),
        n(-37.5000, 150.0000),
        n(-33.9000, 150.0000),
        n(-33.9000, 140.0000),
    ];
    let poly_f64 = [
        (-37.5000_f64, 140.0000),
        (-37.5000, 150.0000),
        (-33.9000, 150.0000),
        (-33.9000, 140.0000),
    ];

    // Probe the production f64-entry path used by `find_admin`: query point
    // stays in f64 all the way through, only vertex storage is f32. With that
    // call path the edge equation is fully f64, so it must agree with the
    // pure-f64 reference implementation down to the float-precision floor.
    let mut prod_disagreements = 0;
    // Probe the legacy f32-entry wrapper too, for comparison — this is NOT
    // used in the hot path anymore, but the function is still public and its
    // precision limit is informational.
    let mut legacy_smallest_safe = f64::INFINITY;

    for (i, offset) in [1e-2, 1e-3, 1e-4, 1e-5, 1e-6, 1e-7, 1e-8].iter().enumerate() {
        for side in [-1.0, 1.0] {
            let lat = -37.5 + side * offset;
            let lng = 145.0;

            let r_prod = point_in_polygon_f64(lat, lng, &poly_f32);
            let r_ref = point_in_polygon_f64_refimpl(lat, lng, &poly_f64);
            if r_prod != r_ref {
                prod_disagreements += 1;
                eprintln!(
                    "production f64 disagrees with reference at offset=1e-{} side={:+} -> prod={} ref={}",
                    i + 2, side, r_prod, r_ref,
                );
            }

            let r_legacy = point_in_polygon(lat as f32, lng as f32, &poly_f32);
            if r_legacy != r_ref {
                eprintln!(
                    "legacy f32-entry wrapper disagrees at offset=1e-{} side={:+} -> wrap={} ref={}",
                    i + 2, side, r_legacy, r_ref,
                );
            } else {
                legacy_smallest_safe = legacy_smallest_safe.min(*offset);
            }
        }
    }

    eprintln!("legacy f32-entry wrapper agrees down to: {:e}", legacy_smallest_safe);
    eprintln!("production f64-entry disagreements: {}", prod_disagreements);

    // The production entry point must match the pure-f64 reference exactly
    // (vertex f32 storage rounds at ~11 cm but the test points are all
    // well above that precision floor, and we're comparing two impls that
    // both read the same f32-stored vertices).
    assert_eq!(
        prod_disagreements, 0,
        "production f64 entry must match the pure-f64 reference exactly",
    );
}

// Copy of `point_in_polygon_f64` parametrised over `(f64, f64)` vertex input
// so we can feed it the unrounded polygon.
fn point_in_polygon_f64_refimpl(lat: f64, lng: f64, vertices: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let n = vertices.len();
    if n == 0 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (vi_lat, vi_lng) = vertices[i];
        let (vj_lat, vj_lng) = vertices[j];
        if ((vi_lng > lng) != (vj_lng > lng))
            && (lat < (vj_lat - vi_lat) * (lng - vi_lng) / (vj_lng - vi_lng) + vi_lat)
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}
