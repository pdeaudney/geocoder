//! Geographic helpers shared across the server, builders, and tools.
//!
//! Right now the only inhabitant is `haversine_m` — used by the
//! `regression-runner` (offline accuracy checks against fixed corpora)
//! and by the shadow-validation worker (online accuracy checks against
//! Google Geocoding). Co-locating the function here lets both callers
//! depend on the same implementation without duplicating the constants.

/// Spherical-earth great-circle distance in metres.
///
/// Accurate to ~0.3 % at any distance — well within the geocoder's own
/// tolerance (10s–1000s of metres). The error grows toward the equator
/// where the WGS-84 spheroid bulges most relative to a sphere; for the
/// "did our top hit land within N metres of the reference?" question
/// this approximation is more than enough.
///
/// Earth radius is the IUGG mean (6 371 000 m), the standard choice for
/// short-distance Haversine work.
pub fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lng2 - lng1).to_radians();
    let a = (dphi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
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
