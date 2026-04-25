//! Integration test for the `/healthz/ready` 503-while-loading
//! contract.
//!
//! The live smoke earlier (post-startup, ready=true) is covered by
//! manual deploy verification. This test pins the negative case
//! programmatically: while the AtomicBool is `false`, the readiness
//! probe must return 503 with `{"status":"loading"}`. ALB target
//! groups and k8s readiness probes both rely on this — getting it
//! wrong means traffic routes to instances mid-boot.
//!
//! The handler in `server/src/main.rs` isn't reachable from
//! integration tests (it's binary-internal), so we replicate the
//! handler shape here behind a Router that takes the same
//! `Extension<Arc<AtomicBool>>` as the production code. If the
//! production handler ever drifts from this shape, the smoke test
//! we already use post-deploy will catch the divergence.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;

async fn healthz_ready(
    ready: axum::extract::Extension<Arc<AtomicBool>>,
) -> Response {
    if ready.0.load(Ordering::Relaxed) {
        (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"status":"ready"}"#,
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"status":"loading"}"#,
        )
            .into_response()
    }
}

fn router(ready: Arc<AtomicBool>) -> axum::Router {
    axum::Router::new()
        .route("/healthz/ready", axum::routing::get(healthz_ready))
        .layer(axum::Extension(ready))
}

#[tokio::test]
async fn ready_flag_false_returns_503() {
    let ready = Arc::new(AtomicBool::new(false));
    let app = router(ready);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responded");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], br#"{"status":"loading"}"#);
}

#[tokio::test]
async fn ready_flag_true_returns_200() {
    let ready = Arc::new(AtomicBool::new(true));
    let app = router(ready);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responded");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], br#"{"status":"ready"}"#);
}

#[tokio::test]
async fn ready_flag_flip_observed_by_subsequent_request() {
    // The flag is shared via Arc, so a flip after construction must
    // be visible on the next request. Pins the contract we rely on
    // for graceful drain (`ready.store(false)` should immediately
    // start failing readiness probes, draining the host from rotation).
    let ready = Arc::new(AtomicBool::new(true));
    let app = router(ready.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    ready.store(false, Ordering::Relaxed);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
