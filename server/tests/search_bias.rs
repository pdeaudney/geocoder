//! Proximity-bias search tests on the gRPC handler surface.
//!
//! `bias_lat`/`bias_lng` re-rank results so geographically-close
//! matches outrank far ones at similar BM25 scores. The same wiring
//! lives behind REST `/search?bias_lat=&bias_lng=` — see
//! `server/src/main.rs::search`.
//!
//! Validation tests run without a real index. Behaviour tests require
//! `GEOCODER_INDEX_DIR` (AU) for Saint-Kilda-style same-name cases or
//! `GEOCODER_PLANET_INDEX_DIR` for the cross-country Cambridge case.

#![cfg(feature = "grpc")]
#![cfg(feature = "forward")]

use arc_swap::ArcSwap;
use query_server::autocomplete::Autocomplete;
use query_server::forward::{BiasCoord, Forward};
use query_server::grpc_service::{
    proto::SearchRequest, Geocoder, GeocoderService,
};
use query_server::{Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL};
use std::path::PathBuf;
use std::sync::Arc;
use tonic::{Code, Request};

fn try_service(env_var: &str) -> Option<GeocoderService> {
    let dir = std::env::var(env_var).ok()?;
    let path = PathBuf::from(&dir);
    let idx = Index::load(
        path.to_str()?,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )
    .ok()?;
    let forward = Forward::open(&path)
        .ok()
        .filter(|f| !f.is_empty())
        .map(Arc::new);
    let autocomplete = Autocomplete::open(&path).ok().flatten().map(Arc::new);
    Some(GeocoderService {
        index: Arc::new(ArcSwap::from_pointee(idx)),
        forward,
        autocomplete,
        ip_db: None,
    })
}

// --- Validation (run without an index) ---

#[test]
fn bias_coord_rejects_invalid_lat() {
    assert_eq!(BiasCoord::try_new(91.0, 0.0).unwrap_err(), "bias_lat");
    assert_eq!(BiasCoord::try_new(-91.0, 0.0).unwrap_err(), "bias_lat");
    assert_eq!(BiasCoord::try_new(f64::NAN, 0.0).unwrap_err(), "bias_lat");
    assert_eq!(BiasCoord::try_new(f64::INFINITY, 0.0).unwrap_err(), "bias_lat");
}

#[test]
fn bias_coord_rejects_invalid_lng() {
    assert_eq!(BiasCoord::try_new(0.0, 181.0).unwrap_err(), "bias_lng");
    assert_eq!(BiasCoord::try_new(0.0, -181.0).unwrap_err(), "bias_lng");
    assert_eq!(BiasCoord::try_new(0.0, f64::NAN).unwrap_err(), "bias_lng");
}

#[test]
fn bias_coord_accepts_valid_range() {
    assert!(BiasCoord::try_new(0.0, 0.0).is_ok());
    assert!(BiasCoord::try_new(90.0, 180.0).is_ok());
    assert!(BiasCoord::try_new(-90.0, -180.0).is_ok());
    // Boundaries are inclusive.
    assert!(BiasCoord::try_new(89.99999, 179.99999).is_ok());
}

#[tokio::test]
async fn grpc_search_rejects_half_bias_lat_only() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "Cambridge".into(),
            bias_lat: Some(42.0),
            bias_lng: None,
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("half bias must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("bias_lat") && err.message().contains("bias_lng"));
}

#[tokio::test]
async fn grpc_search_rejects_half_bias_lng_only() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "Cambridge".into(),
            bias_lat: None,
            bias_lng: Some(-71.0),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("half bias must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn grpc_search_rejects_out_of_range_lat() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "Sydney".into(),
            bias_lat: Some(91.0),
            bias_lng: Some(0.0),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("out-of-range lat must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("bias_lat"));
}

#[tokio::test]
async fn grpc_search_rejects_out_of_range_lng() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "Sydney".into(),
            bias_lat: Some(0.0),
            bias_lng: Some(181.0),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("out-of-range lng must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("bias_lng"));
}

// --- Behavioural (require an index) ---

fn haversine_km(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let r = 6_371.0_f64;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlat = (lat2 - lat1).to_radians();
    let dlng = (lng2 - lng1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + p1.cos() * p2.cos() * (dlng / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

/// AU smoke index has multiple suburbs named `St Kilda` (Melbourne in
/// VIC, plus a smaller one in SA at -34.74°). With a Melbourne-area
/// bias, the closer match must outrank the SA one. Without bias, both
/// are candidates and BM25 alone won't reliably pick Melbourne first.
#[tokio::test]
async fn grpc_search_st_kilda_with_melbourne_bias_picks_melbourne() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let melb = (-37.81, 144.96); // Melbourne CBD
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "St Kilda".into(),
            country_code: "AU".into(),
            kind: "place".into(),
            limit: 5,
            bias_lat: Some(melb.0),
            bias_lng: Some(melb.1),
            ..Default::default()
        }))
        .await
        .expect("search ok");
    let hits = resp.into_inner().results;
    assert!(!hits.is_empty(), "no hits for biased St Kilda search");
    let top = &hits[0];
    let d = haversine_km(top.lat, top.lon, melb.0, melb.1);
    assert!(
        d < 100.0,
        "Melbourne-biased top hit should be near Melbourne; got ({}, {}) which is {:.1} km away",
        top.lat, top.lon, d
    );
}

#[tokio::test]
async fn grpc_search_st_kilda_with_adelaide_bias_picks_sa() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let adelaide = (-34.93, 138.60);
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "St Kilda".into(),
            country_code: "AU".into(),
            kind: "place".into(),
            limit: 5,
            bias_lat: Some(adelaide.0),
            bias_lng: Some(adelaide.1),
            ..Default::default()
        }))
        .await
        .expect("search ok");
    let hits = resp.into_inner().results;
    assert!(!hits.is_empty(), "no hits for Adelaide-biased St Kilda");
    // SA St Kilda is at ~(-34.74, 138.51). Should be closer to
    // Adelaide than Melbourne is.
    let top = &hits[0];
    let d_to_adelaide = haversine_km(top.lat, top.lon, adelaide.0, adelaide.1);
    let d_to_melb = haversine_km(top.lat, top.lon, -37.81, 144.96);
    assert!(
        d_to_adelaide < d_to_melb,
        "Adelaide-biased top hit should be closer to Adelaide than to Melbourne; \
         got ({}, {}) — d_to_adelaide={:.1} km, d_to_melb={:.1} km",
        top.lat, top.lon, d_to_adelaide, d_to_melb
    );
}

/// Sydney is unique on the AU index — biasing should NOT change the
/// answer (BM25 dominates because there's only one strong match).
#[tokio::test]
async fn grpc_search_sydney_unaffected_by_bias() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let london = (51.5074, -0.1278);
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "Sydney".into(),
            country_code: "AU".into(),
            kind: "place".into(),
            limit: 1,
            bias_lat: Some(london.0),
            bias_lng: Some(london.1),
            ..Default::default()
        }))
        .await
        .expect("search ok");
    let hits = resp.into_inner().results;
    assert!(!hits.is_empty(), "no hits for Sydney with London bias");
    let top = &hits[0];
    // Sydney AU is at ~(-33.87, 151.21). Bias to London shouldn't
    // pull the answer halfway across the world.
    assert!(
        top.lat < -10.0 && top.lon > 110.0,
        "Sydney with London bias should still resolve to AU coords; got ({}, {})",
        top.lat, top.lon
    );
}

/// Cross-country same-name disambiguation. Cambridge UK (52.20, 0.12)
/// vs Cambridge MA (42.37, -71.11). Planet-gated.
#[tokio::test]
async fn grpc_search_cambridge_with_us_bias_picks_ma() {
    let Some(svc) = try_service("GEOCODER_PLANET_INDEX_DIR") else { return };
    let boston = (42.36, -71.06);
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "Cambridge".into(),
            kind: "place".into(),
            limit: 5,
            bias_lat: Some(boston.0),
            bias_lng: Some(boston.1),
            ..Default::default()
        }))
        .await
        .expect("search ok");
    let hits = resp.into_inner().results;
    assert!(!hits.is_empty(), "no hits for Cambridge with Boston bias");
    let top = &hits[0];
    assert!(
        top.lon < -50.0,
        "Boston-biased Cambridge should land in the Americas; got ({}, {})",
        top.lat, top.lon
    );
}

#[tokio::test]
async fn grpc_search_cambridge_with_uk_bias_picks_uk() {
    let Some(svc) = try_service("GEOCODER_PLANET_INDEX_DIR") else { return };
    let london = (51.5074, -0.1278);
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "Cambridge".into(),
            kind: "place".into(),
            limit: 5,
            bias_lat: Some(london.0),
            bias_lng: Some(london.1),
            ..Default::default()
        }))
        .await
        .expect("search ok");
    let hits = resp.into_inner().results;
    assert!(!hits.is_empty(), "no hits for Cambridge with UK bias");
    let top = &hits[0];
    let cambridge_uk = (52.20, 0.12);
    let d = haversine_km(top.lat, top.lon, cambridge_uk.0, cambridge_uk.1);
    assert!(
        d < 50.0,
        "UK-biased Cambridge should land near Cambridge UK; got ({}, {}) — {:.1} km",
        top.lat, top.lon, d
    );
}

/// Limit > 1 must NOT take the FST fast-path even without bias —
/// otherwise the multi-result behaviour breaks.
#[tokio::test]
async fn grpc_search_limit_gt_1_returns_multiple_when_available() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "St Kilda".into(),
            country_code: "AU".into(),
            kind: "place".into(),
            limit: 5,
            ..Default::default()
        }))
        .await
        .expect("search ok");
    let hits = resp.into_inner().results;
    // There are at least two St Kildas in AU (VIC + SA). With limit=5
    // we should see more than one.
    assert!(
        hits.len() >= 2,
        "limit=5 search for St Kilda should return >=2 results, got {}: {:?}",
        hits.len(),
        hits.iter().map(|h| (&h.name, h.lat, h.lon)).collect::<Vec<_>>()
    );
}
