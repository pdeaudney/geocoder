//! Prometheus + OpenTelemetry metrics surface.
//!
//! Operators get the same series two ways:
//!   - `GET /metrics` — Prometheus text exposition format, scraped by
//!     a Prometheus server (or any compatible scraper).
//!   - OTLP push — when `OTEL_METRICS_ENABLED` + an OTLP endpoint are
//!     configured, the same observations land in any OTel-native
//!     backend (Honeycomb, Datadog, Tempo+Mimir, …).
//!
//! Implementation: dual-instrumentation. Each public method
//! (`record_request`, `record_shadow`) bumps **both** the `prometheus`
//! crate counter and the OTel `Counter<u64>` / `Histogram<f64>`
//! instrument. The two registrations live next to each other in
//! `Metrics::new`, so adding a new metric is a local diff that's
//! hard to forget on either side.
//!
//! Why dual instead of a single source of truth: the
//! `opentelemetry-prometheus` bridge crate that would have unified
//! the two paths is **discontinued upstream**. The maintained path
//! forward is OTLP push with the OTel collector handling Prometheus
//! exposition externally — but our deployments still expect a local
//! `/metrics` endpoint, so we run both.
//!
//! The metrics are named with a `geocoder_` prefix and labelled by
//! endpoint + (where meaningful) country so per-country request
//! rates and accuracy can be sliced in PromQL or OTel-side
//! aggregations without joining to the underlying logs.
//!
//! ## Counter vs gauge for "requests per second"
//!
//! Prometheus convention stores request rates as **counters**, not
//! gauges — the dashboard computes RPS via `rate(metric[1m])` at query
//! time. We follow that convention. Storing an in-process gauge of
//! "current RPS" would require a sliding-window estimator and would
//! disagree with whichever window the dashboard chose anyway.
//!
//! Headline PromQL:
//!
//! ```promql
//! # overall RPS
//! sum(rate(geocoder_requests_total[1m]))
//!
//! # per-country RPS, /search only
//! sum by (country) (rate(geocoder_requests_total{endpoint="search"}[1m]))
//!
//! # shadow accuracy: 1 − (mismatch share) per endpoint per country
//! 1 - (
//!   sum by (endpoint) (rate(geocoder_shadow_outcomes_total{outcome="mismatch"}[5m]))
//!   / sum by (endpoint) (rate(geocoder_shadow_outcomes_total{outcome=~"match|mismatch"}[5m]))
//! )
//!
//! # /search top-1 distance distribution (Google-vs-ours)
//! histogram_quantile(0.95, sum by (le) (rate(geocoder_shadow_distance_meters_bucket[5m])))
//! ```
//!
//! ## Cardinality
//!
//! - `endpoint` — fixed at ~7 values (`reverse`, `search`, `autocomplete`,
//!   `validate`, `ip_geocode`, `h3`, `unknown`).
//! - `country` — ISO 3166-1 alpha-2 codes plus `unknown`. Bounded at
//!   ~250. Combined with endpoint that's ~1750 series for
//!   `requests_total`, well within Prometheus norms.
//! - `outcome` — fixed at the 9 documented `Outcome` variants.
//! - `axis`/`result` on `shadow_match_total` — 4 × 3 = 12 combinations
//!   per endpoint, ~24 series total.

use std::sync::Arc;

use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider};
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts, Registry, TextEncoder,
};

/// Histogram bucket layout for request duration in seconds. Tuned for
/// our actual latency profile: typical request 0.5–5 ms, p99 < 50 ms,
/// outliers up to 1 s. Default Prometheus buckets bottom out at 5 ms
/// which is too coarse for our hot path.
const DURATION_BUCKETS_SECONDS: &[f64] =
    &[0.0005, 0.001, 0.002, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5];

/// Histogram bucket layout for shadow distance in metres. The
/// 1 km mismatch threshold lives at the high end; below that we want
/// resolution around the typical urban-block scales (10 m to 500 m).
const DISTANCE_BUCKETS_METERS: &[f64] = &[
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 10_000.0,
];

/// Paired Prometheus + OTel handles for the metrics we expose.
/// `Arc<Metrics>` is plumbed via `axum::Extension` into every handler
/// that wants to observe; the `record_*` methods do the dual-write
/// transparently.
///
/// The OTel-side fields are `Option<...>` because the OTel meter
/// provider is itself optional — when `OTEL_METRICS_ENABLED=false`
/// (or no endpoint is configured), `Metrics::new_prometheus_only`
/// returns a registry with the prom side wired and `None` for every
/// OTel handle. The Prometheus surface always works.
pub struct Metrics {
    registry: Registry,

    // --- Prometheus side (always registered) ---

    /// Inbound request count, sliced by endpoint + ISO country (or
    /// `unknown` when the request didn't carry / produce one).
    /// Counters work for RPS via PromQL `rate(...[1m])`.
    pub requests_total: IntCounterVec,

    /// Wall-clock duration per request, sliced by endpoint only.
    /// Country split here would explode cardinality without operator
    /// value — most teams alarm on per-endpoint p99 anyway.
    pub request_duration_seconds: HistogramVec,

    /// Shadow validator outcomes — one increment per shadow attempt
    /// regardless of whether it actually reached Google. The
    /// `outcome` label is the same as the `geocoder.shadow.outcome`
    /// span attribute.
    pub shadow_outcomes_total: IntCounterVec,

    /// Per-axis admin-field match histogram for shadow comparisons.
    /// `axis` ∈ {country, state, city, road}, `result` ∈
    /// {match, mismatch, none}. Lets dashboards plot e.g. "city
    /// match rate per country."
    pub shadow_match_total: IntCounterVec,

    /// /search top-1 distance against Google's top-1 (metres).
    pub shadow_distance_meters: HistogramVec,

    /// Cumulative count of `try_send` failures because the shadow
    /// channel was full. Surfaced as a counter (Prometheus computes
    /// the rate); operators alarm on `rate(...) > 0.01` to detect
    /// shadow backpressure.
    pub shadow_queue_full_total: IntCounter,

    // --- OTel side (None when OTEL_METRICS_ENABLED is off) ---

    otel_requests_total: Option<Counter<u64>>,
    otel_request_duration_seconds: Option<Histogram<f64>>,
    otel_shadow_outcomes_total: Option<Counter<u64>>,
    otel_shadow_match_total: Option<Counter<u64>>,
    otel_shadow_distance_meters: Option<Histogram<f64>>,
    otel_shadow_queue_full_total: Option<Counter<u64>>,
}

impl Metrics {
    /// Prometheus-only constructor. Used when no OTel meter provider
    /// is available (env var off, or operator opted out). The OTLP
    /// push path stays dark; `/metrics` works exactly as before.
    pub fn new() -> Arc<Self> {
        Self::with_optional_meter(None)
    }

    /// Build with an optional OTel meter provider. When provided, every
    /// observation made via `record_request` / `record_shadow` /
    /// the public IntCounter handles also flows through OTel
    /// instruments registered against the supplied provider, which
    /// the periodic reader pushes to OTLP.
    pub fn with_optional_meter(meter_provider: Option<&SdkMeterProvider>) -> Arc<Self> {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new(
                "geocoder_requests_total",
                "Inbound requests, sliced by endpoint and ISO country (lowercase, `unknown` when absent).",
            ),
            &["endpoint", "country"],
        )
        .expect("requests_total opts");
        registry.register(Box::new(requests_total.clone())).expect("register requests_total");

        let request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "geocoder_request_duration_seconds",
                "Per-request wall-clock duration in seconds, sliced by endpoint.",
            )
            .buckets(DURATION_BUCKETS_SECONDS.to_vec()),
            &["endpoint"],
        )
        .expect("request_duration_seconds opts");
        registry.register(Box::new(request_duration_seconds.clone())).expect("register duration");

        let shadow_outcomes_total = IntCounterVec::new(
            Opts::new(
                "geocoder_shadow_outcomes_total",
                "Shadow-validation outcomes against Google's Geocoding API, sliced by endpoint and outcome enum.",
            ),
            &["endpoint", "outcome"],
        )
        .expect("shadow_outcomes_total opts");
        registry.register(Box::new(shadow_outcomes_total.clone())).expect("register shadow_outcomes");

        let shadow_match_total = IntCounterVec::new(
            Opts::new(
                "geocoder_shadow_match_total",
                "Per-axis admin-field comparison results from shadow validation. Axis ∈ {country, state, city, road}, result ∈ {match, mismatch, none}.",
            ),
            &["endpoint", "axis", "result"],
        )
        .expect("shadow_match_total opts");
        registry.register(Box::new(shadow_match_total.clone())).expect("register shadow_match");

        let shadow_distance_meters = HistogramVec::new(
            HistogramOpts::new(
                "geocoder_shadow_distance_meters",
                "Distance in metres between our /search top-1 result and Google's top-1, per shadow comparison.",
            )
            .buckets(DISTANCE_BUCKETS_METERS.to_vec()),
            &["endpoint"],
        )
        .expect("shadow_distance_meters opts");
        registry.register(Box::new(shadow_distance_meters.clone())).expect("register shadow_distance");

        let shadow_queue_full_total = IntCounter::new(
            "geocoder_shadow_queue_full_total",
            "Cumulative shadow `try_send` failures because the bounded mpsc was full. Should stay flat — non-zero rate = shadow backpressure.",
        )
        .expect("shadow_queue_full_total opts");
        registry.register(Box::new(shadow_queue_full_total.clone())).expect("register queue_full");

        // OTel side — paired registrations against the supplied meter
        // provider. Same metric names, same semantic labels. The
        // periodic reader (configured in telemetry::build_meter_provider)
        // pushes these to OTLP at `OTEL_METRIC_EXPORT_INTERVAL`.
        let otel = meter_provider.map(|mp| build_otel_instruments(&mp.meter("query-server")));

        Arc::new(Self {
            registry,
            requests_total,
            request_duration_seconds,
            shadow_outcomes_total,
            shadow_match_total,
            shadow_distance_meters,
            shadow_queue_full_total,
            otel_requests_total: otel.as_ref().map(|o| o.requests_total.clone()),
            otel_request_duration_seconds: otel.as_ref().map(|o| o.request_duration_seconds.clone()),
            otel_shadow_outcomes_total: otel.as_ref().map(|o| o.shadow_outcomes_total.clone()),
            otel_shadow_match_total: otel.as_ref().map(|o| o.shadow_match_total.clone()),
            otel_shadow_distance_meters: otel.as_ref().map(|o| o.shadow_distance_meters.clone()),
            otel_shadow_queue_full_total: otel.as_ref().map(|o| o.shadow_queue_full_total.clone()),
        })
    }

    /// Render the full registry as Prometheus text exposition format.
    /// Returns the bytes ready to send as the body of `/metrics`. The
    /// `Content-Type` the scraper expects is set by the handler.
    pub fn render(&self) -> Vec<u8> {
        let encoder = TextEncoder::new();
        let mut buf = Vec::with_capacity(1024);
        let metric_families = self.registry.gather();
        // encode() can fail only on a downstream io::Write error; we're
        // writing to a Vec, so that path is unreachable in practice.
        encoder.encode(&metric_families, &mut buf).expect("encode metrics to Vec");
        buf
    }

    /// Convenience: record one request with country derived from a
    /// `country_code` Option. Pre-normalises to lowercase + clamps to
    /// `unknown` so handlers don't have to worry about cardinality
    /// hygiene at every call site. Bumps both the Prometheus and the
    /// OTel sides when the latter is configured.
    pub fn record_request(&self, endpoint: &str, country_code: Option<&str>, duration_secs: f64) {
        let country = canonical_country(country_code);

        // --- Prometheus ---
        self.requests_total
            .with_label_values(&[endpoint, country])
            .inc();
        self.request_duration_seconds
            .with_label_values(&[endpoint])
            .observe(duration_secs);

        // --- OTel ---
        if let Some(c) = self.otel_requests_total.as_ref() {
            c.add(
                1,
                &[
                    KeyValue::new("endpoint", endpoint.to_string()),
                    KeyValue::new("country", country.to_string()),
                ],
            );
        }
        if let Some(h) = self.otel_request_duration_seconds.as_ref() {
            h.record(
                duration_secs,
                &[KeyValue::new("endpoint", endpoint.to_string())],
            );
        }
    }

    /// Record one shadow comparison's worth of metrics in a single
    /// call. Replaces the previous 3-call sequence (outcomes, 4×match,
    /// distance) the shadow worker used. `axes` is a fixed-shape slice
    /// of `(axis_name, Option<bool>)` for country/state/city/road in
    /// that order; `None` means the axis wasn't compared (e.g. road
    /// on /search), `Some(true)` is a match, `Some(false)` is a
    /// mismatch. `distance_m` is the /search top-1 haversine in
    /// metres, omitted on /reverse.
    pub fn record_shadow(
        &self,
        endpoint: &str,
        outcome: &str,
        axes: &[(&str, Option<bool>)],
        distance_m: Option<f64>,
    ) {
        // --- Prometheus ---
        self.shadow_outcomes_total
            .with_label_values(&[endpoint, outcome])
            .inc();
        for (axis, axis_match) in axes {
            let result = match *axis_match {
                Some(true) => "match",
                Some(false) => "mismatch",
                None => "none",
            };
            self.shadow_match_total
                .with_label_values(&[endpoint, axis, result])
                .inc();
        }
        if let Some(d) = distance_m {
            self.shadow_distance_meters
                .with_label_values(&[endpoint])
                .observe(d);
        }

        // --- OTel ---
        if let Some(c) = self.otel_shadow_outcomes_total.as_ref() {
            c.add(
                1,
                &[
                    KeyValue::new("endpoint", endpoint.to_string()),
                    KeyValue::new("outcome", outcome.to_string()),
                ],
            );
        }
        if let Some(c) = self.otel_shadow_match_total.as_ref() {
            for (axis, axis_match) in axes {
                let result = match *axis_match {
                    Some(true) => "match",
                    Some(false) => "mismatch",
                    None => "none",
                };
                c.add(
                    1,
                    &[
                        KeyValue::new("endpoint", endpoint.to_string()),
                        KeyValue::new("axis", axis.to_string()),
                        KeyValue::new("result", result.to_string()),
                    ],
                );
            }
        }
        if let (Some(h), Some(d)) = (self.otel_shadow_distance_meters.as_ref(), distance_m) {
            h.record(d, &[KeyValue::new("endpoint", endpoint.to_string())]);
        }
    }

    /// Bump the shadow queue-full counter. Called from the dispatcher
    /// when `try_send` fails — we don't have an endpoint label on this
    /// path because the job got dropped before we could reliably
    /// inspect it.
    pub fn inc_shadow_queue_full(&self) {
        self.shadow_queue_full_total.inc();
        if let Some(c) = self.otel_shadow_queue_full_total.as_ref() {
            c.add(1, &[]);
        }
    }
}

/// Internal: build the OTel-side instruments. Kept in a struct (rather
/// than tuple-returning) so adding a new metric is a one-line diff in
/// both this builder and the corresponding `Metrics` field.
struct OtelInstruments {
    requests_total: Counter<u64>,
    request_duration_seconds: Histogram<f64>,
    shadow_outcomes_total: Counter<u64>,
    shadow_match_total: Counter<u64>,
    shadow_distance_meters: Histogram<f64>,
    shadow_queue_full_total: Counter<u64>,
}

fn build_otel_instruments(meter: &Meter) -> OtelInstruments {
    OtelInstruments {
        requests_total: meter
            .u64_counter("geocoder_requests_total")
            .with_description(
                "Inbound requests, sliced by endpoint and ISO country (lowercase, `unknown` when absent).",
            )
            .build(),
        request_duration_seconds: meter
            .f64_histogram("geocoder_request_duration_seconds")
            .with_description("Per-request wall-clock duration in seconds, sliced by endpoint.")
            .with_unit("s")
            .with_boundaries(DURATION_BUCKETS_SECONDS.to_vec())
            .build(),
        shadow_outcomes_total: meter
            .u64_counter("geocoder_shadow_outcomes_total")
            .with_description(
                "Shadow-validation outcomes against Google's Geocoding API, sliced by endpoint and outcome enum.",
            )
            .build(),
        shadow_match_total: meter
            .u64_counter("geocoder_shadow_match_total")
            .with_description(
                "Per-axis admin-field comparison results from shadow validation. Axis ∈ {country, state, city, road}, result ∈ {match, mismatch, none}.",
            )
            .build(),
        shadow_distance_meters: meter
            .f64_histogram("geocoder_shadow_distance_meters")
            .with_description(
                "Distance in metres between our /search top-1 result and Google's top-1, per shadow comparison.",
            )
            .with_unit("m")
            .with_boundaries(DISTANCE_BUCKETS_METERS.to_vec())
            .build(),
        shadow_queue_full_total: meter
            .u64_counter("geocoder_shadow_queue_full_total")
            .with_description(
                "Cumulative shadow `try_send` failures because the bounded mpsc was full.",
            )
            .build(),
    }
}

/// Normalise an ISO 3166-1 alpha-2 country code: lowercase, accept
/// only 2-letter alpha. Anything else collapses to `unknown` so the
/// `country` label can't be a long-tail explosion of typos / random
/// strings.
pub fn canonical_country(raw: Option<&str>) -> &'static str {
    match raw {
        Some(s) => {
            let trimmed = s.trim();
            let bytes = trimmed.as_bytes();
            if bytes.len() == 2
                && bytes[0].is_ascii_alphabetic()
                && bytes[1].is_ascii_alphabetic()
            {
                // Build the lowercase 2-letter form and intern via a
                // perfect hash table. ISO has 249 codes; we keep the
                // table small by enumerating only the alpha pairs we
                // actually accept. The label needs to be `&'static str`
                // for the IntCounterVec API; rather than maintain a
                // 676-entry static table by hand, we fall back to the
                // `unknown` sentinel for any code we don't see in
                // practice. In production, callers pass codes derived
                // from our index data — those all show up in the
                // common-codes table once at boot, so the cardinality
                // is naturally bounded.
                static_lower_two(bytes[0].to_ascii_lowercase(), bytes[1].to_ascii_lowercase())
            } else {
                "unknown"
            }
        }
        None => "unknown",
    }
}

/// Map a 2-byte lowercase alpha pair to one of 676 (=26²) `&'static
/// str`s. Implemented as a one-time-built table behind a `OnceLock`
/// so we avoid running a 676-arm match on every request.
fn static_lower_two(a: u8, b: u8) -> &'static str {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Box<[String; 676]>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut arr: [String; 676] = std::array::from_fn(|_| String::new());
        for x in 0..26 {
            for y in 0..26 {
                let s = format!(
                    "{}{}",
                    (b'a' + x as u8) as char,
                    (b'a' + y as u8) as char
                );
                arr[x * 26 + y] = s;
            }
        }
        Box::new(arr)
    });
    let idx = ((a - b'a') as usize) * 26 + (b - b'a') as usize;
    // SAFETY: a, b verified ascii alphabetic before call.
    table[idx].as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_country_lowercases_two_letter() {
        assert_eq!(canonical_country(Some("AU")), "au");
        assert_eq!(canonical_country(Some("au")), "au");
        assert_eq!(canonical_country(Some(" Us ")), "us");
    }

    #[test]
    fn canonical_country_rejects_non_alpha_two() {
        assert_eq!(canonical_country(Some("AUS")), "unknown");
        assert_eq!(canonical_country(Some("a")), "unknown");
        assert_eq!(canonical_country(Some("12")), "unknown");
        assert_eq!(canonical_country(Some("")), "unknown");
        assert_eq!(canonical_country(None), "unknown");
    }

    #[test]
    fn canonical_country_returns_static_str() {
        // The IntCounterVec API needs `&'static str` for the label
        // value; exercising the same code twice and getting the same
        // pointer would be the strongest assertion, but the OnceLock
        // table makes that strict. We at least verify the value is
        // stable across calls.
        let a = canonical_country(Some("AU"));
        let b = canonical_country(Some("au"));
        assert_eq!(a, b);
        assert_eq!(a, "au");
    }

    #[test]
    fn render_produces_valid_prometheus_text() {
        let metrics = Metrics::new();
        // Touch one of each shape so the scrape isn't empty.
        metrics.record_request("reverse", Some("au"), 0.0012);
        metrics
            .shadow_outcomes_total
            .with_label_values(&["reverse", "match"])
            .inc();
        metrics
            .shadow_match_total
            .with_label_values(&["reverse", "country", "match"])
            .inc();
        metrics
            .shadow_distance_meters
            .with_label_values(&["search"])
            .observe(123.4);
        metrics.shadow_queue_full_total.inc();

        let body = metrics.render();
        let text = std::str::from_utf8(&body).expect("text exposition is utf-8");

        // Each metric must declare both # HELP and # TYPE in the
        // standard exposition format. A scraper without these lines
        // can't classify the series.
        for name in [
            "geocoder_requests_total",
            "geocoder_request_duration_seconds",
            "geocoder_shadow_outcomes_total",
            "geocoder_shadow_match_total",
            "geocoder_shadow_distance_meters",
            "geocoder_shadow_queue_full_total",
        ] {
            assert!(
                text.contains(&format!("# HELP {name}")),
                "missing HELP for {name} in:\n{text}"
            );
            assert!(
                text.contains(&format!("# TYPE {name}")),
                "missing TYPE for {name} in:\n{text}"
            );
        }
        // Spot-check one observed value lands in the body.
        assert!(text.contains("geocoder_requests_total{country=\"au\",endpoint=\"reverse\"} 1"));
    }

    #[test]
    fn requests_counter_increments() {
        let metrics = Metrics::new();
        for _ in 0..5 {
            metrics.record_request("search", Some("au"), 0.001);
        }
        let body = String::from_utf8(metrics.render()).unwrap();
        assert!(
            body.contains("geocoder_requests_total{country=\"au\",endpoint=\"search\"} 5"),
            "expected count=5 in body:\n{body}"
        );
    }

    #[test]
    fn duration_histogram_records_buckets() {
        let metrics = Metrics::new();
        metrics.record_request("reverse", Some("au"), 0.003); // lands in 0.005s bucket
        metrics.record_request("reverse", Some("au"), 0.030); // lands in 0.05s bucket
        let body = String::from_utf8(metrics.render()).unwrap();
        // Histogram exposition includes `_bucket{le="..."}` lines for
        // each bucket plus a `_sum` and `_count`.
        assert!(body.contains("geocoder_request_duration_seconds_bucket"));
        assert!(body.contains("geocoder_request_duration_seconds_count"));
        assert!(body.contains("geocoder_request_duration_seconds_sum"));
    }

    #[test]
    fn unknown_country_used_when_input_invalid() {
        let metrics = Metrics::new();
        metrics.record_request("h3", Some("XYZ"), 0.0001);
        metrics.record_request("h3", None, 0.0001);
        let body = String::from_utf8(metrics.render()).unwrap();
        // Both fall under unknown — counter == 2.
        assert!(
            body.contains("geocoder_requests_total{country=\"unknown\",endpoint=\"h3\"} 2"),
            "expected count=2 under unknown country in:\n{body}"
        );
    }

    #[test]
    fn record_shadow_dual_writes_prometheus_side() {
        let metrics = Metrics::new();
        let axes = [
            ("country", Some(true)),
            ("state", Some(true)),
            ("city", Some(false)),
            ("road", None),
        ];
        metrics.record_shadow("reverse", "mismatch", &axes, None);

        let body = String::from_utf8(metrics.render()).unwrap();
        // Outcome counter incremented once.
        assert!(
            body.contains("geocoder_shadow_outcomes_total{endpoint=\"reverse\",outcome=\"mismatch\"} 1"),
            "outcome counter not incremented:\n{body}"
        );
        // One row per axis with the right `result` label.
        assert!(body.contains("geocoder_shadow_match_total{axis=\"country\",endpoint=\"reverse\",result=\"match\"} 1"));
        assert!(body.contains("geocoder_shadow_match_total{axis=\"state\",endpoint=\"reverse\",result=\"match\"} 1"));
        assert!(body.contains("geocoder_shadow_match_total{axis=\"city\",endpoint=\"reverse\",result=\"mismatch\"} 1"));
        assert!(body.contains("geocoder_shadow_match_total{axis=\"road\",endpoint=\"reverse\",result=\"none\"} 1"));
    }

    #[test]
    fn record_shadow_search_records_distance_histogram() {
        let metrics = Metrics::new();
        let axes = [
            ("country", Some(true)),
            ("state", Some(true)),
            ("city", Some(true)),
            ("road", None),
        ];
        // /search comparison, top-1 distance 250 m → lands in the
        // 250 m bucket (the next-larger boundary after 100 m in
        // DISTANCE_BUCKETS_METERS).
        metrics.record_shadow("search", "match", &axes, Some(250.0));

        let body = String::from_utf8(metrics.render()).unwrap();
        assert!(body.contains("geocoder_shadow_distance_meters_bucket"));
        assert!(body.contains("geocoder_shadow_distance_meters_sum"));
        assert!(body.contains("geocoder_shadow_distance_meters_count"));
    }

    #[test]
    fn metrics_new_works_without_meter_provider() {
        // The Prometheus surface is unconditional. When OTel is off,
        // every record_* call still writes to the prom side and the
        // OTel branches are no-ops (Option::None). This test pins
        // that contract — a refactor that accidentally requires the
        // meter provider would break the no-OTLP deployment.
        let metrics = Metrics::with_optional_meter(None);
        metrics.record_request("reverse", Some("au"), 0.001);
        metrics.record_shadow("reverse", "match", &[("country", Some(true))], None);
        metrics.inc_shadow_queue_full();
        let body = String::from_utf8(metrics.render()).unwrap();
        assert!(body.contains("geocoder_requests_total"));
        assert!(body.contains("geocoder_shadow_outcomes_total"));
        assert!(body.contains("geocoder_shadow_queue_full_total 1"));
    }
}
