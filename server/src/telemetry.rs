//! Telemetry: structured stdout logging + optional OTLP span export.
//!
//! Operators configure the pipeline through environment variables; the
//! defaults are picked so a fresh container shipped to a log aggregator
//! produces useful output without touching code.
//!
//! ## Variables
//!
//! | Variable                       | Default                       | Effect                                                 |
//! |--------------------------------|-------------------------------|--------------------------------------------------------|
//! | `OTEL_TRACE_ENABLED`           | `true` if endpoint set        | Master gate for OTLP export. `false` disables even if  |
//! |                                |                               | the endpoint is set, `true` enables and uses           |
//! |                                |                               | `http://localhost:4317` if no endpoint was given.      |
//! | `OTEL_EXPORTER_OTLP_ENDPOINT`  | `http://localhost:4317`       | OTLP collector endpoint (e.g. an OTel collector,       |
//! |                                |                               | Tempo, Honeycomb gRPC, Datadog Agent, …).              |
//! | `OTEL_EXPORTER_OTLP_PROTOCOL`  | `grpc`                        | `grpc` or `http/protobuf`. Picks the OTLP transport.   |
//! | `OTEL_SERVICE_NAME`            | `query-server`                | service.name resource attribute.                       |
//! | `OTEL_RESOURCE_ATTRIBUTES`     | unset                         | Comma-separated `key=value` pairs merged into the      |
//! |                                |                               | resource (e.g. `deployment.environment=prod,...`).     |
//! | `GEOCODER_LOG_FORMAT`          | `json` if not a tty, else     | `json` / `pretty` / `compact`. Overrides the autopick. |
//! |                                | `compact`                     |                                                        |
//! | `RUST_LOG` / `OTEL_LOG_LEVEL`  | `info,query_server=info`      | EnvFilter directive controlling event + span levels.   |
//!
//! ## Production defaults
//!
//! - JSON line per log event on stdout, parseable by Loki / CloudWatch /
//!   Stackdriver / Datadog without a parser config.
//! - Spans participate in `current_span` and `spans` JSON fields so a
//!   request-scoped trace ID is visible alongside every log line, even
//!   without the OTLP exporter.
//! - When `OTEL_TRACE_ENABLED=false` is set explicitly, no OTLP traffic is
//!   ever produced — useful for ops who want stdout logs but don't have
//!   a collector deployed yet.

use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing::callsite::Identifier;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::{Context, Filter, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Default OTLP endpoint used when tracing is enabled without an explicit
/// `OTEL_EXPORTER_OTLP_ENDPOINT`. Matches the OpenTelemetry Collector's
/// default gRPC port (4317).
const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4317";

/// Service name that lands in the `service.name` resource attribute when
/// the operator hasn't set `OTEL_SERVICE_NAME`. Visible in Tempo/Jaeger
/// service lists, Datadog APM, Honeycomb dataset list etc.
const DEFAULT_SERVICE_NAME: &str = "query-server";

/// Default EnvFilter directive when the operator hasn't set `RUST_LOG`.
/// `info` for everything plus an explicit `query_server=info` so the
/// crate's own events can't be dropped by an aggressive root level.
const DEFAULT_FILTER: &str = "info,query_server=info,tower_http=info";

/// Default dedup window in seconds. Applied per-callsite to log events
/// emitted by OpenTelemetry-internal targets so a sustained collector
/// outage doesn't flood stdout. Set `GEOCODER_LOG_DEDUP_WINDOW_SEC=0`
/// to disable.
const DEFAULT_DEDUP_WINDOW_SECS: u64 = 30;

/// Target prefix the dedup filter scopes to. OTel-internal events emit
/// under `opentelemetry`, `opentelemetry_sdk`, and `opentelemetry_otlp`
/// — all caught by this prefix. Application logs (`query_server::*`)
/// are deliberately not deduped; we don't want to silence intentional
/// emissions from the handler code.
const DEDUP_TARGET_PREFIX: &str = "opentelemetry";

/// Held by `main()` for the lifetime of the process. `Drop` flushes the
/// batch span processor so spans buffered at shutdown still reach the
/// collector. Without this, sigterm during a graceful shutdown can lose
/// the last second or two of trace data.
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // shutdown() is sync and blocks up to the configured export
            // timeout; the BatchConfig below caps that at 5s so a misconfigured
            // collector can't hang process exit indefinitely.
            if let Err(err) = provider.shutdown() {
                eprintln!("telemetry: tracer provider shutdown error: {err}");
            }
        }
    }
}

/// Initialise stdout logging plus (optionally) OTLP export.
///
/// Call from `main()` before spawning any tasks that emit tracing events.
/// Returns a guard whose `Drop` flushes pending spans on shutdown.
pub fn init() -> TelemetryGuard {
    let filter = build_env_filter();

    let fmt_layer = build_fmt_layer();

    let (otlp_layer, provider) = match build_otlp_layer() {
        Ok(Some((layer, provider))) => (Some(layer), Some(provider)),
        Ok(None) => (None, None),
        Err(err) => {
            // Don't fail startup on a misconfigured collector — ship logs to
            // stdout, surface the misconfig once, keep serving.
            eprintln!("telemetry: OTLP exporter init failed, falling back to stdout-only: {err}");
            (None, None)
        }
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otlp_layer)
        .init();

    if let Some(p) = provider.as_ref() {
        tracing::info!(
            target: "query_server::telemetry",
            otlp_endpoint = %resolved_endpoint(),
            otlp_protocol = %resolved_protocol_name(),
            service_name = %resolved_service_name(),
            "OTLP tracing enabled"
        );
        let _ = p; // silence unused when tracing is off
    } else {
        tracing::info!(
            target: "query_server::telemetry",
            "OTLP tracing disabled — set OTEL_EXPORTER_OTLP_ENDPOINT and OTEL_TRACE_ENABLED=true to enable"
        );
    }

    TelemetryGuard { provider }
}

fn build_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Build the stdout formatting layer. Picks JSON / pretty / compact based
/// on `GEOCODER_LOG_FORMAT` first, then a tty heuristic. Span open/close
/// events are emitted at NEW + CLOSE so request-scoped timing shows up
/// in stdout logs (queryable in Loki / CloudWatch with `level=INFO`).
fn build_fmt_layer<S>() -> Box<dyn Layer<S> + Send + Sync + 'static>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let format = std::env::var("GEOCODER_LOG_FORMAT")
        .ok()
        .map(|s| s.to_ascii_lowercase());
    let format = format.as_deref().unwrap_or_else(|| {
        // stdout-is-a-tty → pretty for humans, otherwise JSON for log
        // aggregators. is_terminal() avoids the libc-version landmines of
        // older isatty crates.
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            "pretty"
        } else {
            "json"
        }
    });

    let base = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_line_number(false)
        .with_file(false)
        .with_span_events(FmtSpan::CLOSE);

    let formatted: Box<dyn Layer<S> + Send + Sync + 'static> = match format {
        "json" => base.json().with_current_span(true).with_span_list(true).boxed(),
        "pretty" => base.pretty().boxed(),
        "compact" | _ => base.compact().boxed(),
    };

    // Wrap the formatted layer in a per-callsite dedup filter scoped to
    // OTel-internal targets so an outage doesn't flood stdout with one
    // "queue full" line per dropped batch. Application logs are
    // unaffected — the filter passes through anything outside the
    // configured target prefix.
    let window = resolve_dedup_window(std::env::var("GEOCODER_LOG_DEDUP_WINDOW_SEC").ok().as_deref());
    if window.is_zero() {
        return formatted;
    }
    Box::new(formatted.with_filter(DedupFilter::new(window, DEDUP_TARGET_PREFIX)))
}

/// Parse the dedup-window env var. `0` disables. Garbage falls back to
/// the default (30 s) — better to keep operators safe than fail open.
pub(crate) fn resolve_dedup_window(raw: Option<&str>) -> Duration {
    let secs = match raw {
        None => DEFAULT_DEDUP_WINDOW_SECS,
        Some(v) => match v.trim().parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                eprintln!(
                    "telemetry: unrecognised GEOCODER_LOG_DEDUP_WINDOW_SEC={v:?}; using default {DEFAULT_DEDUP_WINDOW_SECS}s"
                );
                DEFAULT_DEDUP_WINDOW_SECS
            }
        },
    };
    Duration::from_secs(secs)
}

/// Per-callsite throttle applied to log events from a target subtree.
/// First event from each callsite passes; further events from the same
/// callsite are dropped until `window` has elapsed since the last
/// emission. Implementation is `Mutex<HashMap>` rather than a
/// concurrent map because (a) the number of distinct OTel-internal
/// callsites is tiny (single digits) and (b) the lock is held only
/// long enough to do a hash lookup and insert, so contention is
/// effectively zero even at high event rates.
pub struct DedupFilter {
    state: Mutex<HashMap<Identifier, Instant>>,
    window: Duration,
    target_prefix: &'static str,
}

impl DedupFilter {
    pub fn new(window: Duration, target_prefix: &'static str) -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            window,
            target_prefix,
        }
    }

    /// Returns `true` if the event should pass through to the inner
    /// layer, `false` if it's a same-callsite repeat inside the window.
    fn should_emit(&self, meta: &Metadata<'_>) -> bool {
        if !meta.target().starts_with(self.target_prefix) {
            return true;
        }
        let id = meta.callsite();
        let now = Instant::now();
        let mut state = self.state.lock().expect("dedup state mutex poisoned");
        match state.get(&id) {
            Some(&last) if now.duration_since(last) < self.window => false,
            _ => {
                state.insert(id, now);
                true
            }
        }
    }
}

impl<S> Filter<S> for DedupFilter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(&self, _meta: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        // Spans + events that aren't from a deduped target pass freely;
        // event_enabled() makes the per-callsite decision.
        true
    }

    fn event_enabled(&self, event: &Event<'_>, _: &Context<'_, S>) -> bool {
        self.should_emit(event.metadata())
    }
}

/// If the operator has opted in (or implicitly opted in by setting an
/// endpoint), build the OTLP exporter and wrap it in a tracing layer.
///
/// Returns `Ok(None)` when tracing is disabled — that's a normal startup
/// path, not an error. Returns `Err` only when the operator asked for
/// tracing and we couldn't deliver it.
fn build_otlp_layer<S>() -> Result<Option<(Box<dyn Layer<S> + Send + Sync + 'static>, SdkTracerProvider)>, String>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    if !tracing_enabled() {
        return Ok(None);
    }

    let endpoint = resolved_endpoint();
    let protocol = resolved_protocol();

    // Build the span exporter. Both grpc-tonic and http-proto transports
    // come from the same builder; with_<transport>() picks the wire format.
    let exporter = match protocol {
        Protocol::Grpc => SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&endpoint)
            .with_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("build grpc exporter: {e}"))?,
        Protocol::HttpBinary | Protocol::HttpJson => SpanExporter::builder()
            .with_http()
            .with_endpoint(&endpoint)
            .with_protocol(protocol)
            .with_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("build http exporter: {e}"))?,
    };

    // Cap the queue and scheduled delay so a stalled collector can't
    // build unbounded backpressure into the request path. Per-export
    // timeout is enforced by the exporter itself (with_timeout above);
    // exposing it here would require the experimental async-runtime
    // batch processor feature flag, which is not stable in 0.31.
    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(2048)
        .with_max_export_batch_size(512)
        .with_scheduled_delay(Duration::from_secs(2))
        .build();

    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();

    let provider = SdkTracerProvider::builder()
        .with_resource(build_resource())
        .with_span_processor(processor)
        .build();

    let tracer = provider.tracer(env!("CARGO_PKG_NAME"));

    // Make this provider the global default so any library that creates
    // spans through opentelemetry::global picks it up.
    opentelemetry::global::set_tracer_provider(provider.clone());
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let layer = tracing_opentelemetry::layer().with_tracer(tracer).boxed();
    Ok(Some((layer, provider)))
}

/// Resolve the OTLP master switch.
///
/// - `OTEL_TRACE_ENABLED=true|1|yes|on`  → enabled
/// - `OTEL_TRACE_ENABLED=false|0|no|off` → disabled (even if endpoint set)
/// - unset                               → enabled iff endpoint is set
fn tracing_enabled() -> bool {
    resolve_tracing_enabled(
        std::env::var("OTEL_TRACE_ENABLED").ok().as_deref(),
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok(),
    )
}

/// Pure form of [`tracing_enabled`], decoupled from env-var reads so we
/// can exhaustively test the truth matrix without process-global state.
pub(crate) fn resolve_tracing_enabled(flag: Option<&str>, endpoint_set: bool) -> bool {
    match flag {
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" | "" => false,
            other => {
                eprintln!(
                    "telemetry: unrecognised OTEL_TRACE_ENABLED={other:?}; treating as disabled"
                );
                false
            }
        },
        None => endpoint_set,
    }
}

fn resolved_endpoint() -> String {
    std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"))
        .unwrap_or_else(|_| DEFAULT_OTLP_ENDPOINT.to_string())
}

fn resolved_protocol() -> Protocol {
    let raw = std::env::var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL"))
        .ok();
    parse_protocol(raw.as_deref())
}

/// Pure form of [`resolved_protocol`]. `None` defaults to gRPC; an
/// unrecognised value also falls back to gRPC after a warning. Test
/// coverage for the parser lives next to the function.
pub(crate) fn parse_protocol(raw: Option<&str>) -> Protocol {
    let Some(raw) = raw else { return Protocol::Grpc };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "grpc" => Protocol::Grpc,
        "http/protobuf" | "http-protobuf" | "http/proto" | "http-proto" => Protocol::HttpBinary,
        "http/json" | "http-json" => Protocol::HttpJson,
        other => {
            eprintln!(
                "telemetry: unrecognised OTEL_EXPORTER_OTLP_PROTOCOL={other:?}; defaulting to grpc"
            );
            Protocol::Grpc
        }
    }
}

fn resolved_protocol_name() -> &'static str {
    protocol_name(resolved_protocol())
}

pub(crate) fn protocol_name(p: Protocol) -> &'static str {
    match p {
        Protocol::Grpc => "grpc",
        Protocol::HttpBinary => "http/protobuf",
        Protocol::HttpJson => "http/json",
    }
}

fn resolved_service_name() -> String {
    std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| DEFAULT_SERVICE_NAME.to_string())
}

/// Build the OTel resource describing this process. service.name and
/// service.version are populated unconditionally; everything else comes
/// from `OTEL_RESOURCE_ATTRIBUTES` (the OpenTelemetry standard env var)
/// so deployment.environment, k8s.pod.name, host.name etc. can be set
/// without code changes.
fn build_resource() -> Resource {
    let service_name = resolved_service_name();
    let mut builder = Resource::builder()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")));

    if let Ok(extra) = std::env::var("OTEL_RESOURCE_ATTRIBUTES") {
        for (k, v) in parse_resource_attributes(&extra) {
            builder = builder.with_attribute(KeyValue::new(k, v));
        }
    }

    builder.build()
}

/// Parse `OTEL_RESOURCE_ATTRIBUTES` per the OTel spec: comma-separated
/// `key=value` pairs. Pairs without `=`, empty pairs, and pairs with empty
/// keys are dropped silently — same behaviour the spec asks for. Whitespace
/// around the key and value is trimmed.
pub(crate) fn parse_resource_attributes(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        out.push((key.to_string(), v.trim().to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- OTEL_TRACE_ENABLED truth matrix ---

    #[test]
    fn trace_enabled_true_variants_enable() {
        for v in ["true", "1", "yes", "on", "TRUE", " On "] {
            assert!(
                resolve_tracing_enabled(Some(v), false),
                "expected {v:?} to enable tracing"
            );
        }
    }

    #[test]
    fn trace_enabled_false_variants_disable_even_with_endpoint() {
        for v in ["false", "0", "no", "off", "FALSE", " Off "] {
            assert!(
                !resolve_tracing_enabled(Some(v), true),
                "expected {v:?} to disable tracing even when endpoint is set"
            );
        }
    }

    #[test]
    fn trace_enabled_empty_string_disables() {
        // Empty string is treated as "explicit off" rather than "unset" —
        // operators who write `OTEL_TRACE_ENABLED=` in a templated env
        // file are signalling intent, not a missing variable.
        assert!(!resolve_tracing_enabled(Some(""), true));
    }

    #[test]
    fn trace_enabled_garbage_disables() {
        // Unknown values trip the eprintln warning and fall through to
        // disabled, matching "fail safe" — better to ship logs without
        // OTLP than to surprise an operator with phantom traffic.
        assert!(!resolve_tracing_enabled(Some("maybe"), true));
        assert!(!resolve_tracing_enabled(Some("enabled-please"), true));
    }

    #[test]
    fn trace_enabled_unset_follows_endpoint_presence() {
        assert!(resolve_tracing_enabled(None, true));
        assert!(!resolve_tracing_enabled(None, false));
    }

    // --- OTEL_EXPORTER_OTLP_PROTOCOL parsing ---

    #[test]
    fn protocol_grpc_default_when_unset_or_empty() {
        assert!(matches!(parse_protocol(None), Protocol::Grpc));
        assert!(matches!(parse_protocol(Some("")), Protocol::Grpc));
        assert!(matches!(parse_protocol(Some("grpc")), Protocol::Grpc));
        assert!(matches!(parse_protocol(Some(" GRPC ")), Protocol::Grpc));
    }

    #[test]
    fn protocol_http_protobuf_aliases() {
        for v in [
            "http/protobuf",
            "http-protobuf",
            "http/proto",
            "http-proto",
            " Http/Protobuf ",
        ] {
            assert!(
                matches!(parse_protocol(Some(v)), Protocol::HttpBinary),
                "expected {v:?} to map to HttpBinary"
            );
        }
    }

    #[test]
    fn protocol_http_json_aliases() {
        for v in ["http/json", "http-json", "HTTP/JSON"] {
            assert!(
                matches!(parse_protocol(Some(v)), Protocol::HttpJson),
                "expected {v:?} to map to HttpJson"
            );
        }
    }

    #[test]
    fn protocol_unknown_falls_back_to_grpc() {
        // We warn and fall back rather than refusing to start; an
        // operator misconfiguring the protocol shouldn't take the
        // service offline.
        assert!(matches!(parse_protocol(Some("kafka")), Protocol::Grpc));
        assert!(matches!(parse_protocol(Some("https")), Protocol::Grpc));
    }

    #[test]
    fn protocol_name_round_trips() {
        assert_eq!(protocol_name(Protocol::Grpc), "grpc");
        assert_eq!(protocol_name(Protocol::HttpBinary), "http/protobuf");
        assert_eq!(protocol_name(Protocol::HttpJson), "http/json");
    }

    // --- OTEL_RESOURCE_ATTRIBUTES parsing ---

    #[test]
    fn resource_attrs_basic_pairs() {
        let parsed = parse_resource_attributes("foo=bar,baz=qux");
        assert_eq!(
            parsed,
            vec![
                ("foo".to_string(), "bar".to_string()),
                ("baz".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn resource_attrs_trims_whitespace_per_pair() {
        let parsed = parse_resource_attributes("  deployment.environment = prod ,  k8s.pod.name = api-7  ");
        assert_eq!(
            parsed,
            vec![
                ("deployment.environment".to_string(), "prod".to_string()),
                ("k8s.pod.name".to_string(), "api-7".to_string()),
            ]
        );
    }

    #[test]
    fn resource_attrs_drops_malformed_entries() {
        // No `=`         → dropped, can't be a KV
        // empty pair     → dropped, parser hops over trailing commas
        // empty key      → dropped, would produce a useless attribute
        // empty value    → kept; "service.namespace=" is still meaningful intent
        let parsed = parse_resource_attributes("ok=1,bad,=novalue,trailing=,, ,k=v");
        assert_eq!(
            parsed,
            vec![
                ("ok".to_string(), "1".to_string()),
                ("trailing".to_string(), "".to_string()),
                ("k".to_string(), "v".to_string()),
            ]
        );
    }

    #[test]
    fn resource_attrs_value_can_contain_equals() {
        // split_once('=') only splits on the first '=' so values like
        // base64 / URLs survive intact.
        let parsed = parse_resource_attributes("token=abc=def==");
        assert_eq!(parsed, vec![("token".to_string(), "abc=def==".to_string())]);
    }

    #[test]
    fn resource_attrs_empty_input_yields_no_pairs() {
        assert!(parse_resource_attributes("").is_empty());
        assert!(parse_resource_attributes("   ").is_empty());
        assert!(parse_resource_attributes(",,,").is_empty());
    }

    // --- GEOCODER_LOG_DEDUP_WINDOW_SEC parsing ---

    #[test]
    fn dedup_window_unset_uses_default() {
        assert_eq!(
            resolve_dedup_window(None),
            Duration::from_secs(DEFAULT_DEDUP_WINDOW_SECS)
        );
    }

    #[test]
    fn dedup_window_zero_disables() {
        assert!(resolve_dedup_window(Some("0")).is_zero());
    }

    #[test]
    fn dedup_window_explicit_value_honoured() {
        assert_eq!(resolve_dedup_window(Some("60")), Duration::from_secs(60));
        assert_eq!(resolve_dedup_window(Some("  5  ")), Duration::from_secs(5));
    }

    #[test]
    fn dedup_window_garbage_falls_back_to_default() {
        // Same fail-safe philosophy as the protocol parser — keep the
        // operator protected from log floods rather than fail open on
        // typos.
        assert_eq!(
            resolve_dedup_window(Some("forever")),
            Duration::from_secs(DEFAULT_DEDUP_WINDOW_SECS)
        );
    }

    // --- DedupFilter behaviour ---
    //
    // Each tracing macro invocation in source code has a unique
    // `Identifier` callsite. We can't construct `Metadata` synthetically
    // (the constructor is crate-private in tracing), so we capture real
    // ones via a one-shot subscriber. `capture_metadata(|| emit())`
    // returns the `&'static Metadata` for that emission; calling it from
    // two different source lines yields two different callsites, which
    // is what we exercise in the "distinct callsites" test below.

    fn capture_metadata(emit: impl FnOnce()) -> &'static Metadata<'static> {
        struct Capture(std::sync::Mutex<Option<&'static Metadata<'static>>>);
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &Metadata<'_>) -> bool { true }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &Event<'_>) {
                *self.0.lock().expect("capture mutex poisoned") = Some(event.metadata());
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let cap = std::sync::Arc::new(Capture(std::sync::Mutex::new(None)));
        tracing::subscriber::with_default(cap.clone(), emit);
        let captured = *cap
            .0
            .lock()
            .expect("capture mutex poisoned")
            .as_ref()
            .expect("metadata captured — closure must call a tracing macro");
        captured
    }

    /// Convenience: a metadata reference for the standard OTel-internal
    /// callsite used across most of these tests. Cached so all tests
    /// that don't need a *distinct* callsite share the same one (which
    /// matches production: a single noisy callsite firing repeatedly).
    fn shared_otel_meta() -> &'static Metadata<'static> {
        use std::sync::OnceLock;
        static M: OnceLock<&'static Metadata<'static>> = OnceLock::new();
        M.get_or_init(|| {
            capture_metadata(|| {
                tracing::warn!(target: "opentelemetry_sdk::test", "shared callsite");
            })
        })
    }

    #[test]
    fn dedup_filter_first_event_passes() {
        let filter = DedupFilter::new(Duration::from_secs(30), "opentelemetry");
        assert!(filter.should_emit(shared_otel_meta()));
    }

    #[test]
    fn dedup_filter_second_event_within_window_dropped() {
        let filter = DedupFilter::new(Duration::from_secs(30), "opentelemetry");
        assert!(filter.should_emit(shared_otel_meta()));
        assert!(!filter.should_emit(shared_otel_meta()));
        assert!(!filter.should_emit(shared_otel_meta()));
    }

    #[test]
    fn dedup_filter_event_after_window_passes() {
        // Use a tiny window so we can wait it out without slowing the
        // suite. The exact threshold isn't important — we just need to
        // observe "passes again after it expires."
        let filter = DedupFilter::new(Duration::from_millis(20), "opentelemetry");
        assert!(filter.should_emit(shared_otel_meta()));
        assert!(!filter.should_emit(shared_otel_meta()));
        std::thread::sleep(Duration::from_millis(40));
        assert!(filter.should_emit(shared_otel_meta()));
    }

    #[test]
    fn dedup_filter_off_target_passes_unconditionally() {
        // A DedupFilter scoped to "opentelemetry" must not affect logs
        // from other targets. We construct a synthetic filter with a
        // non-matching prefix to verify the early-return path.
        let filter = DedupFilter::new(Duration::from_secs(30), "no_match_here");
        assert!(filter.should_emit(shared_otel_meta()));
        assert!(filter.should_emit(shared_otel_meta()));
        assert!(filter.should_emit(shared_otel_meta()));
    }

    #[test]
    fn dedup_filter_distinct_callsites_each_emit_independently() {
        // Two different source lines → two different callsites. Both
        // should be allowed to emit within the same window because the
        // throttle is per-callsite, not per-target.
        let filter = DedupFilter::new(Duration::from_secs(30), "opentelemetry");
        let meta_a = capture_metadata(|| {
            tracing::warn!(target: "opentelemetry_sdk::callsite_a", "first source line");
        });
        let meta_b = capture_metadata(|| {
            tracing::warn!(target: "opentelemetry_sdk::callsite_b", "second source line");
        });

        // First emission from each callsite passes.
        assert!(filter.should_emit(meta_a));
        assert!(filter.should_emit(meta_b));
        // Second emission from each is throttled — independently.
        assert!(!filter.should_emit(meta_a));
        assert!(!filter.should_emit(meta_b));
    }

    #[test]
    fn dedup_filter_prefix_is_strict_starts_with() {
        // The prefix is a substring-from-position-0 match, not a regex
        // or glob. Targets that share a longer-than-prefix opening are
        // matched; targets shorter than the prefix never match.
        let filter = DedupFilter::new(Duration::from_secs(30), "opentelemetry");
        let in_scope = capture_metadata(|| {
            tracing::warn!(target: "opentelemetry_sdk::span_processor", "in scope");
        });
        let out_of_scope_short = capture_metadata(|| {
            tracing::warn!(target: "opentel", "shorter than prefix");
        });
        let out_of_scope_unrelated = capture_metadata(|| {
            tracing::warn!(target: "tracing::dispatcher", "unrelated");
        });

        // In-scope event respects throttle.
        assert!(filter.should_emit(in_scope));
        assert!(!filter.should_emit(in_scope));
        // Out-of-scope events bypass the throttle entirely.
        for _ in 0..5 {
            assert!(filter.should_emit(out_of_scope_short));
            assert!(filter.should_emit(out_of_scope_unrelated));
        }
    }

    #[test]
    fn dedup_filter_empty_prefix_matches_every_target() {
        // Edge case: an empty prefix means "throttle everything." Useful
        // for operators who want a global cap on their own log volume.
        let filter = DedupFilter::new(Duration::from_secs(30), "");
        let app_meta = capture_metadata(|| {
            tracing::warn!(target: "anything::at::all", "app log");
        });
        assert!(filter.should_emit(app_meta));
        assert!(!filter.should_emit(app_meta));
    }

    #[test]
    fn dedup_filter_concurrent_writers_emit_exactly_once_per_window() {
        // Sixteen threads × 1 000 attempts each = 16 000 attempts on a
        // single callsite within the dedup window. The Mutex-protected
        // state must serialise the "first emission" decision so exactly
        // one attempt across all threads sees `true`.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let filter = std::sync::Arc::new(DedupFilter::new(
            Duration::from_secs(30),
            "opentelemetry",
        ));
        let meta = shared_otel_meta();
        let passes = std::sync::Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(16);
        for _ in 0..16 {
            let f = filter.clone();
            let p = passes.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    if f.should_emit(meta) {
                        p.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }

        assert_eq!(
            passes.load(Ordering::Relaxed),
            1,
            "exactly one of 16 000 concurrent attempts must see should_emit=true within the window"
        );
    }

    // The Filter-trait wiring (`event_enabled` forwarding to
    // `should_emit`) is exercised end-to-end by
    // `dedup_filter_collapses_otel_log_flood_to_one_line` in
    // tests/operational_resilience.rs. That goes through real
    // tracing-subscriber machinery, which is more representative
    // than a hand-rolled probe here.
}
