//! End-to-end tests for the per-field length caps on the gRPC
//! surface. REST handlers use the same `query_server::limits::check`
//! helper — the cap logic itself is unit-tested in
//! `server/src/limits.rs`; this file pins the integration point on
//! the gRPC service so the two surfaces can't drift on what they
//! reject.
//!
//! Skipped when `GEOCODER_INDEX_DIR` is not set (constructing a
//! `GeocoderService` requires a loaded `Index` even though the
//! length check fires before any index access).

#![cfg(feature = "grpc")]

use arc_swap::ArcSwap;
use query_server::grpc_service::{
    proto::{
        AutocompleteRequest, IpGeocodeRequest, ReverseRequest, SearchRequest, ValidateRequest,
    },
    Geocoder, GeocoderService,
};
use query_server::limits;
use query_server::{Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL};
use std::sync::Arc;
use tonic::{Code, Request};

fn try_service() -> Option<GeocoderService> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let idx = Index::load(
        &dir,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )
    .ok()?;
    Some(GeocoderService {
        index: Arc::new(ArcSwap::from_pointee(idx)),
        #[cfg(feature = "forward")]
        forward: None,
        #[cfg(feature = "forward")]
        autocomplete: None,
        ip_db: None,
    })
}

#[tokio::test]
async fn search_rejects_oversize_q() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "a".repeat(limits::SEARCH_Q + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize q must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument, "got {err:?}");
    assert!(err.message().contains("q"), "msg: {}", err.message());
}

#[tokio::test]
async fn search_rejects_oversize_street() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            street: "a".repeat(limits::STRUCTURED_FIELD + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize street must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("street"));
}

#[tokio::test]
async fn search_rejects_oversize_city() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            city: "a".repeat(limits::STRUCTURED_FIELD + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize city must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("city"));
}

#[tokio::test]
async fn search_rejects_oversize_state() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            state: "a".repeat(limits::STRUCTURED_FIELD + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize state must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("state"));
}

#[tokio::test]
async fn search_rejects_oversize_housenumber() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            housenumber: "9".repeat(limits::HOUSENUMBER + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize housenumber must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("housenumber"));
}

#[tokio::test]
async fn search_rejects_oversize_country_code_list() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            country_code: "AU,".repeat(limits::COUNTRY_CODE_LIST),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize country_code must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("country_code"));
}

#[tokio::test]
async fn autocomplete_rejects_oversize_q() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .autocomplete(Request::new(AutocompleteRequest {
            q: "a".repeat(limits::AUTOCOMPLETE_Q + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize autocomplete q must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("q"));
}

/// Autocomplete uses the SINGLE country-code cap (2 bytes), not
/// the multi-list one. A 3-byte input must be rejected.
#[tokio::test]
async fn autocomplete_rejects_oversize_country_code() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .autocomplete(Request::new(AutocompleteRequest {
            q: "syd".into(),
            country_code: "AUS".into(), // 3 bytes, over the 2-byte cap
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize country_code must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("country_code"));
}

#[tokio::test]
async fn validate_rejects_oversize_postcode() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .validate(Request::new(ValidateRequest {
            street: "Main St".into(),
            city: "Sydney".into(),
            country_code: "AU".into(),
            postcode: "1".repeat(limits::POSTCODE + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize postcode must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("postcode"));
}

#[tokio::test]
async fn validate_rejects_oversize_housenumber() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .validate(Request::new(ValidateRequest {
            street: "Main St".into(),
            city: "Sydney".into(),
            country_code: "AU".into(),
            housenumber: "9".repeat(limits::HOUSENUMBER + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize housenumber must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("housenumber"));
}

#[tokio::test]
async fn validate_rejects_oversize_street() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .validate(Request::new(ValidateRequest {
            street: "a".repeat(limits::STRUCTURED_FIELD + 1),
            city: "Sydney".into(),
            country_code: "AU".into(),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize validate street must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("street"));
}

#[tokio::test]
async fn validate_rejects_oversize_city() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .validate(Request::new(ValidateRequest {
            street: "Main St".into(),
            city: "a".repeat(limits::STRUCTURED_FIELD + 1),
            country_code: "AU".into(),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize validate city must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("city"));
}

#[tokio::test]
async fn validate_rejects_oversize_state() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .validate(Request::new(ValidateRequest {
            street: "Main St".into(),
            city: "Sydney".into(),
            country_code: "AU".into(),
            state: "a".repeat(limits::STRUCTURED_FIELD + 1),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize validate state must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("state"));
}

#[tokio::test]
async fn validate_rejects_oversize_country_code() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .validate(Request::new(ValidateRequest {
            street: "Main St".into(),
            city: "Sydney".into(),
            country_code: "AU,".repeat(limits::COUNTRY_CODE_LIST),
            ..Default::default()
        }))
        .await;
    let err = resp.expect_err("oversize validate country_code must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("country_code"));
}

#[tokio::test]
async fn reverse_rejects_oversize_lang() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .reverse(Request::new(ReverseRequest {
            lat: 0.0,
            lon: 0.0,
            lang: "x".repeat(limits::LANG + 1),
            h3_res: vec![],
        }))
        .await;
    let err = resp.expect_err("oversize lang must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("lang"));
}

#[tokio::test]
async fn ip_geocode_rejects_oversize_ip_string() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .ip_geocode(Request::new(IpGeocodeRequest {
            ip: "x".repeat(limits::IP + 1),
            h3_res: vec![],
        }))
        .await;
    let err = resp.expect_err("oversize ip must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("ip"));
}

/// Inputs at exactly the cap boundary must NOT be rejected as
/// "too long". They may still fail downstream for other reasons
/// (e.g. forward index disabled returns Unimplemented) — but the
/// failure must not be `InvalidArgument` produced by the length
/// check. Off-by-one on the comparison would silently exclude
/// legitimate boundary inputs.
#[tokio::test]
async fn search_at_cap_boundary_passes_length_check() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .search(Request::new(SearchRequest {
            q: "a".repeat(limits::SEARCH_Q),
            ..Default::default()
        }))
        .await;
    if let Err(err) = resp {
        assert_ne!(
            err.code(),
            Code::InvalidArgument,
            "boundary-length input incorrectly rejected by the length check: {}",
            err.message(),
        );
    }
}

#[tokio::test]
async fn autocomplete_at_cap_boundary_passes_length_check() {
    let Some(svc) = try_service() else { return };
    let resp = svc
        .autocomplete(Request::new(AutocompleteRequest {
            q: "a".repeat(limits::AUTOCOMPLETE_Q),
            ..Default::default()
        }))
        .await;
    if let Err(err) = resp {
        assert_ne!(
            err.code(),
            Code::InvalidArgument,
            "boundary-length autocomplete q incorrectly rejected: {}",
            err.message(),
        );
    }
}

/// Cap CONSTANTS themselves must stay above the realistic upper
/// bound of real user inputs. If a future commit tightens these
/// too aggressively, this test fails — independent of any service
/// state, so it always runs.
#[test]
fn cap_constants_above_realistic_lower_bounds() {
    // Pelias accepts up to ~256 chars of search q; we should be at
    // least that.
    assert!(limits::SEARCH_Q >= 256);
    // BCP-47 with subtags reaches `zh-Hant-TW` (10).
    assert!(limits::LANG >= 10);
    // IPv6 with zone id (`fe80::1234:5678:9abc:def0%enp0s31f6`) ~37
    // chars; must fit comfortably.
    assert!(limits::IP >= 45);
    // 8-letter postcodes (`SW1A 1AA`) plus headroom for non-GB
    // formats.
    assert!(limits::POSTCODE >= 10);
    // Comma-separated multi-country (`US,CA,MX,…`) for at least 4
    // codes = 11 chars.
    assert!(limits::COUNTRY_CODE_LIST >= 11);
    // Tokens for "Strait of Magellan", "Avenue Frederic Mistral",
    // longest realistic OSM names ~70 chars; cap should comfortably
    // exceed.
    assert!(limits::STRUCTURED_FIELD >= 100);
}
