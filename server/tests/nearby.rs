//! /nearby endpoint tests.
//!
//! Validation tests run without a real index. Behaviour tests gated on
//! `GEOCODER_INDEX_DIR` so they no-op in CI without one.

#![cfg(feature = "grpc")]
#![cfg(feature = "forward")]

// NearbyQuery::validate field-by-field unit tests live in
// `server/src/forward.rs::nearby_validation_tests`. This file only
// covers the gRPC handler surface (grpc validation status codes +
// live-index behaviour gated on GEOCODER_INDEX_DIR).

use arc_swap::ArcSwap;
use query_server::autocomplete::Autocomplete;
use query_server::forward::Forward;
use query_server::grpc_service::{
    proto::NearbyRequest, Geocoder, GeocoderService,
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

// --- gRPC validation (require an index — the `forward index not
//     built` short-circuit fires before validate() otherwise) ---

#[tokio::test]
async fn grpc_nearby_rejects_invalid_kind() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let resp = svc
        .nearby(Request::new(NearbyRequest {
            lat: -33.87,
            lng: 151.21,
            radius_km: 5.0,
            kind: "asdf".into(),
            ..Default::default()
        }))
        .await;
    let status = resp.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("kind"),
        "expected kind error, got: {}", status.message()
    );
}

#[tokio::test]
async fn grpc_nearby_rejects_invalid_radius() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let resp = svc
        .nearby(Request::new(NearbyRequest {
            lat: -33.87,
            lng: 151.21,
            radius_km: 200.0, // > MAX_RADIUS_KM
            ..Default::default()
        }))
        .await;
    let status = resp.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("radius_km"),
        "expected radius error, got: {}", status.message()
    );
}

// --- Behaviour (require GEOCODER_INDEX_DIR) ---

/// Centre on Sydney CBD with a 5 km radius — every hit's lat/lon must
/// be within the radius (validates the post-filter), and `distance_m`
/// must be monotonically increasing (validates the sort).
#[tokio::test]
async fn grpc_nearby_returns_hits_inside_radius_sorted() {
    let Some(svc) = try_service("GEOCODER_INDEX_DIR") else { return };
    let resp = svc
        .nearby(Request::new(NearbyRequest {
            lat: -33.8688,
            lng: 151.2093,
            radius_km: 5.0,
            limit: 10,
            ..Default::default()
        }))
        .await;
    let body = resp.expect("nearby ok").into_inner();
    if body.results.is_empty() {
        // Index might be too small; tolerate empty rather than fail.
        return;
    }

    let radius_m = 5.0 * 1_000.0;
    let mut prev_d = 0.0_f64;
    for hit in &body.results {
        assert!(
            hit.distance_m <= radius_m + 1.0,
            "hit beyond radius: {} m > {}", hit.distance_m, radius_m
        );
        assert!(
            hit.distance_m >= prev_d,
            "results not sorted by distance: {} < {}", hit.distance_m, prev_d
        );
        prev_d = hit.distance_m;
    }
}
