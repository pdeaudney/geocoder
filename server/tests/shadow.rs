//! Integration tests for the shadow validator.
//!
//! Brings up a mock Google Geocoding endpoint as a small axum app on
//! a localhost port, points a `ShadowDispatcher` at it, fires sample
//! jobs, and asserts the cost-protection gates and failure-mode
//! state machines all behave correctly.
//!
//! The unit tests in `shadow::tests` cover the pure logic
//! (sampler probability, URL building, status parser, comparator,
//! master-switch matrix). Anything that needs a live worker —
//! daily-cap accounting, rate-limit backoff, REQUEST_DENIED
//! disable, queue-full backpressure, latency neutrality —
//! lives here.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use query_server::shadow::{OurSnapshot, ShadowConfig, ShadowDispatcher};
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;

// ---------------------------------------------------------------------
// Mock Google server scaffolding.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct MockState {
    /// Counter of HTTP hits — useful for asserting "Google was called
    /// exactly N times" or "no further calls after disable."
    hits: Arc<AtomicU64>,
    /// Pluggable response strategy. The closure runs for each request
    /// and returns the body the mock should serve.
    responder: Arc<dyn Fn(u64) -> MockResponse + Send + Sync>,
}

#[derive(Clone, Debug)]
enum MockResponse {
    /// Serve a 200 with the given JSON body.
    Json(serde_json::Value),
    /// Sleep before responding — used by the latency-neutrality test.
    JsonAfter(serde_json::Value, Duration),
}

#[derive(Deserialize)]
#[allow(dead_code)] // Fields exist so axum's Query extractor accepts the URL shape;
                    // the mock body is determined by the responder closure, not the
                    // parsed query, so we don't read these directly.
struct MockQuery {
    #[serde(default)]
    latlng: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

async fn mock_handler(
    Query(_q): Query<MockQuery>,
    axum::extract::Extension(state): axum::extract::Extension<MockState>,
) -> Response {
    let hit_n = state.hits.fetch_add(1, Ordering::Relaxed);
    match (state.responder)(hit_n) {
        MockResponse::Json(v) => axum::Json(v).into_response(),
        MockResponse::JsonAfter(v, dur) => {
            tokio::time::sleep(dur).await;
            axum::Json(v).into_response()
        }
    }
}

/// Spin up the mock on `127.0.0.1:0`, return the base URL we can drop
/// into `ShadowConfig::base_url`. The server task lives until the
/// process exits — tests don't bother shutting it down because the
/// runtime is dropped on test exit anyway.
async fn spawn_mock(state: MockState) -> String {
    let app = Router::new()
        .route("/maps/api/geocode/json", get(mock_handler))
        .layer(axum::Extension(state));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 127.0.0.1:0");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/maps/api/geocode/json")
}

/// Test config preset — short timeouts, tiny caps, dispatcher pointed
/// at the supplied mock URL.
fn test_config(base_url: &str) -> ShadowConfig {
    ShadowConfig {
        api_key: "test-key".into(),
        sample_rate: 1.0, // always sample
        daily_cap: 1000,
        rps_cap: 50,
        queue_capacity: 64,
        inflight_cap: 4,
        timeout: Duration::from_millis(500),
        backoff_secs: 30,
        base_url: base_url.to_string(),
    }
}

fn ok_au_body() -> serde_json::Value {
    json!({
        "status": "OK",
        "results": [{
            "formatted_address": "Sydney NSW, Australia",
            "geometry": { "location": { "lat": -33.8688, "lng": 151.2093 } },
            "address_components": [
                { "long_name": "Sydney", "short_name": "Sydney", "types": ["locality"] },
                { "long_name": "New South Wales", "short_name": "NSW", "types": ["administrative_area_level_1"] },
                { "long_name": "Australia", "short_name": "AU", "types": ["country"] }
            ]
        }]
    })
}

fn our_au_snapshot() -> OurSnapshot {
    OurSnapshot {
        country_code: Some("au".into()),
        state: Some("NSW".into()),
        city: Some("Sydney".into()),
        road: None,
        display_name: Some("Sydney, NSW, Australia".into()),
        lat: None,
        lng: None,
    }
}

fn fire_one_reverse(disp: &Arc<ShadowDispatcher>) {
    // Build the synthetic Address by hand — handlers do this from a
    // borrowed Address but for an isolated dispatcher test the owned
    // form is cleaner.
    let snap = our_au_snapshot();
    disp.shadow_search_with_snapshot("anchor", Some("au"), Some((-33.8688, 151.2093, snap)));
}

// ---------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------

/// The headline regression: a slow Google must not slow down the
/// caller. Mock sleeps 200 ms; we fire 100 sample calls and assert
/// every single one returns within a few ms. If a refactor ever
/// accidentally awaits the Google call on the request path, this
/// asserts in single-digit ms and the test fails immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handler_latency_unaffected_by_slow_google() {
    let hits = Arc::new(AtomicU64::new(0));
    let url = spawn_mock(MockState {
        hits: hits.clone(),
        responder: Arc::new(|_| MockResponse::JsonAfter(ok_au_body(), Duration::from_millis(200))),
    })
    .await;

    let mut cfg = test_config(&url);
    cfg.queue_capacity = 256; // ensure capacity for our 100 calls
    let disp = ShadowDispatcher::spawn(cfg, None);

    // Time each fire-and-forget call individually. The dispatcher's
    // `shadow_*` methods do at most: rand() + small clone + try_send.
    // 1 ms is generous; 5 ms catches a regression while leaving
    // headroom for CI scheduler jitter.
    let mut max_per_call = Duration::ZERO;
    for _ in 0..100 {
        let t0 = Instant::now();
        fire_one_reverse(&disp);
        let elapsed = t0.elapsed();
        if elapsed > max_per_call {
            max_per_call = elapsed;
        }
    }
    assert!(
        max_per_call < Duration::from_millis(5),
        "max per-call latency {max_per_call:?} — handler is awaiting Google"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn happy_path_dispatches_to_google() {
    let hits = Arc::new(AtomicU64::new(0));
    let url = spawn_mock(MockState {
        hits: hits.clone(),
        responder: Arc::new(|_| MockResponse::Json(ok_au_body())),
    })
    .await;
    let disp = ShadowDispatcher::spawn(test_config(&url), None);

    fire_one_reverse(&disp);

    // Worker is async; give it time to send + receive + dispatch.
    // Polling rather than fixed sleep keeps the test fast in the
    // common case while being tolerant on slow CI runners.
    for _ in 0..40 {
        if hits.load(Ordering::Relaxed) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "expected exactly one Google call for one sampled job"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daily_cap_blocks_after_limit() {
    let hits = Arc::new(AtomicU64::new(0));
    let url = spawn_mock(MockState {
        hits: hits.clone(),
        responder: Arc::new(|_| MockResponse::Json(ok_au_body())),
    })
    .await;
    let mut cfg = test_config(&url);
    cfg.daily_cap = 3; // tiny, easy to overshoot in a test
    cfg.queue_capacity = 32;
    let disp = ShadowDispatcher::spawn(cfg, None);

    // Fire 10 — only the first 3 should reach Google; the rest get
    // outcome=daily_cap_hit emitted instead.
    for _ in 0..10 {
        fire_one_reverse(&disp);
    }
    // Wait for the worker to process all 10 channel items.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let observed = hits.load(Ordering::Relaxed);
    assert_eq!(
        observed, 3,
        "daily_cap=3 must hard-cap dispatch at 3 — Google was hit {observed} times"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_denied_disables_dispatcher_for_lifetime() {
    // Mock returns REQUEST_DENIED on the first call, OK on subsequent
    // (would-be) calls. We assert the worker never makes a second HTTP
    // request — the auth_disabled flag short-circuits all subsequent
    // dispatches.
    let hits = Arc::new(AtomicU64::new(0));
    let url = spawn_mock(MockState {
        hits: hits.clone(),
        responder: Arc::new(|n| {
            if n == 0 {
                MockResponse::Json(json!({ "status": "REQUEST_DENIED", "results": [] }))
            } else {
                MockResponse::Json(ok_au_body())
            }
        }),
    })
    .await;
    let disp = ShadowDispatcher::spawn(test_config(&url), None);

    // Fire a first job — gets REQUEST_DENIED, flips auth_disabled.
    fire_one_reverse(&disp);
    // Wait until the first hit lands so the flip is observable.
    for _ in 0..40 {
        if hits.load(Ordering::Relaxed) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(hits.load(Ordering::Relaxed), 1);

    // Fire 50 more. None should reach the mock.
    for _ in 0..50 {
        fire_one_reverse(&disp);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "post-REQUEST_DENIED dispatcher must make zero further HTTP calls"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_query_limit_triggers_backoff_window() {
    // Mock returns OVER_QUERY_LIMIT on the first call, then OK. The
    // dispatcher should set backoff_until ~30 s out and refuse to
    // dispatch further jobs in that window. We use a 1 s backoff for
    // the test (configurable via cfg.backoff_secs) so we can verify
    // both halves: jobs during the window are dropped, jobs after the
    // window go through.
    let hits = Arc::new(AtomicU64::new(0));
    let url = spawn_mock(MockState {
        hits: hits.clone(),
        responder: Arc::new(|n| {
            if n == 0 {
                MockResponse::Json(json!({ "status": "OVER_QUERY_LIMIT", "results": [] }))
            } else {
                MockResponse::Json(ok_au_body())
            }
        }),
    })
    .await;
    let mut cfg = test_config(&url);
    cfg.backoff_secs = 1; // 1 s so the test runs quickly
    let disp = ShadowDispatcher::spawn(cfg, None);

    // First job trips OVER_QUERY_LIMIT.
    fire_one_reverse(&disp);
    for _ in 0..40 {
        if hits.load(Ordering::Relaxed) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(hits.load(Ordering::Relaxed), 1);

    // Burst during backoff — all dropped, no HTTP calls reach mock.
    for _ in 0..10 {
        fire_one_reverse(&disp);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "during 1 s backoff, no further HTTP calls — got {}",
        hits.load(Ordering::Relaxed)
    );

    // Wait the backoff window out, then fire one more job — should
    // reach the mock.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    fire_one_reverse(&disp);
    for _ in 0..40 {
        if hits.load(Ordering::Relaxed) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        hits.load(Ordering::Relaxed),
        2,
        "after backoff expires, dispatch resumes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_full_drops_without_panic() {
    // Pin the worker so the channel fills: serve responses with a long
    // delay (5 s) so each job takes a permit and holds it. We use a
    // tiny channel + tiny inflight cap so the channel fills almost
    // immediately. Then fire many more shadow_* calls — none should
    // panic; queue_full_count must climb.
    let hits = Arc::new(AtomicU64::new(0));
    let url = spawn_mock(MockState {
        hits: hits.clone(),
        responder: Arc::new(|_| MockResponse::JsonAfter(ok_au_body(), Duration::from_secs(5))),
    })
    .await;
    let mut cfg = test_config(&url);
    cfg.queue_capacity = 4;
    cfg.inflight_cap = 1;
    cfg.rps_cap = 2;
    let disp = ShadowDispatcher::spawn(cfg, None);

    // Pump well past channel capacity. The first few enter the
    // channel; the rest hit Full.
    for _ in 0..200 {
        fire_one_reverse(&disp);
    }
    // Give the worker one tick to dispatch + the rest of the calls
    // to attempt queueing.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Many of those 200 were dropped on the floor as queue_full.
    // We don't assert the exact count (depends on scheduler timing
    // — the worker could have drained 1–4 in that 50 ms). What we
    // check is that we didn't panic and that a meaningful chunk
    // hit the queue_full path.
    // (The dispatcher's queue_full_count getter is test-only.)
    let drops = disp.queue_full_count();
    assert!(
        drops > 0,
        "expected some queue_full drops with cap=4 + 200 sends, got {drops}"
    );
}
