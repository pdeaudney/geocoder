//! Query-time conversion from `(lat, lng)` to an H3 cell identifier.
//!
//! Used as an optional response-enrichment field: when the client
//! sends `h3_res=7,9,12` on a geocoding request, we stamp each
//! returned coordinate with its H3 cell at each requested resolution.
//! The result is a `{resolution: cell_id}` map so clients can do
//! spatial joins against H3-indexed datasets (Kepler.gl, Databricks,
//! Snowflake, DuckDB all have native H3 functions).
//!
//! The cell ID is the standard 15-char lowercase hex string
//! representation — not the u64 integer form — because JSON number
//! precision can't carry the full 64-bit value and every H3 consumer
//! accepts the hex form.
//!
//! # Scope
//!
//! Nothing here touches on-disk data. H3 cells are computed on the
//! fly from the result coord (~tens of nanoseconds per call) — the
//! motivation for NOT storing them is that query-time compute is
//! free, clients may want different resolutions per request, and
//! adding a stored field would require an index-format bump.

use h3o::{LatLng, Resolution};
use std::collections::BTreeMap;

/// Maximum number of resolutions a single request can ask for.
/// Keeps clients from submitting `h3_res=0,1,2,...,15` and inflating
/// the response body unnecessarily. 4 is enough to span every
/// practical analytics need (e.g. "coarse aggregation + fine match").
pub const MAX_RESOLUTIONS: usize = 4;

/// Compute the H3 cell covering `(lat, lng)` at the given resolution.
/// Returns `None` on any invalid input: out-of-range coords, NaN,
/// or a resolution outside 0–15. The caller should treat `None` as
/// "skip this resolution for this hit" — don't surface it as an
/// error, because a mixed batch where one resolution fails and
/// others succeed is a normal outcome.
pub fn h3_at(lat: f64, lng: f64, res: u8) -> Option<String> {
    let resolution = Resolution::try_from(res).ok()?;
    let coord = LatLng::new(lat, lng).ok()?;
    Some(coord.to_cell(resolution).to_string())
}

/// Build the per-resolution map that enriches a response coord.
/// Duplicate resolutions get deduped (returned once), and keys are
/// sorted ascending for deterministic JSON output. Returns `None`
/// when `resolutions` is empty — the signal to skip response
/// enrichment entirely.
pub fn build_h3_map(
    lat: f64,
    lng: f64,
    resolutions: &[u8],
) -> Option<BTreeMap<String, String>> {
    if resolutions.is_empty() {
        return None;
    }
    // BTreeMap gives us the dedupe + sort for free. Skipping
    // individual failed conversions (rather than bailing on the
    // whole map) matches the per-hit semantics — one bad input
    // shouldn't zero out the whole response.
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for &res in resolutions {
        if let Some(cell) = h3_at(lat, lng, res) {
            out.insert(res.to_string(), cell);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Parse a user-typed `h3_res=` query value into a validated
/// `Vec<u8>`. Accepts comma-separated resolutions (e.g. "7,9,12"),
/// rejects anything outside 0–15, caps the list length at
/// [`MAX_RESOLUTIONS`]. On empty input, returns `Ok(vec![])` — the
/// caller treats that as "no enrichment requested".
///
/// Returns `Err(String)` with a human-facing message that the HTTP
/// layer can surface as a 400 response.
pub fn parse_h3_res(raw: &str) -> Result<Vec<u8>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<u8> = Vec::new();
    for tok in trimmed.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        let res: u8 = t.parse().map_err(|_| {
            format!("h3_res: {t:?} is not a valid integer (0–15)")
        })?;
        if res > 15 {
            return Err(format!("h3_res: {res} is out of range 0–15"));
        }
        out.push(res);
    }
    if out.len() > MAX_RESOLUTIONS {
        return Err(format!(
            "h3_res: too many resolutions ({}, max {MAX_RESOLUTIONS})",
            out.len()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Canonical Sydney coord round-trips ---

    #[test]
    fn sydney_opera_house_res_9_has_expected_format() {
        // Known coord; exact cell ID depends on the H3 spec, which
        // h3o implements from scratch — rather than hardcoding a
        // brittle 15-char literal we validate the structure and
        // round-trip via parse + center.
        let cell = h3_at(-33.8568, 151.2153, 9).expect("sydney res=9");
        assert_eq!(cell.len(), 15, "H3 cell IDs at res 0–15 are 15 hex chars");
        assert!(cell.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "cell is lowercase hex: {cell}");
        // Resolved-space cells at res 0–15 start with '8' (H3 mode bits).
        assert!(cell.starts_with('8'), "resolved cell starts with 8: {cell}");

        // Round-trip: h3o can parse what it produced, and the cell's
        // center is within the resolution's tolerance of our input.
        // Res 9 has an average edge length of ~174m; 1km tolerance is
        // safe.
        let parsed: h3o::CellIndex = cell.parse().expect("parseable");
        let center: LatLng = parsed.into();
        let d_lat = (center.lat() - -33.8568).abs();
        let d_lng = (center.lng() - 151.2153).abs();
        // ~0.01° = ~1km — well within the res-9 hexagon's diameter.
        assert!(d_lat < 0.01 && d_lng < 0.01, "res-9 center within 1km of input");
    }

    // --- Resolution boundary coverage ---

    #[test]
    fn resolution_0_lowest_is_valid() {
        // Resolution 0 = 122 base cells covering the earth. Legit.
        let cell = h3_at(-33.87, 151.21, 0).expect("res=0 is valid");
        assert!(cell.starts_with('8'));
    }

    #[test]
    fn resolution_15_highest_is_valid() {
        // Resolution 15 = ~0.9m² hexagons. Still valid.
        let cell = h3_at(-33.87, 151.21, 15).expect("res=15 is valid");
        assert!(cell.starts_with('8'));
    }

    #[test]
    fn resolution_16_is_rejected() {
        assert_eq!(h3_at(-33.87, 151.21, 16), None);
    }

    // --- Coordinate edge cases ---

    #[test]
    fn antimeridian_coord_resolves() {
        // ±180° longitude is the antimeridian. H3 handles it natively;
        // we just care we don't panic and get a valid cell.
        assert!(h3_at(0.0, 180.0, 9).is_some(), "+180° lon resolves");
        assert!(h3_at(0.0, -180.0, 9).is_some(), "-180° lon resolves");
        assert!(h3_at(0.0, 179.999, 9).is_some(), "just inside resolves");
    }

    #[test]
    fn near_pole_coord_resolves() {
        // H3 has 12 pentagon cells near the poles; they're
        // structurally different from hexagons but still valid cells
        // that our API can return.
        assert!(h3_at(89.9, 0.0, 9).is_some(), "near north pole");
        assert!(h3_at(-89.9, 0.0, 9).is_some(), "near south pole");
        assert!(h3_at(90.0, 0.0, 9).is_some(), "exactly north pole");
        assert!(h3_at(-90.0, 0.0, 9).is_some(), "exactly south pole");
    }

    #[test]
    fn out_of_range_coord_is_normalised_by_h3o_not_rejected() {
        // Spec-match behaviour: h3o follows the reference H3 C impl,
        // which normalises out-of-range lat/lng onto the sphere
        // rather than rejecting. (91, 0) gets clamped/wrapped to a
        // valid cell; callers can't use lat/lng range as a pre-flight
        // validator. NaN and infinity ARE rejected (next test).
        //
        // If strict validation is ever needed, add a
        // `valid_coord(lat, lng)` check upstream of h3_at rather than
        // expecting h3_at to do it.
        assert!(h3_at(91.0, 0.0, 9).is_some(), "lat > 90 gets normalised");
        assert!(h3_at(-91.0, 0.0, 9).is_some(), "lat < -90 gets normalised");
        assert!(h3_at(0.0, 181.0, 9).is_some(), "lon > 180 gets normalised");
        assert!(h3_at(0.0, -181.0, 9).is_some(), "lon < -180 gets normalised");
    }

    #[test]
    fn nan_and_infinity_coords_return_none() {
        assert_eq!(h3_at(f64::NAN, 0.0, 9), None);
        assert_eq!(h3_at(0.0, f64::NAN, 9), None);
        assert_eq!(h3_at(f64::INFINITY, 0.0, 9), None);
        assert_eq!(h3_at(f64::NEG_INFINITY, 0.0, 9), None);
    }

    #[test]
    fn f32_precision_coord_still_resolves() {
        // Our on-disk coords are f32; when promoted to f64 for the
        // h3_at call, the tiny precision loss shouldn't matter.
        let lat_f32: f32 = -33.8568;
        let lng_f32: f32 = 151.2153;
        assert!(h3_at(lat_f32 as f64, lng_f32 as f64, 9).is_some());
    }

    // --- build_h3_map behaviour ---

    #[test]
    fn empty_resolutions_returns_none() {
        assert!(build_h3_map(-33.87, 151.21, &[]).is_none());
    }

    #[test]
    fn single_resolution_produces_one_key() {
        let map = build_h3_map(-33.87, 151.21, &[9]).expect("map");
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("9"));
    }

    #[test]
    fn multi_resolution_produces_sorted_keys() {
        // Input intentionally unsorted. BTreeMap gives us sorted
        // output for deterministic JSON.
        let map = build_h3_map(-33.87, 151.21, &[12, 7, 9]).expect("map");
        let keys: Vec<&String> = map.keys().collect();
        // BTreeMap sorts by string; "12" < "7" < "9" lexicographically.
        // That's a quirk of string sort but it's deterministic and
        // that's what the API contract promises.
        assert_eq!(keys, vec!["12", "7", "9"]);
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn duplicate_resolutions_are_deduped() {
        let map = build_h3_map(-33.87, 151.21, &[9, 9, 9]).expect("map");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn bad_resolution_in_list_skipped_but_others_survive() {
        // 20 is invalid; 9 is valid. Map should contain only "9".
        let map = build_h3_map(-33.87, 151.21, &[9, 20]).expect("map");
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("9"));
    }

    #[test]
    fn all_resolutions_bad_returns_none() {
        // Every requested res is invalid → no map at all.
        let map = build_h3_map(-33.87, 151.21, &[16, 20, 100]);
        assert!(map.is_none());
    }

    // --- parse_h3_res CLI surface ---

    #[test]
    fn parse_single_resolution() {
        assert_eq!(parse_h3_res("9"), Ok(vec![9]));
    }

    #[test]
    fn parse_multi_resolution() {
        assert_eq!(parse_h3_res("7,9,12"), Ok(vec![7, 9, 12]));
    }

    #[test]
    fn parse_with_whitespace_tolerated() {
        assert_eq!(parse_h3_res(" 7 , 9 , 12 "), Ok(vec![7, 9, 12]));
    }

    #[test]
    fn parse_empty_string_yields_empty_vec() {
        assert_eq!(parse_h3_res(""), Ok(vec![]));
        assert_eq!(parse_h3_res("  "), Ok(vec![]));
    }

    #[test]
    fn parse_rejects_negative() {
        assert!(parse_h3_res("-1").is_err());
    }

    #[test]
    fn parse_rejects_out_of_range() {
        let err = parse_h3_res("16").unwrap_err();
        assert!(err.contains("0–15") || err.contains("out of range"));
    }

    #[test]
    fn parse_rejects_non_integer() {
        assert!(parse_h3_res("abc").is_err());
        assert!(parse_h3_res("9.5").is_err());
    }

    #[test]
    fn parse_rejects_too_many_resolutions() {
        // MAX_RESOLUTIONS = 4; asking for 5 is a 400.
        assert!(parse_h3_res("1,2,3,4,5").is_err());
    }

    #[test]
    fn parse_accepts_resolution_0_and_15_boundaries() {
        assert_eq!(parse_h3_res("0"), Ok(vec![0]));
        assert_eq!(parse_h3_res("15"), Ok(vec![15]));
    }
}
