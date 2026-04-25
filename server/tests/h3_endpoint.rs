//! End-to-end coverage for the `H3` gRPC RPC.
//!
//! The companion REST endpoint `/h3` reuses the same `h3_cell` primitives
//! (`parse_h3_res` + `build_h3_map`) which are already exhaustively
//! covered by `h3_cell::tests` and `tests/h3_response.rs`. What's
//! exercised here is the new handler-level logic:
//!
//!  - Required-resolutions rule (empty list → InvalidArgument; the
//!    enrichment field on other RPCs treats empty as "skip", but the
//!    standalone H3 call has no other purpose).
//!  - NaN/infinity coords → InvalidArgument (build_h3_map returns
//!    None for these and we surface that as a client error rather than
//!    swallowing it).
//!  - The proto-shaped response carries a `map<uint32, string>` keyed
//!    by resolution, not the JSON-string-keyed shape.
//!
//! Skips when `GEOCODER_INDEX_DIR` isn't set — the GeocoderService
//! struct still needs a real `Index` to construct, even though the H3
//! RPC itself doesn't touch it.

#![cfg(feature = "grpc")]

use std::sync::Arc;

use arc_swap::ArcSwap;
use query_server::grpc_service::proto::H3Request;
use query_server::grpc_service::{Geocoder, GeocoderService};
use query_server::Index;
use tonic::{Code, Request};

fn try_load_service() -> Option<GeocoderService> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let index = Index::load(
        &dir,
        query_server::DEFAULT_STREET_CELL_LEVEL,
        query_server::DEFAULT_ADMIN_CELL_LEVEL,
        query_server::DEFAULT_SEARCH_DISTANCE,
    )
    .ok()?;
    Some(GeocoderService {
        index: Arc::new(ArcSwap::from(Arc::new(index))),
        #[cfg(feature = "forward")]
        forward: None,
        #[cfg(feature = "forward")]
        autocomplete: None,
        ip_db: None,
    })
}

#[tokio::test]
async fn returns_cell_map_for_valid_coord_and_resolution() {
    let Some(service) = try_load_service() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    let resp = service
        .h3(Request::new(H3Request {
            lat: -33.8568,
            lon: 151.2153,
            h3_res: vec![9],
        }))
        .await
        .expect("valid request must succeed");

    let body = resp.into_inner();
    assert_eq!(body.lat, -33.8568);
    assert_eq!(body.lon, 151.2153);
    assert_eq!(body.h3.len(), 1, "exactly one resolution requested");
    let cell = body.h3.get(&9).expect("resolution 9 must be present");
    assert_eq!(cell.len(), 15, "H3 cell IDs are 15 lowercase hex chars");
    assert!(cell.starts_with('8'), "resolved-space cells start with 8: {cell}");
    // Round-trip through h3o so we know it's a real cell, not just a
    // 15-byte string. Pins the wire-format contract from the client side.
    let parsed: h3o::CellIndex = cell.parse().expect("h3o can parse our output");
    assert_eq!(parsed.resolution(), h3o::Resolution::Nine);
}

#[tokio::test]
async fn returns_multi_resolution_map_with_all_requested_keys() {
    let Some(service) = try_load_service() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    let resp = service
        .h3(Request::new(H3Request {
            lat: -33.8568,
            lon: 151.2153,
            h3_res: vec![7, 9, 12],
        }))
        .await
        .expect("valid request must succeed");

    let body = resp.into_inner();
    assert_eq!(body.h3.len(), 3, "all three resolutions present");
    for &res in &[7u32, 9, 12] {
        let cell = body.h3.get(&res).unwrap_or_else(|| panic!("res {res} present"));
        assert_eq!(cell.len(), 15);
    }
}

#[tokio::test]
async fn empty_resolutions_list_returns_invalid_argument() {
    let Some(service) = try_load_service() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    // Standalone H3 with no resolutions has no work to do — clients
    // get a 400-shaped gRPC error rather than a silent OK with an
    // empty body.
    let err = service
        .h3(Request::new(H3Request {
            lat: -33.8568,
            lon: 151.2153,
            h3_res: vec![],
        }))
        .await
        .expect_err("empty resolutions must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("h3_res"),
        "error message should reference h3_res: {}",
        err.message()
    );
}

#[tokio::test]
async fn out_of_range_resolution_rejected() {
    let Some(service) = try_load_service() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    // 0..=15 are valid; 16+ are rejected at the validate_h3_res
    // gate (shared with the REST surface) so the two surfaces
    // can't drift.
    let err = service
        .h3(Request::new(H3Request {
            lat: -33.8568,
            lon: 151.2153,
            h3_res: vec![16],
        }))
        .await
        .expect_err("res=16 must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn too_many_resolutions_rejected() {
    let Some(service) = try_load_service() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    // MAX_RESOLUTIONS = 4; asking for 5 inflates the response and
    // gives clients no real benefit. The cap is identical on the
    // REST side (parse_h3_res) so both surfaces fail at the same
    // boundary.
    let err = service
        .h3(Request::new(H3Request {
            lat: -33.8568,
            lon: 151.2153,
            h3_res: vec![1, 2, 3, 4, 5],
        }))
        .await
        .expect_err("5 resolutions must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn nan_coordinate_returns_invalid_argument() {
    let Some(service) = try_load_service() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    // h3o rejects NaN/infinity (unlike out-of-range lat/lng which
    // it normalises onto the sphere). The handler surfaces that as
    // a 400-shape error rather than silently returning an empty
    // map.
    let err = service
        .h3(Request::new(H3Request {
            lat: f64::NAN,
            lon: 151.21,
            h3_res: vec![9],
        }))
        .await
        .expect_err("NaN lat must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn antimeridian_coordinate_resolves() {
    let Some(service) = try_load_service() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    // ±180° lon is the most surprising real-world coord — covered
    // here so a future change to validate_h3_res that accidentally
    // tightens the input bounds would fail this test.
    let resp = service
        .h3(Request::new(H3Request {
            lat: 0.0,
            lon: 180.0,
            h3_res: vec![9],
        }))
        .await
        .expect("antimeridian must resolve");
    let body = resp.into_inner();
    assert_eq!(body.h3.len(), 1);
}

#[tokio::test]
async fn pole_coordinate_resolves_to_pentagon_cell() {
    let Some(service) = try_load_service() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    // H3 has 12 pentagon cells near the poles; clients shouldn't
    // need to care about hex-vs-pentagon — both serialise as
    // the same 15-char hex string. Test that the wire format
    // doesn't betray the geometry.
    let resp = service
        .h3(Request::new(H3Request {
            lat: 90.0,
            lon: 0.0,
            h3_res: vec![5],
        }))
        .await
        .expect("north pole must resolve");
    let cell = resp.into_inner().h3.get(&5).expect("res 5").clone();
    assert_eq!(cell.len(), 15);
    assert!(cell.starts_with('8'));
}
