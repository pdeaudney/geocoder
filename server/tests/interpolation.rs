//! Validates review item #1: `project_point_on_polyline` uses squared distances
//! as if they were linear segment lengths, so the returned `t` is not
//! proportional to true arc length on polylines with uneven segments.
//!
//! ## Setup
//!
//! Construct a 2-segment polyline at the equator (so `cos_lat == 1`) with very
//! uneven segment lengths:
//!
//!   A ────(long)──── B ──(short)── C
//!
//! Query a point at B (the polyline node between the two segments). A correct
//! linear-arc-length parametrization would return `t = long / (long + short)`.
//! The buggy squared-length version returns `t = long^2 / (long^2 + short^2)`,
//! which is biased toward the longer segment.
//!
//! We pick a 10:1 ratio so the two expected values differ substantially:
//!   - correct t            = 10 / 11        ≈ 0.909
//!   - buggy squared-length = 100 / 101      ≈ 0.990
//!
//! The test asserts the *correct* value. It will fail until the bug is fixed,
//! and that failure is the proof the bug exists.

use query_server::{project_point_on_polyline, NodeCoord};

/// Equator-local geometry so lat/lng degrees map cleanly to arc length.
fn node(lat: f32, lng: f32) -> NodeCoord {
    NodeCoord { lat, lng }
}

#[test]
fn project_point_t_is_linear_arc_length_not_squared() {
    // Equator: cos_lat = 1, so dlat^2 + dlng^2 is a clean planar distance squared.
    // Segment 1: from (0,0) to (0, 1.0)   — length 1.0 deg
    // Segment 2: from (0,1) to (0, 1.1)   — length 0.1 deg
    // Total length = 1.1 deg, ratio 10:1.
    let nodes = [node(0.0, 0.0), node(0.0, 1.0), node(0.0, 1.1)];

    // Query point: exactly on the middle node.
    let (lat, lng) = (0.0_f64, 1.0_f64);
    let cos_lat = lat.to_radians().cos();

    let (dist_sq, t) = project_point_on_polyline(lat, lng, &nodes, cos_lat)
        .expect("polyline has 2 segments");

    // Query sits on a node; distance should be ~0.
    assert!(dist_sq < 1e-18, "expected near-zero dist, got {dist_sq}");

    // Correct linear parametrisation: t == long / (long + short) == 10/11.
    let expected = 10.0_f64 / 11.0_f64;
    let buggy_squared = 100.0_f64 / 101.0_f64;

    // Demonstrate that buggy and correct are *meaningfully* different (~8%).
    assert!((expected - buggy_squared).abs() > 0.05);

    // Fails today (returns ~0.990); will pass once squared-length bug is fixed.
    assert!(
        (t - expected).abs() < 1e-3,
        "t = {t}, expected ≈ {expected} (linear arc-length); \
         got the squared-length value ≈ {buggy_squared}, \
         which confirms review item #1",
    );
}

#[test]
fn project_point_t_is_monotonic_along_polyline() {
    // Property test: sliding the query point across a 3-segment polyline should
    // produce a strictly increasing t in [0, 1]. With the squared-length bug
    // the t values still increase but the spacing is wrong. We assert only the
    // monotonicity property and the endpoint values — both should hold before
    // and after the fix, so this test acts as a regression guardrail for the
    // fix rather than a bug-probe.
    let nodes = [
        node(0.0, 0.0),
        node(0.0, 0.5),
        node(0.0, 2.0),
        node(0.0, 2.1),
    ];
    let cos_lat = 1.0;

    let mut prev_t = -1.0;
    for step in 0..=20 {
        let lng = 2.1 * (step as f64) / 20.0;
        let (_, t) = project_point_on_polyline(0.0, lng, &nodes, cos_lat)
            .expect("polyline has segments");
        assert!(
            t >= prev_t - 1e-9,
            "t is not monotonic: step {step} lng={lng}, prev_t={prev_t} t={t}",
        );
        assert!((0.0..=1.0).contains(&t), "t out of range: {t}");
        prev_t = t;
    }
}

#[test]
fn project_point_degenerate_polylines() {
    let cos_lat = 1.0;
    // Too few nodes.
    assert!(project_point_on_polyline(0.0, 0.0, &[], cos_lat).is_none());
    assert!(project_point_on_polyline(0.0, 0.0, &[node(0.0, 0.0)], cos_lat).is_none());

    // Zero-length polyline.
    let zero = [node(0.0, 0.0), node(0.0, 0.0), node(0.0, 0.0)];
    assert!(project_point_on_polyline(0.0, 0.0, &zero, cos_lat).is_none());
}
