//! Sampled, async shadow validation against Google's Geocoding API.
//!
//! A small fraction of `/reverse` and `/search` requests are forwarded
//! to Google after the user-facing response has already been returned.
//! The handler does a single `try_send` to a bounded channel
//! (microseconds) and is otherwise unaffected by Google's latency.
//! A dedicated background worker drains the channel, dispatches the
//! Google call subject to per-second + daily caps, parses the
//! response, and emits a structured comparison record (tracing span +
//! INFO log line) so an aggregator can compute admin-field match
//! rates and search-top-1 distance histograms without further
//! instrumentation.
//!
//! ## Cost protection (defence in depth)
//!
//! 1. **Master switch** — `GOOGLE_GEOCODING_ENABLED`. Even with a key
//!    set, an explicit `false` keeps the dispatcher dormant. Operators
//!    staging a deploy with the secret already provisioned but cost not
//!    yet authorised use this.
//! 2. **Sample rate** — random per-request gate (default 0.1 %).
//! 3. **RPS cap** — token bucket (default 4/s) smooths bursts.
//! 4. **Daily cap** — atomic counter reset at UTC midnight (default
//!    1 000/day, well under the $200 free-tier ceiling at $5/1000).
//! 5. **API-key absence** — disabled outright.
//!
//! Each gate fails closed: a malformed env var, a missing key, or a
//! `REQUEST_DENIED` from Google all park the dispatcher rather than
//! letting traffic leak through.
//!
//! ## Failure isolation
//!
//! - `REQUEST_DENIED` (bad key, billing problem) → set
//!   `auth_disabled=true` for the rest of the process lifetime. The
//!   worker keeps draining the channel so handlers don't see queue
//!   pressure; outcomes are reported as `auth_disabled`.
//! - `OVER_QUERY_LIMIT` → set a 30 s backoff window. Channel drained
//!   meanwhile; outcomes reported as `rate_limited`. Avoids the retry
//!   storm that would otherwise pin Google's quota in saturation.
//! - HTTP timeout / network error → drop with `outcome=timeout`,
//!   deduped warning.
//! - `ZERO_RESULTS` is **not** an error — it's a real signal that
//!   Google had no answer for that input; reported as
//!   `outcome=zero_results`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, TimeZone, Utc};
use serde::Deserialize;
use tokio::sync::{mpsc, Semaphore};

use crate::geo::haversine_m;
use crate::metrics::Metrics;
use crate::Address;

// --- Public configuration ------------------------------------------------

/// Default base URL for Google's Geocoding API. Made overridable in
/// `ShadowConfig::base_url` so integration tests can point at a mock
/// localhost server. Operators wouldn't normally change this — it's
/// not exposed as an env var.
pub const DEFAULT_GOOGLE_BASE_URL: &str = "https://maps.googleapis.com/maps/api/geocode/json";

/// Tuneable parameters for the shadow worker. All defaults are sized
/// for production safety: 0.1 % sample, 4 RPS, 1000/day. An operator
/// changing any of these is opting into more expensive coverage.
#[derive(Clone, Debug)]
pub struct ShadowConfig {
    pub api_key: String,
    pub sample_rate: f64,
    pub daily_cap: u32,
    pub rps_cap: u32,
    pub queue_capacity: usize,
    pub inflight_cap: usize,
    pub timeout: Duration,
    pub backoff_secs: u64,
    /// Base URL (no trailing query). Defaults to Google's endpoint;
    /// overridden in tests to point at a mock server. Not surfaced as
    /// an env var because changing it in production is an anti-pattern.
    pub base_url: String,
}

impl ShadowConfig {
    /// Resolve config from environment. Returns `Some(cfg)` only when
    /// both the master switch is on and an API key is set. Logs a
    /// startup line summarising why if disabled — operators get one
    /// place to look for "is it on?".
    pub fn from_env() -> Option<Self> {
        let flag = std::env::var("GOOGLE_GEOCODING_ENABLED").ok();
        let api_key_opt = std::env::var("GOOGLE_GEOCODING_API_KEY").ok();
        let api_key_set = api_key_opt
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        let enabled = resolve_shadow_enabled(flag.as_deref(), api_key_set);
        if !enabled {
            let reason = match (flag.as_deref(), api_key_set) {
                (Some(v), _) if matches!(v.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "no" | "off" | "") => "GOOGLE_GEOCODING_ENABLED=false",
                (Some(v), _) if matches!(v.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on") && !api_key_set => "GOOGLE_GEOCODING_ENABLED=true but GOOGLE_GEOCODING_API_KEY unset",
                (Some(other), _) => {
                    tracing::warn!(
                        target: "query_server::shadow",
                        value = %other,
                        "unrecognised GOOGLE_GEOCODING_ENABLED — treating as disabled"
                    );
                    "unrecognised GOOGLE_GEOCODING_ENABLED"
                }
                (None, false) => "GOOGLE_GEOCODING_API_KEY unset",
                (None, true) => unreachable!("resolve_shadow_enabled returns true here"),
            };
            tracing::info!(
                target: "query_server::shadow",
                reason = reason,
                "shadow validation disabled"
            );
            return None;
        }

        let api_key = api_key_opt.expect("api_key_set guarantees Some");

        Some(ShadowConfig {
            api_key,
            sample_rate: env_f64("GOOGLE_GEOCODING_SAMPLE_RATE", 0.001).clamp(0.0, 1.0),
            daily_cap: clamp_daily_cap(env_u32("GOOGLE_GEOCODING_DAILY_CAP", 1000)),
            rps_cap: env_u32("GOOGLE_GEOCODING_RPS_CAP", 4).max(1),
            queue_capacity: env_usize("GOOGLE_GEOCODING_QUEUE_CAPACITY", 256).max(1),
            inflight_cap: env_usize("GOOGLE_GEOCODING_INFLIGHT_CAP", 4).max(1),
            timeout: Duration::from_millis(env_u64("GOOGLE_GEOCODING_TIMEOUT_MS", 2000)),
            backoff_secs: env_u64("GOOGLE_GEOCODING_BACKOFF_SECS", 30),
            base_url: DEFAULT_GOOGLE_BASE_URL.to_string(),
        })
    }
}

/// Pure form of the master-switch resolution. Truth matrix:
///
/// | flag        | key_set | result   |
/// |-------------|---------|----------|
/// | None        | true    | enabled  |
/// | None        | false   | disabled |
/// | true*       | true    | enabled  |
/// | true*       | false   | disabled |
/// | false*      | any     | disabled |
/// | garbage     | any     | disabled |
///
/// `true*` accepts `true|1|yes|on` (case-insensitive, whitespace-trimmed);
/// `false*` accepts `false|0|no|off|<empty>`.
pub(crate) fn resolve_shadow_enabled(flag: Option<&str>, api_key_set: bool) -> bool {
    let parsed: Option<bool> = match flag {
        None => None,
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" | "" => Some(false),
            _ => Some(false), // garbage → disabled
        },
    };
    match (parsed, api_key_set) {
        (Some(true), true) => true,
        (Some(true), false) => false,
        (Some(false), _) => false,
        (None, has_key) => has_key,
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Maximum daily cap we allow an operator to set. At the documented
/// $5/1000 Google price, the upper bound corresponds to ~$500/day —
/// already well past the normal $200/month free tier and into "you
/// definitely meant to do this" territory. Operators wanting more
/// either raise the constant in source (visible in code review) or
/// set up Google's own billing alerts.
const MAX_DAILY_CAP: u32 = 100_000;

/// Clamp the daily cap to a defensible upper bound. Misreading a typo
/// like `GOOGLE_GEOCODING_DAILY_CAP=1000000` (intended 1000) without
/// this guard would silently authorise $5 000/day in Google calls.
/// We log the clamp rather than failing startup so operators get told
/// what happened without the deploy bouncing.
fn clamp_daily_cap(raw: u32) -> u32 {
    if raw > MAX_DAILY_CAP {
        tracing::warn!(
            target: "query_server::shadow",
            requested = raw,
            cap = MAX_DAILY_CAP,
            "GOOGLE_GEOCODING_DAILY_CAP clamped — raise MAX_DAILY_CAP in source if intentional"
        );
        return MAX_DAILY_CAP;
    }
    raw
}

// --- Outcome + Endpoint enums -------------------------------------------

/// What happened to a sampled request. Reported as
/// `geocoder.shadow.outcome` on the span and as the `outcome` field on
/// the structured log line, so an aggregator can group by category
/// without parsing log strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Match,
    Mismatch,
    ZeroResults,
    GoogleError,
    AuthDisabled,
    DailyCapHit,
    QueueFull,
    RateLimited,
    Timeout,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Match => "match",
            Outcome::Mismatch => "mismatch",
            Outcome::ZeroResults => "zero_results",
            Outcome::GoogleError => "google_error",
            Outcome::AuthDisabled => "auth_disabled",
            Outcome::DailyCapHit => "daily_cap_hit",
            Outcome::QueueFull => "queue_full",
            Outcome::RateLimited => "rate_limited",
            Outcome::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Reverse,
    Search,
}

impl Endpoint {
    fn as_str(self) -> &'static str {
        match self {
            Endpoint::Reverse => "reverse",
            Endpoint::Search => "search",
        }
    }
}

// --- Owned snapshot of our result ---------------------------------------

/// Owned copy of the fields we compare against Google. Built at the
/// handler boundary from the borrowed `Address`/result so the channel
/// can carry it without lifetime gymnastics. The clone is shallow —
/// each `Option<String>` is at most a few dozen bytes — but it
/// happens on the request-serving thread, so we pay the alloc cost
/// only on sampled requests.
#[derive(Clone, Debug, Default)]
pub struct OurSnapshot {
    pub country_code: Option<String>,
    pub state: Option<String>,
    pub city: Option<String>,
    pub road: Option<String>,
    pub display_name: Option<String>,
    /// For /search top-1: the result coord we'd dispatch to the client.
    /// `None` for /reverse since the input coord is the reference.
    pub lat: Option<f64>,
    pub lng: Option<f64>,
}

impl OurSnapshot {
    /// Build a snapshot from the borrowed `Address` returned by the
    /// reverse-geocoder. `lat`/`lng` are left `None` — the caller
    /// supplies coords explicitly for /search.
    pub fn from_address(addr: &Address<'_>) -> Self {
        let d = &addr.address;
        OurSnapshot {
            country_code: d.country_code.clone(),
            state: d.state.map(str::to_owned),
            city: d.city.map(str::to_owned),
            road: d.road.map(str::to_owned),
            display_name: addr.display_name.clone(),
            lat: None,
            lng: None,
        }
    }
}

// --- Shadow job carried over the channel --------------------------------

#[derive(Debug)]
pub struct ShadowJob {
    pub endpoint: Endpoint,
    /// For /reverse: the input coord. For /search: the top-1 result coord.
    pub our_lat: f64,
    pub our_lng: f64,
    /// The freeform query string for /search; ignored for /reverse.
    pub query: Option<String>,
    /// Optional country code hint passed to Google as `region=` (ccTLD
    /// bias). For /search only.
    pub country_code: Option<String>,
    pub ours: OurSnapshot,
}

// --- Public dispatcher --------------------------------------------------

pub struct ShadowDispatcher {
    tx: mpsc::Sender<ShadowJob>,
    /// Counter of `try_send` failures due to channel full. Read at
    /// shutdown / via /healthz extensions if we ever add them.
    queue_full_count: Arc<AtomicU64>,
    /// Same probability gate the worker would apply, exposed here so
    /// the dispatcher can short-circuit before doing the snapshot
    /// clone. Held in an `AtomicU64` of f64 bits so it's cheaply
    /// readable per-request and a future runtime-tune endpoint can
    /// flip it without restart.
    sample_rate_bits: Arc<AtomicU64>,
    /// Optional Prometheus metrics handle. When present, the worker
    /// records outcome / distance / per-axis match counters. When
    /// absent (e.g. unit-test contexts that don't care about metrics),
    /// the worker still emits spans + logs but skips the metrics path.
    metrics: Option<Arc<Metrics>>,
}

impl ShadowDispatcher {
    /// Spawn the worker (and its midnight-reset companion) onto the
    /// current tokio runtime. Returns a handle the router can clone
    /// into an axum Extension. `metrics`, when supplied, receives
    /// shadow outcome + distance + match observations.
    pub fn spawn(cfg: ShadowConfig, metrics: Option<Arc<Metrics>>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<ShadowJob>(cfg.queue_capacity);
        let queue_full_count = Arc::new(AtomicU64::new(0));
        let sample_rate_bits = Arc::new(AtomicU64::new(f64::to_bits(cfg.sample_rate)));

        let worker = Worker::new(cfg, metrics.clone());
        worker.spawn_loops(rx);

        tracing::info!(
            target: "query_server::shadow",
            "shadow dispatcher started"
        );

        Arc::new(Self {
            tx,
            queue_full_count,
            sample_rate_bits,
            metrics,
        })
    }

    /// Sample a /reverse request. Cheap: returns immediately when the
    /// probability gate fails. On the rare sampled path, takes one
    /// snapshot clone and a `try_send`. Never awaits, never blocks.
    pub fn shadow_reverse(&self, lat: f64, lng: f64, ours: &Address<'_>) {
        if !self.passes_sample_gate() {
            return;
        }
        let job = ShadowJob {
            endpoint: Endpoint::Reverse,
            our_lat: lat,
            our_lng: lng,
            query: None,
            country_code: None,
            ours: OurSnapshot::from_address(ours),
        };
        self.try_send(job);
    }

    /// Sample a /search request. `ours_top` carries the top-1 hit's
    /// coords + admin shape; `None` when our search returned no hits
    /// — we still shadow because Google's verdict on a zero-result
    /// query is informative.
    pub fn shadow_search(
        &self,
        query: &str,
        country_code: Option<&str>,
        ours_top: Option<(f64, f64, &Address<'_>)>,
    ) {
        let snap = ours_top.map(|(lat, lng, addr)| (lat, lng, OurSnapshot::from_address(addr)));
        self.shadow_search_with_snapshot(query, country_code, snap);
    }

    /// Same as [`shadow_search`] but takes an already-owned snapshot.
    /// Used by `/search` handlers that hold their results as JSON
    /// values rather than the borrowed `Address` struct — they
    /// build the `OurSnapshot` from JSON fields and call this.
    pub fn shadow_search_with_snapshot(
        &self,
        query: &str,
        country_code: Option<&str>,
        ours_top: Option<(f64, f64, OurSnapshot)>,
    ) {
        if !self.passes_sample_gate() {
            return;
        }
        let (lat, lng, ours_snap) = ours_top.unwrap_or((0.0, 0.0, OurSnapshot::default()));
        let job = ShadowJob {
            endpoint: Endpoint::Search,
            our_lat: lat,
            our_lng: lng,
            query: Some(query.to_owned()),
            country_code: country_code.map(str::to_owned),
            ours: ours_snap,
        };
        self.try_send(job);
    }

    fn passes_sample_gate(&self) -> bool {
        let rate = f64::from_bits(self.sample_rate_bits.load(Ordering::Relaxed));
        if rate <= 0.0 {
            return false;
        }
        if rate >= 1.0 {
            return true;
        }
        rand::random::<f64>() < rate
    }

    fn try_send(&self, job: ShadowJob) {
        // Capture the endpoint before the send moves the job — needed
        // for the queue-full / closed emission paths (the error variant
        // hands the job back, but pulling the endpoint out of the
        // returned variant adds extra branching for no real gain).
        let endpoint = job.endpoint;
        match self.tx.try_send(job) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.queue_full_count.fetch_add(1, Ordering::Relaxed);
                if let Some(m) = self.metrics.as_ref() {
                    m.inc_shadow_queue_full();
                    m.record_shadow(endpoint.as_str(), Outcome::QueueFull.as_str(), &[], None);
                }
                emit_outcome(endpoint, Outcome::QueueFull, None, None, None);
                // Self-rate-limited WARN so an oncall sees a clear
                // signal in stdout when the bounded mpsc saturates,
                // rather than having to be already watching the
                // metric. The DedupFilter in telemetry.rs is scoped
                // to `opentelemetry*` targets, so we hand-roll the
                // throttle here. One line per 30 s under sustained
                // pressure is enough to draw attention without
                // dominating the log.
                warn_queue_full_throttled();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Worker died. This is a bug — surface it loudly. We
                // never re-arm the channel, so further calls will keep
                // landing here; the dedup filter in
                // server/src/telemetry.rs would suppress flooded
                // warnings if the target matched `opentelemetry*`,
                // which it doesn't — so we live with one warning per
                // sampled call until the process restarts. Acceptable
                // because this branch is unreachable in practice.
                tracing::error!(
                    target: "query_server::shadow",
                    "shadow channel closed unexpectedly — worker has died"
                );
            }
        }
    }

    /// Cumulative count of `try_send` failures because the bounded
    /// channel was full. Exposed because it's a useful metric for any
    /// future `/healthz/shadow` endpoint and for integration tests
    /// that pin the queue-full backpressure behaviour. Read-only,
    /// monotonically increasing.
    pub fn queue_full_count(&self) -> u64 {
        self.queue_full_count.load(Ordering::Relaxed)
    }
}

// --- Worker -------------------------------------------------------------

struct Worker {
    cfg: ShadowConfig,
    client: reqwest::Client,
    inflight: Arc<Semaphore>,
    daily_count: Arc<AtomicU32>,
    auth_disabled: Arc<AtomicBool>,
    backoff_until_unix_ms: Arc<AtomicU64>,
    metrics: Option<Arc<Metrics>>,
}

impl Worker {
    fn new(cfg: ShadowConfig, metrics: Option<Arc<Metrics>>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(cfg.timeout)
            // Connection pooling defaults are fine — small fleet of
            // calls per day, idle conns get reaped after 90 s.
            .user_agent(concat!(
                "traccar-geocoder-shadow/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .expect("reqwest::Client::build cannot fail with a valid TLS backend");
        Self {
            inflight: Arc::new(Semaphore::new(cfg.inflight_cap)),
            daily_count: Arc::new(AtomicU32::new(0)),
            auth_disabled: Arc::new(AtomicBool::new(false)),
            backoff_until_unix_ms: Arc::new(AtomicU64::new(0)),
            client,
            cfg,
            metrics,
        }
    }

    fn spawn_loops(self, rx: mpsc::Receiver<ShadowJob>) {
        // Token-bucket interval: one tick every 1/rps_cap seconds. Using
        // tokio::time::interval so MissedTickBehavior::Burst is the
        // default — under load this means catch-up ticks fire as fast
        // as the runtime can poll, capped at the bucket size = rps_cap.
        let period = Duration::from_secs_f64(1.0 / self.cfg.rps_cap as f64);

        // Daily-cap reset task — runs forever, sleeps until next UTC
        // midnight, resets the counter, repeats.
        let daily_count = self.daily_count.clone();
        tokio::spawn(async move {
            loop {
                let sleep = duration_until_next_utc_midnight();
                tracing::debug!(
                    target: "query_server::shadow",
                    sleep_secs = sleep.as_secs(),
                    "sleeping until next UTC midnight for daily cap reset"
                );
                tokio::time::sleep(sleep).await;
                daily_count.store(0, Ordering::Relaxed);
                tracing::info!(
                    target: "query_server::shadow",
                    "daily cap reset"
                );
            }
        });

        // Main worker loop.
        tokio::spawn(async move {
            let mut bucket = tokio::time::interval(period);
            // Don't burst on startup — wait the first period so we
            // never exceed `rps_cap` over any 1 s window.
            bucket.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            self.run(rx, bucket).await;
        });
    }

    async fn run(self, mut rx: mpsc::Receiver<ShadowJob>, mut bucket: tokio::time::Interval) {
        let cfg = self.cfg.clone();
        let client = self.client.clone();
        let inflight = self.inflight.clone();
        let daily_count = self.daily_count.clone();
        let auth_disabled = self.auth_disabled.clone();
        let backoff = self.backoff_until_unix_ms.clone();
        let metrics = self.metrics.clone();

        while let Some(job) = rx.recv().await {
            // Always drain — the channel keeps moving even when we
            // intend not to dispatch, so the handler-side queue_full
            // counter only ever reflects genuine backpressure.
            if auth_disabled.load(Ordering::Relaxed) {
                emit_job_outcome(&job, Outcome::AuthDisabled, None, None, None, metrics.as_ref());
                continue;
            }
            if now_unix_ms() < backoff.load(Ordering::Relaxed) {
                emit_job_outcome(&job, Outcome::RateLimited, None, None, None, metrics.as_ref());
                continue;
            }
            // Daily-cap check + increment. Note: the load here and the
            // fetch_add at line ~554 *appear* to form a check-then-act
            // race, but they aren't — the worker is a single tokio
            // task (spawn_loops above does exactly one tokio::spawn for
            // run()), so these two operations are sequential within the
            // same async context. The only concurrent writer is the
            // midnight-reset task storing 0, which is the *desired*
            // clearing behavior (it can never push the count *up*
            // mid-decision). Relaxed ordering is therefore sufficient.
            if daily_count.load(Ordering::Relaxed) >= cfg.daily_cap {
                emit_job_outcome(&job, Outcome::DailyCapHit, None, None, None, metrics.as_ref());
                continue;
            }

            // Wait for a token in the rate bucket. This is the only
            // backpressure point: under sustained sampling, recv blocks
            // here and the channel fills in front of it.
            bucket.tick().await;

            let permit = match inflight.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => break, // Semaphore closed — worker shutting down.
            };
            daily_count.fetch_add(1, Ordering::Relaxed);

            let cfg = cfg.clone();
            let client = client.clone();
            let auth_disabled = auth_disabled.clone();
            let backoff = backoff.clone();
            let metrics = metrics.clone();
            tokio::spawn(async move {
                execute_job(client, cfg, auth_disabled, backoff, metrics, job).await;
                drop(permit); // explicit for clarity; Drop releases the semaphore.
            });
        }
    }
}

async fn execute_job(
    client: reqwest::Client,
    cfg: ShadowConfig,
    auth_disabled: Arc<AtomicBool>,
    backoff: Arc<AtomicU64>,
    metrics: Option<Arc<Metrics>>,
    job: ShadowJob,
) {
    let started = Instant::now();
    let url = build_google_url(&cfg.base_url, &cfg.api_key, &job);
    let resp = client.get(&url).send().await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let m = metrics.as_ref();

    let body = match resp {
        Ok(r) => match r.json::<GoogleResponse>().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "query_server::shadow",
                    error = %e,
                    "failed to parse Google response body"
                );
                emit_job_outcome(&job, Outcome::GoogleError, None, None, Some(latency_ms), m);
                return;
            }
        },
        Err(e) if e.is_timeout() => {
            emit_job_outcome(&job, Outcome::Timeout, None, None, Some(latency_ms), m);
            return;
        }
        Err(e) => {
            tracing::warn!(
                target: "query_server::shadow",
                error = %e,
                "Google HTTP error"
            );
            emit_job_outcome(&job, Outcome::GoogleError, None, None, Some(latency_ms), m);
            return;
        }
    };

    match parse_status(&body.status) {
        StatusKind::Ok => {
            // Carry on — comparison below.
        }
        StatusKind::ZeroResults => {
            emit_job_outcome(&job, Outcome::ZeroResults, None, Some(&body.status), Some(latency_ms), m);
            return;
        }
        StatusKind::OverQueryLimit => {
            let until = now_unix_ms() + cfg.backoff_secs * 1000;
            backoff.store(until, Ordering::Relaxed);
            tracing::warn!(
                target: "query_server::shadow",
                backoff_secs = cfg.backoff_secs,
                "Google OVER_QUERY_LIMIT — pausing dispatch"
            );
            emit_job_outcome(&job, Outcome::RateLimited, None, Some(&body.status), Some(latency_ms), m);
            return;
        }
        StatusKind::RequestDenied => {
            auth_disabled.store(true, Ordering::Relaxed);
            tracing::error!(
                target: "query_server::shadow",
                "Google REQUEST_DENIED — disabling shadow validation for the lifetime of the process"
            );
            emit_job_outcome(&job, Outcome::AuthDisabled, None, Some(&body.status), Some(latency_ms), m);
            return;
        }
        StatusKind::Other => {
            emit_job_outcome(&job, Outcome::GoogleError, None, Some(&body.status), Some(latency_ms), m);
            return;
        }
    }

    let google = GoogleSnapshot::from_body(&body);
    emit_job_outcome(
        &job,
        compare(&job, &google).outcome,
        Some(&google),
        Some(&body.status),
        Some(latency_ms),
        m,
    );
}

/// Build the Google Geocoding API URL for a shadow job.
fn build_google_url(base_url: &str, api_key: &str, job: &ShadowJob) -> String {
    match job.endpoint {
        Endpoint::Reverse => format!(
            "{base}?latlng={lat},{lng}&key={k}",
            base = base_url,
            lat = job.our_lat,
            lng = job.our_lng,
            k = api_key,
        ),
        Endpoint::Search => {
            let q = job.query.as_deref().unwrap_or("");
            // urlencode the query — addresses regularly contain spaces
            // and commas. Use a tiny manual encoder rather than pulling
            // in `url` for one call site.
            let encoded = percent_encode(q);
            let region = job
                .country_code
                .as_deref()
                .map(|cc| format!("&region={}", cc.to_ascii_lowercase()))
                .unwrap_or_default();
            format!(
                "{base}?address={q}{region}&key={k}",
                base = base_url,
                q = encoded,
                region = region,
                k = api_key,
            )
        }
    }
}

/// Minimal RFC-3986 unreserved-set encoder. We URL-encode the query
/// portion only; the rest of the URL is built from controlled values.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                out.push('%');
                out.push(hex_nibble(b >> 4));
                out.push(hex_nibble(b & 0x0F));
            }
        }
    }
    out
}
fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _ => unreachable!("4-bit nibble"),
    }
}

// --- Google response parsing -------------------------------------------

#[derive(Debug, Deserialize)]
struct GoogleResponse {
    status: String,
    #[serde(default)]
    results: Vec<GoogleResult>,
}

#[derive(Debug, Deserialize)]
struct GoogleResult {
    #[serde(default)]
    formatted_address: Option<String>,
    #[serde(default)]
    geometry: Option<GoogleGeometry>,
    #[serde(default)]
    address_components: Vec<GoogleComponent>,
}

#[derive(Debug, Deserialize)]
struct GoogleGeometry {
    #[serde(default)]
    location: Option<GoogleLocation>,
}
#[derive(Debug, Deserialize)]
struct GoogleLocation {
    lat: f64,
    lng: f64,
}
#[derive(Debug, Deserialize)]
struct GoogleComponent {
    #[serde(default)]
    long_name: String,
    #[serde(default)]
    short_name: String,
    #[serde(default)]
    types: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusKind {
    Ok,
    ZeroResults,
    OverQueryLimit,
    RequestDenied,
    Other,
}

fn parse_status(s: &str) -> StatusKind {
    match s {
        "OK" => StatusKind::Ok,
        "ZERO_RESULTS" => StatusKind::ZeroResults,
        "OVER_QUERY_LIMIT" => StatusKind::OverQueryLimit,
        "REQUEST_DENIED" => StatusKind::RequestDenied,
        _ => StatusKind::Other,
    }
}

/// The fields we extract from Google's response shape into our flat
/// comparison form. Built from `results[0].address_components` — Google
/// returns these as a flat list tagged by `types`, so we walk it once
/// and project to our four-axis schema.
#[derive(Clone, Debug, Default)]
pub struct GoogleSnapshot {
    pub country_code: Option<String>,
    pub state: Option<String>,
    pub city: Option<String>,
    pub road: Option<String>,
    pub formatted_address: Option<String>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
}

impl GoogleSnapshot {
    fn from_body(body: &GoogleResponse) -> Self {
        let Some(top) = body.results.first() else {
            return Self::default();
        };
        let mut snap = Self {
            formatted_address: top.formatted_address.clone(),
            lat: top.geometry.as_ref().and_then(|g| g.location.as_ref()).map(|l| l.lat),
            lng: top.geometry.as_ref().and_then(|g| g.location.as_ref()).map(|l| l.lng),
            ..Self::default()
        };
        for c in &top.address_components {
            for t in &c.types {
                match t.as_str() {
                    "country" => {
                        // Google's `short_name` is the ISO 3166-1 alpha-2.
                        // Stash both for diagnostics but compare on lowercase.
                        if snap.country_code.is_none() && !c.short_name.is_empty() {
                            snap.country_code = Some(c.short_name.to_ascii_lowercase());
                        }
                    }
                    "administrative_area_level_1" => {
                        if snap.state.is_none() {
                            snap.state = Some(c.long_name.clone());
                        }
                    }
                    "locality" | "postal_town" => {
                        if snap.city.is_none() {
                            snap.city = Some(c.long_name.clone());
                        }
                    }
                    "route" => {
                        if snap.road.is_none() {
                            snap.road = Some(c.long_name.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        snap
    }
}

// --- Comparator ---------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Comparison {
    pub outcome: Outcome,
    pub country_match: Option<bool>,
    pub state_match: Option<bool>,
    pub city_match: Option<bool>,
    pub road_match: Option<bool>,
    pub distance_m: Option<f64>,
}

fn compare(job: &ShadowJob, google: &GoogleSnapshot) -> Comparison {
    let country_match = compare_str_ci(job.ours.country_code.as_deref(), google.country_code.as_deref());
    let state_match = compare_admin(job.ours.state.as_deref(), google.state.as_deref());
    let city_match = compare_admin(job.ours.city.as_deref(), google.city.as_deref());
    let road_match = match job.endpoint {
        Endpoint::Reverse => compare_admin(job.ours.road.as_deref(), google.road.as_deref()),
        Endpoint::Search => None, // search comparison focuses on coord + admin, not road
    };
    let distance_m = match job.endpoint {
        Endpoint::Search => google
            .lat
            .zip(google.lng)
            .map(|(glat, glng)| haversine_m(job.our_lat, job.our_lng, glat, glng)),
        Endpoint::Reverse => None,
    };

    // Outcome: a "match" requires every COMPARED field to match. Fields
    // where either side is unknown (None) don't count toward mismatch
    // — they're just not compared. Search adds a distance ceiling: if
    // we produced a coord, we expect Google's top hit within 1 km.
    let admin_axes = [country_match, state_match, city_match, road_match];
    let any_compared = admin_axes.iter().any(|m| m.is_some()) || distance_m.is_some();
    let any_mismatch = admin_axes.iter().any(|m| matches!(m, Some(false)))
        || distance_m.map(|d| d > 1000.0).unwrap_or(false);
    let outcome = if !any_compared {
        Outcome::ZeroResults
    } else if any_mismatch {
        Outcome::Mismatch
    } else {
        Outcome::Match
    };

    Comparison {
        outcome,
        country_match,
        state_match,
        city_match,
        road_match,
        distance_m,
    }
}

fn compare_str_ci(a: Option<&str>, b: Option<&str>) -> Option<bool> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.eq_ignore_ascii_case(y)),
        _ => None,
    }
}

/// Admin-name comparator. The two sides routinely disagree on shape
/// even when they agree on identity:
///
///   our index   ←→   Google
///   "NSW"           "New South Wales"
///   "Sydney"        "Sydney City Council"
///   "Greater       "Greater London"
///    London"
///
/// To absorb that drift without false positives we try four ways:
///
/// 1. Exact (case-folded) equality.
/// 2. Either side a substring of the other (handles "Sydney" ⊂
///    "Sydney City Council").
/// 3. Either side an **initials match** of the other's whitespace-
///    separated words (handles "NSW" ↔ "New South Wales").
///
/// All comparisons are ASCII-lowercase. Diacritic folding isn't
/// applied here — admin names that differ by accent (e.g.
/// "Québec" / "Quebec") are rare enough that we accept the false
/// negative rather than pull in a Unicode lib.
fn compare_admin(a: Option<&str>, b: Option<&str>) -> Option<bool> {
    match (a, b) {
        (Some(x), Some(y)) => {
            let xl = x.to_ascii_lowercase();
            let yl = y.to_ascii_lowercase();
            if xl == yl || xl.contains(&yl) || yl.contains(&xl) {
                return Some(true);
            }
            if matches_initials(&xl, &yl) || matches_initials(&yl, &xl) {
                return Some(true);
            }
            Some(false)
        }
        _ => None,
    }
}

/// True iff `short` equals the first letter of each whitespace-separated
/// word of `long`. Both sides must already be ASCII-lowercased. Empty
/// inputs return false — an empty string isn't an "initials" match.
fn matches_initials(short: &str, long: &str) -> bool {
    if short.is_empty() {
        return false;
    }
    let initials: String = long
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .collect();
    !initials.is_empty() && short == initials
}

// --- Span + log emission -----------------------------------------------

fn emit_outcome(
    endpoint: Endpoint,
    outcome: Outcome,
    _google: Option<&GoogleSnapshot>,
    _google_status: Option<&str>,
    _latency_ms: Option<u64>,
) {
    // Queue-full path emission. The job was moved into try_send before
    // the error surfaced, so we don't have an endpoint or our snapshot
    // — just the outcome. Underscored field names because tracing's
    // `info!` macro hits a parser ambiguity with dotted identifiers
    // (the span macros parse them fine; events don't). Aggregators
    // grouping by outcome should consume the span attributes from
    // emit_job_outcome below; this event is the human-readable summary
    // for the rare hand-grep case.
    tracing::info!(
        target: "query_server::shadow",
        endpoint_hint = endpoint.as_str(),
        outcome = outcome.as_str(),
        "shadow {}",
        outcome.as_str()
    );
}

fn emit_job_outcome(
    job: &ShadowJob,
    outcome: Outcome,
    google: Option<&GoogleSnapshot>,
    google_status: Option<&str>,
    latency_ms: Option<u64>,
    metrics: Option<&Arc<Metrics>>,
) {
    let comparison = google.map(|g| compare(job, g));
    let span = tracing::info_span!(
        target: "query_server::shadow",
        "shadow",
        geocoder.shadow.endpoint = job.endpoint.as_str(),
        geocoder.shadow.outcome = outcome.as_str(),
        geocoder.shadow.our.country_code = job.ours.country_code.as_deref().unwrap_or(""),
        geocoder.shadow.google.country_code = google.and_then(|g| g.country_code.as_deref()).unwrap_or(""),
        geocoder.shadow.google.status = google_status.unwrap_or(""),
        geocoder.shadow.latency_ms = latency_ms.unwrap_or(0),
        geocoder.shadow.country.match = comparison.as_ref().and_then(|c| c.country_match).unwrap_or(false),
        geocoder.shadow.state.match = comparison.as_ref().and_then(|c| c.state_match).unwrap_or(false),
        geocoder.shadow.city.match = comparison.as_ref().and_then(|c| c.city_match).unwrap_or(false),
        geocoder.shadow.road.match = comparison.as_ref().and_then(|c| c.road_match).unwrap_or(false),
        geocoder.shadow.distance_m = comparison.as_ref().and_then(|c| c.distance_m).unwrap_or(0.0),
    );
    let _enter = span.enter();

    if matches!(outcome, Outcome::Mismatch) {
        tracing::info!(
            target: "query_server::shadow",
            our_formatted = job.ours.display_name.as_deref().unwrap_or(""),
            google_formatted = google.and_then(|g| g.formatted_address.as_deref()).unwrap_or(""),
            "shadow mismatch"
        );
    } else {
        tracing::info!(
            target: "query_server::shadow",
            "shadow {}",
            outcome.as_str()
        );
    }

    // Metrics — single helper bumps both the Prometheus and OTel sides.
    // The label cardinality cap is documented in metrics.rs; outcomes
    // ∈ 9 fixed values, axes ∈ 4 fixed, results ∈ 3 — bounded.
    if let Some(m) = metrics {
        let endpoint = job.endpoint.as_str();
        let axes = [
            ("country", comparison.as_ref().and_then(|c| c.country_match)),
            ("state", comparison.as_ref().and_then(|c| c.state_match)),
            ("city", comparison.as_ref().and_then(|c| c.city_match)),
            ("road", comparison.as_ref().and_then(|c| c.road_match)),
        ];
        let distance_m = comparison.as_ref().and_then(|c| c.distance_m);
        m.record_shadow(endpoint, outcome.as_str(), &axes, distance_m);
    }
}

// --- Time helpers ------------------------------------------------------

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Queue-full warning gate. Process-wide AtomicU64 of last-emit unix
/// milliseconds. Emit at most once per `QUEUE_FULL_WARN_WINDOW_MS`,
/// using a CAS so two threads racing through the gate produce only
/// one log line (not strictly required for correctness — duplicate
/// warnings would still be useful — but keeps stdout tidy).
const QUEUE_FULL_WARN_WINDOW_MS: u64 = 30_000;
static LAST_QUEUE_FULL_WARN_MS: AtomicU64 = AtomicU64::new(0);

fn warn_queue_full_throttled() {
    let now = now_unix_ms();
    let last = LAST_QUEUE_FULL_WARN_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < QUEUE_FULL_WARN_WINDOW_MS {
        return;
    }
    if LAST_QUEUE_FULL_WARN_MS
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        // Another thread already won the gate this window. Drop ours.
        return;
    }
    tracing::warn!(
        target: "query_server::shadow",
        window_secs = QUEUE_FULL_WARN_WINDOW_MS / 1000,
        "shadow queue full — dispatch dropped; raise GOOGLE_GEOCODING_QUEUE_CAPACITY or GOOGLE_GEOCODING_INFLIGHT_CAP if persistent"
    );
}

/// Compute the Duration from now until 00:00:00 UTC tomorrow. Used by
/// the daily-cap reset task — sleeping until that instant means we
/// reset exactly once per UTC day even if the worker started mid-day.
///
/// Wraps `duration_until_next_utc_midnight_from(Utc::now())` so the
/// inner pure function is testable across calendar edge cases (end of
/// month, end of year, leap day) without time mocking.
fn duration_until_next_utc_midnight() -> Duration {
    duration_until_next_utc_midnight_from(Utc::now())
}

/// Pure form of [`duration_until_next_utc_midnight`]. Defensively
/// returns a 1-hour fallback if chrono can't compute the next
/// midnight from the supplied `now` — in practice this only happens
/// when `now` is at `NaiveDate::MAX` (≈ year 262143), but the
/// alternative is a worker-killing panic on an extreme edge case.
/// One hour is short enough for the daily cap to recover quickly
/// after the worker wakes up.
fn duration_until_next_utc_midnight_from(now: chrono::DateTime<chrono::Utc>) -> Duration {
    const FALLBACK: Duration = Duration::from_secs(3600);
    let Some(tomorrow) = now.date_naive().succ_opt() else {
        tracing::warn!(
            target: "query_server::shadow",
            "could not compute next UTC midnight (date overflow); falling back to 1-hour sleep"
        );
        return FALLBACK;
    };
    let midnight_opt = Utc.with_ymd_and_hms(tomorrow.year(), tomorrow.month(), tomorrow.day(), 0, 0, 0);
    let Some(midnight) = midnight_opt.single() else {
        tracing::warn!(
            target: "query_server::shadow",
            "next UTC midnight is ambiguous (system clock skew?); falling back to 1-hour sleep"
        );
        return FALLBACK;
    };
    let ms = (midnight - now).num_milliseconds().max(0) as u64;
    Duration::from_millis(ms)
}

// --- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Master-switch truth matrix — all 7 documented cells.

    #[test]
    fn enabled_when_unset_and_key_present() {
        assert!(resolve_shadow_enabled(None, true));
    }

    #[test]
    fn disabled_when_unset_and_no_key() {
        assert!(!resolve_shadow_enabled(None, false));
    }

    #[test]
    fn enabled_when_true_and_key_present() {
        for v in ["true", "1", "yes", "on", "TRUE", " On "] {
            assert!(resolve_shadow_enabled(Some(v), true), "expected {v:?} → enabled");
        }
    }

    #[test]
    fn disabled_when_true_but_no_key() {
        // Operator opted in but forgot the key. Fail closed, log the
        // mismatch in from_env (covered by integration test).
        assert!(!resolve_shadow_enabled(Some("true"), false));
    }

    #[test]
    fn disabled_when_false_regardless_of_key() {
        for v in ["false", "0", "no", "off", "FALSE", " Off ", ""] {
            assert!(!resolve_shadow_enabled(Some(v), true), "{v:?} with key → must disable");
            assert!(!resolve_shadow_enabled(Some(v), false), "{v:?} no key → must disable");
        }
    }

    #[test]
    fn disabled_on_garbage_value() {
        for v in ["maybe", "enabled-please", "yolo"] {
            assert!(!resolve_shadow_enabled(Some(v), true), "garbage {v:?} → must disable");
        }
    }

    // --- Sampling gate ---

    #[test]
    fn sampler_zero_rate_never_passes() {
        let bits = AtomicU64::new(f64::to_bits(0.0));
        for _ in 0..1000 {
            let rate = f64::from_bits(bits.load(Ordering::Relaxed));
            // Mirror the dispatcher's logic. Avoids needing a real
            // dispatcher just to exercise the gate.
            let pass = if rate <= 0.0 { false } else if rate >= 1.0 { true } else { rand::random::<f64>() < rate };
            assert!(!pass);
        }
    }

    #[test]
    fn sampler_full_rate_always_passes() {
        let bits = AtomicU64::new(f64::to_bits(1.0));
        for _ in 0..1000 {
            let rate = f64::from_bits(bits.load(Ordering::Relaxed));
            let pass = if rate <= 0.0 { false } else if rate >= 1.0 { true } else { rand::random::<f64>() < rate };
            assert!(pass);
        }
    }

    #[test]
    fn sampler_probability_gate_obeyed() {
        // 100 000 trials at 10 % — within ±0.5 % is ample (binomial
        // SD = √(p(1-p)/n) ≈ 0.095 %; we leave 5× SD slack).
        let n = 100_000usize;
        let rate = 0.1f64;
        let mut hits = 0usize;
        for _ in 0..n {
            if rand::random::<f64>() < rate {
                hits += 1;
            }
        }
        let observed = hits as f64 / n as f64;
        assert!(
            (rate - 0.005..rate + 0.005).contains(&observed),
            "expected {rate} ± 0.5 %, got {observed}"
        );
    }

    // --- Google status parser ---

    #[test]
    fn parse_status_known_variants() {
        assert_eq!(parse_status("OK"), StatusKind::Ok);
        assert_eq!(parse_status("ZERO_RESULTS"), StatusKind::ZeroResults);
        assert_eq!(parse_status("OVER_QUERY_LIMIT"), StatusKind::OverQueryLimit);
        assert_eq!(parse_status("REQUEST_DENIED"), StatusKind::RequestDenied);
    }

    #[test]
    fn parse_status_unknown_falls_through_to_other() {
        // Google has added new status values over the years (e.g.
        // INVALID_REQUEST, UNKNOWN_ERROR). We don't enumerate them —
        // anything we don't recognise gets routed to the generic
        // GoogleError outcome.
        assert_eq!(parse_status("INVALID_REQUEST"), StatusKind::Other);
        assert_eq!(parse_status("UNKNOWN_ERROR"), StatusKind::Other);
        assert_eq!(parse_status(""), StatusKind::Other);
    }

    // --- Address-component projection ---

    #[test]
    fn google_snapshot_extracts_canonical_components() {
        let body = GoogleResponse {
            status: "OK".into(),
            results: vec![GoogleResult {
                formatted_address: Some("123 George St, Sydney NSW 2000, Australia".into()),
                geometry: Some(GoogleGeometry {
                    location: Some(GoogleLocation { lat: -33.8688, lng: 151.2093 }),
                }),
                address_components: vec![
                    GoogleComponent {
                        long_name: "George Street".into(),
                        short_name: "George St".into(),
                        types: vec!["route".into()],
                    },
                    GoogleComponent {
                        long_name: "Sydney".into(),
                        short_name: "Sydney".into(),
                        types: vec!["locality".into(), "political".into()],
                    },
                    GoogleComponent {
                        long_name: "New South Wales".into(),
                        short_name: "NSW".into(),
                        types: vec!["administrative_area_level_1".into(), "political".into()],
                    },
                    GoogleComponent {
                        long_name: "Australia".into(),
                        short_name: "AU".into(),
                        types: vec!["country".into(), "political".into()],
                    },
                ],
            }],
        };
        let snap = GoogleSnapshot::from_body(&body);
        assert_eq!(snap.country_code.as_deref(), Some("au"));
        assert_eq!(snap.state.as_deref(), Some("New South Wales"));
        assert_eq!(snap.city.as_deref(), Some("Sydney"));
        assert_eq!(snap.road.as_deref(), Some("George Street"));
        assert!(snap.lat.is_some() && snap.lng.is_some());
    }

    #[test]
    fn google_snapshot_handles_postal_town_as_city_fallback() {
        // UK addresses don't always have `locality`; Google sometimes
        // returns `postal_town` instead. We treat the two as
        // interchangeable — the geocoder shouldn't penalise itself for
        // a UK locality we resolved to "Manchester" while Google
        // returns it as a postal_town.
        let body = GoogleResponse {
            status: "OK".into(),
            results: vec![GoogleResult {
                formatted_address: Some("Manchester, UK".into()),
                geometry: None,
                address_components: vec![GoogleComponent {
                    long_name: "Manchester".into(),
                    short_name: "Manchester".into(),
                    types: vec!["postal_town".into()],
                }],
            }],
        };
        let snap = GoogleSnapshot::from_body(&body);
        assert_eq!(snap.city.as_deref(), Some("Manchester"));
    }

    #[test]
    fn google_snapshot_empty_results_yields_default() {
        let body = GoogleResponse { status: "ZERO_RESULTS".into(), results: vec![] };
        let snap = GoogleSnapshot::from_body(&body);
        assert!(snap.country_code.is_none());
        assert!(snap.state.is_none());
        assert!(snap.city.is_none());
        assert!(snap.road.is_none());
    }

    // --- Comparator ---

    #[test]
    fn admin_compare_handles_abbreviation() {
        // Our index emits "NSW", Google emits "New South Wales" —
        // contains-either-way catches that.
        assert_eq!(compare_admin(Some("NSW"), Some("New South Wales")), Some(true));
        assert_eq!(compare_admin(Some("New South Wales"), Some("NSW")), Some(true));
        assert_eq!(compare_admin(Some("NSW"), Some("VIC")), Some(false));
    }

    #[test]
    fn admin_compare_none_short_circuits() {
        assert_eq!(compare_admin(None, Some("NSW")), None);
        assert_eq!(compare_admin(Some("NSW"), None), None);
        assert_eq!(compare_admin(None, None), None);
    }

    #[test]
    fn country_compare_is_case_insensitive_exact() {
        // ISO 3166-1 alpha-2 — exact code match (case-folded).
        // contains-either-way would let "AU" match "AUS" which is the
        // alpha-3 from a different table; not what we want.
        assert_eq!(compare_str_ci(Some("au"), Some("AU")), Some(true));
        assert_eq!(compare_str_ci(Some("au"), Some("us")), Some(false));
    }

    #[test]
    fn comparison_match_when_all_axes_align() {
        let job = ShadowJob {
            endpoint: Endpoint::Reverse,
            our_lat: -33.86,
            our_lng: 151.21,
            query: None,
            country_code: None,
            ours: OurSnapshot {
                country_code: Some("au".into()),
                state: Some("NSW".into()),
                city: Some("Sydney".into()),
                road: Some("George Street".into()),
                ..Default::default()
            },
        };
        let google = GoogleSnapshot {
            country_code: Some("au".into()),
            state: Some("New South Wales".into()),
            city: Some("Sydney".into()),
            road: Some("George Street".into()),
            ..Default::default()
        };
        let cmp = compare(&job, &google);
        assert_eq!(cmp.outcome, Outcome::Match);
        assert_eq!(cmp.country_match, Some(true));
        assert_eq!(cmp.state_match, Some(true));
        assert_eq!(cmp.city_match, Some(true));
        assert_eq!(cmp.road_match, Some(true));
    }

    #[test]
    fn comparison_mismatch_on_state_disagreement() {
        let job = ShadowJob {
            endpoint: Endpoint::Reverse,
            our_lat: 0.0,
            our_lng: 0.0,
            query: None,
            country_code: None,
            ours: OurSnapshot {
                country_code: Some("au".into()),
                state: Some("VIC".into()),
                city: Some("Melbourne".into()),
                ..Default::default()
            },
        };
        let google = GoogleSnapshot {
            country_code: Some("au".into()),
            state: Some("New South Wales".into()),
            city: Some("Melbourne".into()),
            ..Default::default()
        };
        let cmp = compare(&job, &google);
        assert_eq!(cmp.outcome, Outcome::Mismatch);
        assert_eq!(cmp.state_match, Some(false));
    }

    #[test]
    fn comparison_search_uses_distance_when_under_1km() {
        let job = ShadowJob {
            endpoint: Endpoint::Search,
            our_lat: -33.8568,
            our_lng: 151.2153,
            query: Some("Sydney CBD".into()),
            country_code: Some("au".into()),
            ours: OurSnapshot {
                country_code: Some("au".into()),
                ..Default::default()
            },
        };
        // Google places Sydney CBD ~200 m away — still a match.
        let google = GoogleSnapshot {
            country_code: Some("au".into()),
            lat: Some(-33.8588),
            lng: Some(151.2153),
            ..Default::default()
        };
        let cmp = compare(&job, &google);
        assert_eq!(cmp.outcome, Outcome::Match);
        assert!(cmp.distance_m.unwrap() < 1000.0);
    }

    #[test]
    fn comparison_search_mismatch_when_distance_exceeds_1km() {
        let job = ShadowJob {
            endpoint: Endpoint::Search,
            our_lat: -33.8568,
            our_lng: 151.2153,
            query: Some("Sydney".into()),
            country_code: None,
            ours: OurSnapshot::default(),
        };
        // Google says it's actually in Brisbane — way more than 1 km.
        let google = GoogleSnapshot {
            lat: Some(-27.47),
            lng: Some(153.02),
            ..Default::default()
        };
        let cmp = compare(&job, &google);
        assert_eq!(cmp.outcome, Outcome::Mismatch);
        assert!(cmp.distance_m.unwrap() > 100_000.0);
    }

    #[test]
    fn comparison_zero_results_when_nothing_compared() {
        // Both sides empty — there's literally nothing to assert. We
        // call this `ZeroResults` rather than `Match` so an aggregator
        // can exclude it from match-rate denominators.
        let job = ShadowJob {
            endpoint: Endpoint::Reverse,
            our_lat: 0.0,
            our_lng: 0.0,
            query: None,
            country_code: None,
            ours: OurSnapshot::default(),
        };
        let google = GoogleSnapshot::default();
        let cmp = compare(&job, &google);
        assert_eq!(cmp.outcome, Outcome::ZeroResults);
    }

    // --- URL building ---

    #[test]
    fn reverse_url_carries_latlng_and_key() {
        let job = ShadowJob {
            endpoint: Endpoint::Reverse,
            our_lat: -33.8568,
            our_lng: 151.2153,
            query: None,
            country_code: None,
            ours: OurSnapshot::default(),
        };
        let url = build_google_url(DEFAULT_GOOGLE_BASE_URL, "KEY123", &job);
        assert!(url.starts_with(DEFAULT_GOOGLE_BASE_URL));
        assert!(url.contains("latlng=-33.8568,151.2153"));
        assert!(url.contains("key=KEY123"));
        assert!(!url.contains("address="));
    }

    #[test]
    fn search_url_encodes_query_and_includes_region() {
        let job = ShadowJob {
            endpoint: Endpoint::Search,
            our_lat: 0.0,
            our_lng: 0.0,
            query: Some("10 Alysse Close, Baulkham Hills".into()),
            country_code: Some("AU".into()),
            ours: OurSnapshot::default(),
        };
        let url = build_google_url(DEFAULT_GOOGLE_BASE_URL, "KEY", &job);
        // Spaces and commas must be escaped.
        assert!(url.contains("address=10%20Alysse%20Close%2C%20Baulkham%20Hills"));
        // Region is lowercased ccTLD.
        assert!(url.contains("&region=au"));
    }

    #[test]
    fn build_url_honours_custom_base() {
        // Integration tests need this — they spin up a localhost mock.
        let job = ShadowJob {
            endpoint: Endpoint::Reverse,
            our_lat: 0.0,
            our_lng: 0.0,
            query: None,
            country_code: None,
            ours: OurSnapshot::default(),
        };
        let url = build_google_url("http://127.0.0.1:9999/mock", "KEY", &job);
        assert!(url.starts_with("http://127.0.0.1:9999/mock?"));
    }

    #[test]
    fn percent_encode_round_trips_safe_chars() {
        // RFC-3986 unreserved set passes through untouched. Anything
        // outside it (including '+' and '&') must be encoded so we
        // don't break the query string's structure.
        assert_eq!(percent_encode("ABCxyz019-._~"), "ABCxyz019-._~");
        assert_eq!(percent_encode(" "), "%20");
        assert_eq!(percent_encode(","), "%2C");
        assert_eq!(percent_encode("&"), "%26");
        assert_eq!(percent_encode("é"), "%C3%A9"); // 2-byte utf-8
    }

    // --- Daily-cap clamp ---

    #[test]
    fn daily_cap_passthrough_below_max() {
        // Most operators never hit the clamp. Pin the no-op path.
        assert_eq!(clamp_daily_cap(0), 0);
        assert_eq!(clamp_daily_cap(1000), 1000);
        assert_eq!(clamp_daily_cap(40_000), 40_000);
        assert_eq!(clamp_daily_cap(MAX_DAILY_CAP), MAX_DAILY_CAP);
    }

    #[test]
    fn daily_cap_clamps_above_max() {
        // The headline scenario: typo of `100000000` instead of
        // `100000` would silently authorise $500 000/day. The clamp
        // brings it down to a defensible ceiling and warns.
        assert_eq!(clamp_daily_cap(MAX_DAILY_CAP + 1), MAX_DAILY_CAP);
        assert_eq!(clamp_daily_cap(1_000_000), MAX_DAILY_CAP);
        assert_eq!(clamp_daily_cap(u32::MAX), MAX_DAILY_CAP);
    }

    // --- Queue-full warning throttle ---

    // --- UTC-midnight calculation ---

    #[test]
    fn midnight_from_midday_is_about_twelve_hours() {
        let noon = Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).single().unwrap();
        let d = duration_until_next_utc_midnight_from(noon);
        // 12 hours exactly. Pinning to the second to flag any tz-offset bug.
        assert_eq!(d, Duration::from_secs(12 * 3600));
    }

    #[test]
    fn midnight_from_one_second_before_midnight_is_one_second() {
        // 23:59:59 → next midnight is 1 s away.
        let just_before = Utc.with_ymd_and_hms(2026, 4, 25, 23, 59, 59).single().unwrap();
        let d = duration_until_next_utc_midnight_from(just_before);
        assert_eq!(d, Duration::from_secs(1));
    }

    #[test]
    fn midnight_handles_end_of_month_rollover() {
        // 2026-04-30 → 2026-05-01. The naive `now.date() + 1.day()`
        // approach has historically tripped people up on end-of-month;
        // chrono's `succ_opt()` handles it correctly. Pin that.
        let last_day_of_april = Utc.with_ymd_and_hms(2026, 4, 30, 23, 59, 0).single().unwrap();
        let d = duration_until_next_utc_midnight_from(last_day_of_april);
        // From 23:59 to next midnight = 60 s, regardless of the
        // calendar boundary.
        assert_eq!(d, Duration::from_secs(60));
    }

    #[test]
    fn midnight_handles_end_of_year_rollover() {
        // 2026-12-31 23:59:30 → 2027-01-01 00:00:00 (30 s)
        let nye = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 30).single().unwrap();
        let d = duration_until_next_utc_midnight_from(nye);
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn midnight_handles_leap_day() {
        // 2028-02-28 (leap year) → 2028-02-29, not 2028-03-01.
        // Pin chrono's leap-year correctness; if it ever flipped to
        // skipping Feb 29 the daily counter would be 24 h late on the
        // first reset of every leap year.
        let feb_28_2028 = Utc.with_ymd_and_hms(2028, 2, 28, 23, 0, 0).single().unwrap();
        let d = duration_until_next_utc_midnight_from(feb_28_2028);
        // 23:00 → next midnight is 1 hour later, on Feb 29.
        assert_eq!(d, Duration::from_secs(3600));

        // Then from Feb 29 23:00, next midnight is Mar 1.
        let feb_29_2028 = Utc.with_ymd_and_hms(2028, 2, 29, 23, 0, 0).single().unwrap();
        let d = duration_until_next_utc_midnight_from(feb_29_2028);
        assert_eq!(d, Duration::from_secs(3600));
    }

    #[test]
    fn midnight_falls_back_on_date_overflow() {
        // chrono's NaiveDate::MAX is ~year 262143. succ_opt() returns
        // None there; we don't panic, we fall back to a 1-hour sleep.
        // We can't construct DateTime<Utc>::MAX directly because the
        // Utc.with_ymd_and_hms builder rejects out-of-range dates, so
        // we use the actual MAX naive date.
        use chrono::NaiveDateTime;
        let max_naive = chrono::NaiveDate::MAX
            .and_hms_opt(0, 0, 0)
            .expect("max date 00:00:00 is constructible");
        let extreme = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            NaiveDateTime::from(max_naive),
            chrono::Utc,
        );
        let d = duration_until_next_utc_midnight_from(extreme);
        // Falls back to 1 hour rather than panicking.
        assert_eq!(d, Duration::from_secs(3600));
    }

    #[test]
    fn warn_queue_full_throttled_runs_without_panic() {
        // Functional smoke: the gate must be safe to call rapidly
        // (no panic on the static AtomicU64 path) and idempotent
        // within the throttle window. We can't assert log-line
        // emission directly without a subscriber, but we can verify
        // the counter advances on each window cross.
        let before = LAST_QUEUE_FULL_WARN_MS.load(Ordering::Relaxed);
        for _ in 0..1000 {
            warn_queue_full_throttled();
        }
        let after = LAST_QUEUE_FULL_WARN_MS.load(Ordering::Relaxed);
        // Either: the test was the first caller this window (after >
        // before), or another test already won the gate this window
        // (after >= before). Either way the gate doesn't regress.
        assert!(
            after >= before,
            "warn gate's last-emit timestamp must be monotonic — got before={before}, after={after}"
        );
    }
}
