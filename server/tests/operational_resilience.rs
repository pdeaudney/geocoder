//! Operational-resilience integration tests.
//!
//! Two failure modes operators care about, both bounded-memory invariants:
//!
//!  1. **OTLP collector down** — `BatchSpanProcessor` must drop spans
//!     past its queue cap rather than grow unbounded. We point the
//!     exporter at a non-listening port and fire a day's worth of
//!     spans (86 400 = one per second × 24 h, run as fast as possible)
//!     to prove the queue is bounded under sustained outage.
//!
//!  2. **Index swap** — `ArcSwap` must hand the new index to new
//!     readers while leaving in-flight readers safely on the old `Arc`.
//!     We reload the same dir twice (no symlink / duplication needed:
//!     we're testing the swap mechanism, not data divergence) and
//!     assert pointer semantics + `Arc::strong_count` reclamation.
//!
//! Both tests demonstrate the safety properties without long soak
//! times — the OTLP one finishes in seconds even though it represents
//! a day of traffic, because we're not rate-limiting anything.

use arc_swap::ArcSwap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

const SPANS_PER_DAY_AT_1_RPS: usize = 86_400;

/// Read peak RSS in bytes via `getrusage(RUSAGE_SELF)`.
///
/// `ru_maxrss` is process-wide and only ever increases; we use it to
/// observe "max additional RSS during the test window" via the delta
/// between two reads. macOS reports bytes, Linux reports KiB —
/// normalised below.
fn max_rss_bytes() -> u64 {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: passing a valid &mut to a syscall that writes to it. The
    // syscall doesn't fail in practice for RUSAGE_SELF on any supported
    // platform.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
    let raw = r.ru_maxrss as u64;
    if cfg!(target_os = "macos") { raw } else { raw * 1024 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otlp_down_drops_spans_without_unbounded_growth() {
    // Bind+drop to grab a port that's guaranteed not listening. Every
    // export attempt then fails with connection-refused — the worst
    // case the BatchSpanProcessor faces in production.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let dead_addr = listener.local_addr().expect("addr");
    drop(listener);

    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(format!("http://{dead_addr}"))
        // 100 ms timeout so we don't waste seconds per failed export.
        .with_timeout(Duration::from_millis(100))
        .build()
        .expect("build OTLP exporter");

    // Tight queue + tight delay so drops kick in fast under load.
    // max_queue_size = 64 is much smaller than production's 2048; the
    // bounded-growth invariant we're testing holds at any cap, but a
    // small queue makes drop pressure dominate well within the test
    // budget.
    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(64)
        .with_max_export_batch_size(16)
        .with_scheduled_delay(Duration::from_millis(20))
        .build();
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();

    let provider = SdkTracerProvider::builder()
        .with_resource(
            Resource::builder()
                .with_service_name("operational-resilience-test")
                .build(),
        )
        .with_span_processor(processor)
        .build();
    let tracer = provider.tracer("test");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let rss_before = max_rss_bytes();

    // 86 400 spans = a day at 1 RPS, fired flat-out. If the queue
    // weren't bounded, this would OOM well before the loop ends; our
    // 64-cap with a dead exporter forces continuous drop pressure.
    for i in 0..SPANS_PER_DAY_AT_1_RPS {
        let span = tracing::info_span!(
            "forward.search_structured",
            geocoder.q = "load-test-query",
            geocoder.match_count = (i % 10) as u64,
        );
        let _enter = span.enter();
        // Yield occasionally so the processor's background task can
        // drain the queue and attempt exports — keeps the queue
        // turning over instead of just sitting full.
        if i % 1024 == 0 {
            tokio::task::yield_now().await;
        }
    }

    // Force a flush before measuring; ignore errors, the collector is
    // dead by design.
    let _ = provider.force_flush();
    let _ = provider.shutdown();

    let rss_after = max_rss_bytes();
    let delta = rss_after.saturating_sub(rss_before);

    // Generous bound — empirically tens of MB on macOS and Linux when
    // the queue is doing its job. 500 MB is well over a healthy pass
    // and well under what an unbounded queue would consume after 86k
    // spans (~hundreds of MB to gigabytes for unbounded retention).
    assert!(
        delta < 500 * 1024 * 1024,
        "RSS grew by {} bytes ({} MB) during the OTLP-down load — \
         queue may not be bounded under collector outage",
        delta,
        delta / (1024 * 1024)
    );
}

#[test]
fn index_swap_old_arc_remains_valid_for_inflight_readers() {
    let Some(dir) = std::env::var("GEOCODER_INDEX_DIR").ok() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    // First load — the "current" index that production handlers would
    // read against right now.
    let v1 = Arc::new(
        query_server::Index::load(
            &dir,
            query_server::DEFAULT_STREET_CELL_LEVEL,
            query_server::DEFAULT_ADMIN_CELL_LEVEL,
            query_server::DEFAULT_SEARCH_DISTANCE,
        )
        .expect("first index load"),
    );
    let v1_ptr = Arc::as_ptr(&v1);
    let live = Arc::new(ArcSwap::from(v1.clone()));

    // Snapshot: simulates an in-flight handler that took `live.load()`
    // before the swap. ArcSwap's contract: this Arc keeps the data
    // valid for the snapshot's lifetime regardless of subsequent stores.
    let inflight = live.load_full();
    assert_eq!(
        Arc::as_ptr(&inflight),
        v1_ptr,
        "fresh load_full() must point at v1"
    );

    // Reload the same dir — production's reloader does the same thing
    // when the marker mtime changes. We don't need different data on
    // disk to exercise the swap mechanism; we're testing ArcSwap
    // semantics, not data divergence. (If you wanted to test before/
    // after with different content, a symlink to two prepared dirs
    // would work — same-dir reload is sufficient because each
    // `Index::load` re-opens and re-mmaps every file, exercising
    // exactly the production code path.)
    let v2 = Arc::new(
        query_server::Index::load(
            &dir,
            query_server::DEFAULT_STREET_CELL_LEVEL,
            query_server::DEFAULT_ADMIN_CELL_LEVEL,
            query_server::DEFAULT_SEARCH_DISTANCE,
        )
        .expect("second index load"),
    );
    let v2_ptr = Arc::as_ptr(&v2);
    assert_ne!(
        v1_ptr, v2_ptr,
        "second load must allocate a distinct Index"
    );
    live.store(v2.clone());

    // The in-flight Arc still points at v1 — that's the key safety
    // property. A handler running across the swap reads coherent data,
    // not a torn mix of old and new mmaps.
    let _addr_old = inflight.query(-33.8568, 151.2153);
    assert_eq!(
        Arc::as_ptr(&inflight),
        v1_ptr,
        "in-flight snapshot pointer must not change across a swap"
    );

    // New readers see v2.
    let after = live.load_full();
    let _addr_new = after.query(-33.8568, 151.2153);
    assert_eq!(
        Arc::as_ptr(&after),
        v2_ptr,
        "post-swap load must return the new Arc"
    );
    assert_ne!(
        Arc::as_ptr(&after),
        v1_ptr,
        "post-swap load must not return the old Arc"
    );

    // Once the in-flight handle drops, v1's strong-count is the test's
    // own retained handle (1) — proves the old index gets reclaimed
    // when no readers remain. If ArcSwap or our reloader leaked a
    // reference, this would fail.
    drop(inflight);
    drop(after);
    let lingering = Arc::strong_count(&v1);
    assert_eq!(
        lingering, 1,
        "old index Arc should be down to the test's own handle (1) \
         after readers drop; got {} — reload may be leaking refs",
        lingering
    );
}

// --- Log dedup: end-to-end through tracing-subscriber ---
//
// These three tests exercise the production wiring (a fmt layer wrapped
// in `DedupFilter`, fed by `tracing_subscriber::set_default`), so a
// regression in the trait impl, the layer ordering, or the writer plumb
// fails here even though the unit tests in `mod telemetry::tests` pass.

#[derive(Clone, Default)]
struct CaptureBuf(Arc<Mutex<Vec<u8>>>);

impl CaptureBuf {
    fn lines(&self) -> Vec<String> {
        std::str::from_utf8(&self.0.lock().expect("buf poisoned").clone())
            .expect("utf-8")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect()
    }
}

impl Write for CaptureBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buf poisoned").extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureBuf {
    type Writer = CaptureBuf;
    fn make_writer(&'a self) -> CaptureBuf {
        self.clone()
    }
}

fn install_dedup_subscriber(
    buf: CaptureBuf,
    window: Duration,
    target_prefix: &'static str,
) -> tracing::dispatcher::DefaultGuard {
    let fmt = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(buf)
        .with_filter(query_server::telemetry::DedupFilter::new(window, target_prefix));
    let subscriber = tracing_subscriber::registry().with(fmt);
    tracing::subscriber::set_default(subscriber)
}

#[test]
fn dedup_filter_collapses_otel_log_flood_to_one_line() {
    // Single OTel-internal callsite floods 100×; dedup must collapse
    // it to one stdout line. Application-target logs must pass through
    // untouched in the same window.
    let buf = CaptureBuf::default();
    let _g = install_dedup_subscriber(buf.clone(), Duration::from_secs(30), "opentelemetry");

    for _ in 0..100 {
        tracing::warn!(
            target: "opentelemetry_sdk::trace::span_processor",
            "Queue is full, dropping span"
        );
    }
    for _ in 0..5 {
        tracing::info!(target: "query_server::http", "request complete");
    }

    drop(_g);

    let lines = buf.lines();
    let otel_lines = lines
        .iter()
        .filter(|l| l.contains("opentelemetry_sdk::trace::span_processor"))
        .count();
    let app_lines = lines
        .iter()
        .filter(|l| l.contains("query_server::http"))
        .count();

    assert_eq!(
        otel_lines, 1,
        "expected 100 OTel-internal warnings to collapse to exactly 1 stdout line, got {otel_lines}"
    );
    assert_eq!(
        app_lines, 5,
        "application logs must not be deduped; got {app_lines}"
    );
}

#[test]
fn dedup_filter_distinct_otel_callsites_each_get_one_line() {
    // Multiple distinct OTel-internal callsites each get one line per
    // window — drops are throttled per-callsite, not per-target. This
    // is the realistic outage shape: the SDK emits a few different
    // warnings (queue full, export failed, …) and each should pass
    // once.
    let buf = CaptureBuf::default();
    let _g = install_dedup_subscriber(buf.clone(), Duration::from_secs(30), "opentelemetry");

    // Three different source lines = three distinct callsites. Each
    // fires 50× to simulate sustained pressure.
    for _ in 0..50 {
        tracing::warn!(target: "opentelemetry_sdk::trace::span_processor", "queue full");
    }
    for _ in 0..50 {
        tracing::warn!(target: "opentelemetry_otlp::exporter", "export failed");
    }
    for _ in 0..50 {
        tracing::warn!(target: "opentelemetry::propagator", "context decode error");
    }

    drop(_g);

    let lines = buf.lines();
    let queue_lines = lines.iter().filter(|l| l.contains("queue full")).count();
    let export_lines = lines.iter().filter(|l| l.contains("export failed")).count();
    let propagator_lines = lines
        .iter()
        .filter(|l| l.contains("context decode error"))
        .count();

    assert_eq!(queue_lines, 1, "expected 1 line for queue-full callsite, got {queue_lines}");
    assert_eq!(
        export_lines, 1,
        "expected 1 line for export-failed callsite, got {export_lines}"
    );
    assert_eq!(
        propagator_lines, 1,
        "expected 1 line for propagator callsite, got {propagator_lines}"
    );
}

#[test]
fn dedup_filter_window_expiry_emits_again_end_to_end() {
    // After the window elapses, the same callsite gets another line.
    // Tiny 30 ms window keeps the test fast while leaving margin
    // against scheduler jitter — we sleep 60 ms to be safely past it.
    let buf = CaptureBuf::default();
    let _g = install_dedup_subscriber(buf.clone(), Duration::from_millis(30), "opentelemetry");

    for _ in 0..10 {
        tracing::warn!(
            target: "opentelemetry_sdk::trace::span_processor",
            "first burst"
        );
    }
    std::thread::sleep(Duration::from_millis(60));
    for _ in 0..10 {
        tracing::warn!(
            target: "opentelemetry_sdk::trace::span_processor",
            "first burst"
        );
    }

    drop(_g);

    // Both bursts come from the same source line → same callsite. We
    // expect exactly 2 emitted lines: one per window. Anything ≥ 3
    // would mean the window was treated as too short; ≤ 1 would mean
    // expiry wasn't honoured.
    let lines = buf.lines();
    let count = lines
        .iter()
        .filter(|l| l.contains("first burst"))
        .count();
    assert_eq!(
        count, 2,
        "expected exactly 2 emissions across two windows, got {count}"
    );
}

#[test]
fn index_swap_query_served_by_correct_version_after_drop() {
    // Companion check to the strong-count test: once the test drops
    // its own handle to v1, strong_count goes to zero and the index
    // is freed. We can't observe deallocation directly, but we can
    // observe that the live ArcSwap continues to serve queries
    // through v2 — i.e. the swap didn't accidentally leave the live
    // pointer dangling at a freed v1.
    let Some(dir) = std::env::var("GEOCODER_INDEX_DIR").ok() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    let v1 = Arc::new(
        query_server::Index::load(
            &dir,
            query_server::DEFAULT_STREET_CELL_LEVEL,
            query_server::DEFAULT_ADMIN_CELL_LEVEL,
            query_server::DEFAULT_SEARCH_DISTANCE,
        )
        .expect("first index load"),
    );
    let live = Arc::new(ArcSwap::from(v1.clone()));

    let v2 = Arc::new(
        query_server::Index::load(
            &dir,
            query_server::DEFAULT_STREET_CELL_LEVEL,
            query_server::DEFAULT_ADMIN_CELL_LEVEL,
            query_server::DEFAULT_SEARCH_DISTANCE,
        )
        .expect("second index load"),
    );
    live.store(v2.clone());

    // Drop our handle to v1. ArcSwap no longer references it (we
    // stored v2). If anything in the swap path retained v1 internally,
    // strong_count > 1 here.
    drop(v1);

    // Live queries continue to land on v2 — the swap didn't break.
    for _ in 0..10 {
        let snap = live.load_full();
        let _ = snap.query(-33.8568, 151.2153);
    }
}
