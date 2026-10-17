//! Geographic helpers shared across the server, builders, and tools.
//!
//! `haversine_m` is used by the `regression-runner` (offline accuracy
//! checks against fixed corpora) and by the shadow-validation worker
//! (online accuracy checks against Google Geocoding). The `dist_sq`
//! conversions keep the reverse-geocode distance caps in the same unit
//! as `crate::dist_sq`, which works in radians².

/// IUGG mean Earth radius, the standard choice for short-distance work.
pub const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Convert a distance in metres to the radians² unit `crate::dist_sq`
/// returns, so a cap can be compared against it directly.
pub fn metres_to_dist_sq(metres: f64) -> f64 {
    let rad = metres / EARTH_RADIUS_M;
    rad * rad
}

/// Inverse of [`metres_to_dist_sq`].
pub fn dist_sq_to_metres(dist_sq: f64) -> f64 {
    dist_sq.sqrt() * EARTH_RADIUS_M
}

/// Spherical-earth great-circle distance in metres.
///
/// Accurate to ~0.3 % at any distance — well within the geocoder's own
/// tolerance (10s–1000s of metres). The error grows toward the equator
/// where the WGS-84 spheroid bulges most relative to a sphere; for the
/// "did our top hit land within N metres of the reference?" question
/// this approximation is more than enough.
pub fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lng2 - lng1).to_radians();
    let a = (dphi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_distance_when_points_identical() {
        let d = haversine_m(-33.8568, 151.2153, -33.8568, 151.2153);
        assert!(d < 0.001, "expected ~0 m, got {d}");
    }

    #[test]
    fn equator_one_degree_lng_is_about_111km() {
        // At the equator, 1° longitude ≈ 111.32 km. Haversine on a
        // 6371 km sphere gives 111.195 km; well within the documented
        // ~0.3 % tolerance.
        let d = haversine_m(0.0, 0.0, 0.0, 1.0);
        assert!(
            (110_000.0..112_000.0).contains(&d),
            "expected ~111 km at equator, got {d:.0} m"
        );
    }

    #[test]
    fn sydney_to_melbourne_is_about_713km() {
        // Sydney CBD (-33.8568, 151.2153) to Melbourne CBD
        // (-37.8136, 144.9631). Reference: 713 km great-circle.
        let d = haversine_m(-33.8568, 151.2153, -37.8136, 144.9631);
        let km = d / 1000.0;
        assert!(
            (710.0..717.0).contains(&km),
            "expected ~713 km Sydney→Melbourne, got {km:.1} km"
        );
    }

    #[test]
    fn dist_sq_conversions_match_haversine() {
        // Bondi Beach, then a point about 75 m east. `dist_sq` takes radian
        // deltas, exactly as `Index::query_geo` and `find_poi` call it.
        let (lat, lng): (f64, f64) = (-33.8915, 151.2767);
        let lng2 = lng + 0.000_812;
        let cos_lat = lat.to_radians().cos();
        let d = crate::dist_sq(0.0, (lng2 - lng).to_radians(), cos_lat);
        let expected_m = haversine_m(lat, lng, lat, lng2);
        let got_m = dist_sq_to_metres(d);
        assert!(
            (got_m - expected_m).abs() < 0.1,
            "dist_sq_to_metres gave {got_m:.2} m, haversine {expected_m:.2} m"
        );
        assert!(d < metres_to_dist_sq(expected_m + 1.0));
        assert!(d > metres_to_dist_sq(expected_m - 1.0));
    }

    #[test]
    fn segment_distance_is_metric_at_high_latitude() {
        // A diagonal street centred on (60, 10) and a point 70 m from its
        // midpoint along the metric perpendicular. Projecting in raw degrees
        // reports ~87 m here, which the 75 m reverse cap would reject.
        let (lat0, lng0): (f64, f64) = (60.0, 10.0);
        let c = lat0.to_radians().cos();
        let off = 70.0 / EARTH_RADIUS_M;
        let norm = (1.0 + c * c).sqrt();
        let lat = lat0 + (-c / norm * off).to_degrees();
        let lng = lng0 + (1.0 / norm * off / c).to_degrees();
        let (d, t) = crate::point_to_segment_with_t(
            lat,
            lng,
            lat0 - 0.01,
            lng0 - 0.01,
            lat0 + 0.01,
            lng0 + 0.01,
            lat.to_radians().cos(),
        );
        let got_m = dist_sq_to_metres(d);
        assert!((got_m - 70.0).abs() < 0.5, "expected ~70 m, got {got_m:.2} m");
        assert!((t - 0.5).abs() < 1e-3, "expected t ~0.5, got {t}");
    }

    #[test]
    fn antimeridian_short_path_handled() {
        // Two points either side of the dateline. Spherical great-circle
        // takes the short way around — Haversine does the right thing
        // because it uses (lng2 - lng1) as a sin argument and the periodicity
        // of sin handles the wrap. The actual short-path distance from
        // 179°E to 179°W at the equator is ~222 km (2° of arc), not
        // ~39 700 km.
        let d = haversine_m(0.0, 179.0, 0.0, -179.0);
        let km = d / 1000.0;
        assert!(
            (220.0..225.0).contains(&km),
            "expected ~222 km across antimeridian, got {km:.1} km"
        );
    }
}
