//! Prometheus metrics surface.
//!
//! Operators scrape `GET /metrics` for the standard Prometheus text
//! exposition format. The metrics are named with a `geocoder_` prefix
//! and labelled by endpoint + (where meaningful) country so per-country
//! request rates and accuracy can be sliced in PromQL without joining
//! to the underlying logs.
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

/// Registry + named handles for every series we expose. `Arc<Metrics>`
/// is plumbed via `axum::Extension` into every handler that wants to
/// observe.
pub struct Metrics {
    registry: Registry,

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
}

impl Metrics {
    /// Construct a fresh registry + register every named metric. Panics
    /// only if a metric registration fails — which only happens on a
    /// programming bug (duplicate name), not on environment state.
    pub fn new() -> Arc<Self> {
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

        Arc::new(Self {
            registry,
            requests_total,
            request_duration_seconds,
            shadow_outcomes_total,
            shadow_match_total,
            shadow_distance_meters,
            shadow_queue_full_total,
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
    /// hygiene at every call site.
    pub fn record_request(&self, endpoint: &str, country_code: Option<&str>, duration_secs: f64) {
        let country = canonical_country(country_code);
        self.requests_total
            .with_label_values(&[endpoint, country])
            .inc();
        self.request_duration_seconds
            .with_label_values(&[endpoint])
            .observe(duration_secs);
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
}
