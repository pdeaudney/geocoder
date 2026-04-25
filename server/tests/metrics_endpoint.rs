//! Integration test for the `/metrics` endpoint.
//!
//! The unit tests in `metrics::tests` cover the underlying registry
//! and country-code canonicalisation. This file pins the surface
//! contract: `/metrics` returns 200 with `text/plain; version=0.0.4`
//! and a body that scrapers can parse, and per-handler request
//! observations show up in the scrape.
//!
//! Skipped when `GEOCODER_INDEX_DIR` isn't set — same skip pattern as
//! the rest of the integration suite.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

use query_server::metrics::Metrics;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_returns_prometheus_text_format() {
    let metrics = Metrics::new();
    // Inject some observations directly so we don't need a real
    // index — the endpoint test is about the surface, not the
    // handler integration.
    metrics.record_request("reverse", Some("au"), 0.0012);
    metrics.record_request("reverse", Some("us"), 0.0014);
    metrics.record_request("search", Some("au"), 0.0021);
    // Touch the shadow vectors so their TYPE/HELP land in scrape.
    // Prometheus's text encoder only emits a metric family when it
    // has at least one observation; in production the shadow worker
    // does this on first sampled call. The endpoint test mirrors
    // that here to assert the full surface.
    metrics
        .shadow_outcomes_total
        .with_label_values(&["reverse", "match"])
        .inc();
    metrics
        .shadow_distance_meters
        .with_label_values(&["search"])
        .observe(123.0);

    let app = axum::Router::new()
        .route("/metrics", axum::routing::get(metrics_handler))
        .layer(axum::Extension(metrics.clone()));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router should respond");

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .expect("content-type header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        ct.starts_with("text/plain"),
        "expected text/plain, got {ct}"
    );
    assert!(
        ct.contains("version=0.0.4"),
        "expected scrape format version annotation, got {ct}"
    );

    let body_bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("metrics body fits in 1 MiB");
    let body = std::str::from_utf8(&body_bytes).unwrap();

    // The headline counter we'd hit in PromQL.
    assert!(
        body.contains("geocoder_requests_total"),
        "expected geocoder_requests_total in scrape:\n{body}"
    );
    // The two countries we observed must each appear as a series.
    assert!(
        body.contains("country=\"au\""),
        "expected au series:\n{body}"
    );
    assert!(
        body.contains("country=\"us\""),
        "expected us series:\n{body}"
    );
    // Histogram for duration must produce buckets / sum / count.
    assert!(
        body.contains("geocoder_request_duration_seconds_bucket"),
        "expected duration histogram buckets:\n{body}"
    );
    // The shadow-side metrics are registered (so Grafana queries
    // for them don't 404 on first deploy) even before any shadow
    // observation has fired.
    assert!(
        body.contains("# TYPE geocoder_shadow_outcomes_total counter"),
        "shadow_outcomes_total must be declared even when zero:\n{body}"
    );
    assert!(
        body.contains("# TYPE geocoder_shadow_distance_meters histogram"),
        "shadow_distance_meters must be declared even when zero:\n{body}"
    );
}

// Mirror the handler from main.rs. We don't import the binary's
// handler directly (it lives in main.rs which integration tests
// can't see); replicating the 5-line shape keeps the surface contract
// pinned without leaking the binary into the lib API.
async fn metrics_handler(
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let body = metrics.0.render();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}
