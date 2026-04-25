//! Integration tests for the OTel metrics surface.
//!
//! Two things to pin operationally:
//!   1. **Failure isolation** — a dead OTLP collector must not slow
//!      handler observations. The `record_request` / `record_shadow`
//!      methods are non-blocking; the periodic reader does its
//!      export work on a background tokio task.
//!   2. **Metrics-only deployment is viable** — the Prometheus surface
//!      keeps working when the OTel meter provider is absent (the
//!      `Metrics::new()` path with no provider).
//!
//! The unit tests in `metrics::tests` cover the dual-write semantics
//! against the Prometheus registry. A full mock-OTLP receiver test
//! would assert protobuf bytes hit the wire — that's a much larger
//! piece of test infrastructure and the OTel SDK already has its own
//! coverage for the periodic reader. This file focuses on what we
//! can break in our integration code: the wiring between
//! `telemetry::build_meter_provider` and `Metrics::with_optional_meter`,
//! and the resilience of the request path under collector outage.

use std::sync::Arc;
use std::time::{Duration, Instant};

use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::Resource;
use query_server::metrics::Metrics;

/// Build a meter provider pointed at a known-dead port, with a tight
/// periodic export interval. Mirrors the production
/// `telemetry::build_meter_provider` shape but skips the env-var
/// resolution so the test is hermetic.
fn build_doomed_meter_provider() -> SdkMeterProvider {
    // Bind+drop a TCP listener to grab a port we know nothing's
    // listening on. Same pattern as the OTLP-down test in
    // `tests/operational_resilience.rs`.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let dead_addr = listener.local_addr().expect("addr");
    drop(listener);

    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(format!("http://{dead_addr}"))
        .with_timeout(Duration::from_millis(100))
        .build()
        .expect("build doomed metrics exporter");

    let reader = PeriodicReader::builder(exporter)
        // Fire often enough to actually attempt + fail an export
        // during the test window.
        .with_interval(Duration::from_millis(200))
        .build();

    SdkMeterProvider::builder()
        .with_resource(
            Resource::builder()
                .with_service_name("otel-metrics-export-test")
                .build(),
        )
        .with_reader(reader)
        .build()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handler_path_unaffected_by_otlp_outage() {
    let provider = build_doomed_meter_provider();
    let metrics = Metrics::with_optional_meter(Some(&provider));

    // 200 fire-and-forget observations. The exporter background task
    // is failing every 200ms in the meantime — recording must stay
    // sub-millisecond regardless. 5 ms is generous; on the dev box
    // this completes in tens of microseconds even on debug builds.
    let mut max_per_call = Duration::ZERO;
    for i in 0..200 {
        let t0 = Instant::now();
        metrics.record_request("reverse", Some("au"), 0.001);
        if i % 4 == 0 {
            metrics.record_shadow(
                "reverse",
                "match",
                &[
                    ("country", Some(true)),
                    ("state", Some(true)),
                    ("city", Some(true)),
                    ("road", Some(true)),
                ],
                None,
            );
        }
        let elapsed = t0.elapsed();
        if elapsed > max_per_call {
            max_per_call = elapsed;
        }
    }
    assert!(
        max_per_call < Duration::from_millis(5),
        "max per-call latency {max_per_call:?} — observation path is awaiting OTLP"
    );

    // The Prometheus surface stays live regardless of OTLP state.
    let body = metrics.render();
    let text = std::str::from_utf8(&body).expect("utf-8");
    assert!(
        text.contains("geocoder_requests_total{country=\"au\",endpoint=\"reverse\"} 200"),
        "Prometheus surface must keep working during OTLP outage:\n{text}"
    );
    assert!(
        text.contains("geocoder_shadow_outcomes_total"),
        "shadow outcomes must keep registering:\n{text}"
    );

    // Drain the provider before the runtime drops — the exporter has
    // pending failures buffered and we want a clean tear-down.
    let _ = provider.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_new_with_meter_provider_succeeds() {
    // Smoke: building Metrics against a real (if doomed) meter
    // provider succeeds — i.e. the `with_optional_meter(Some(&_))`
    // path doesn't panic on any of the OTel instrument-builder calls.
    // This is the regression guard for an OTel SDK upgrade silently
    // changing the builder API.
    let provider = build_doomed_meter_provider();
    let metrics: Arc<Metrics> = Metrics::with_optional_meter(Some(&provider));
    metrics.record_request("h3", None, 0.0001);
    let _ = provider.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_new_without_meter_provider_succeeds() {
    // The other half — Prometheus-only deployment must keep working.
    // Already covered by the lib-side `metrics_new_works_without_meter_provider`
    // test; this version is the integration-level stability guard.
    let metrics = Metrics::with_optional_meter(None);
    metrics.record_request("h3", Some("au"), 0.0001);
    metrics.record_shadow("reverse", "match", &[], None);
    let body = metrics.render();
    assert!(!body.is_empty());
}
