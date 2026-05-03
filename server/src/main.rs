// mimalloc as the global allocator. Outperforms system malloc on
// tantivy postings construction, FST builds, hashmap rehashes, and
// per-request response allocation. See
// docs/performance/build-pipeline-perf-plan.md stage 2.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use arc_swap::ArcSwap;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use query_server::admin_config::AdminConfig;
use query_server::autocomplete::Autocomplete;
use query_server::ip_geo::IpGeo;
use query_server::metrics::{canonical_country, Metrics};
use query_server::shadow::{ShadowConfig, ShadowDispatcher};
use query_server::telemetry;
use query_server::{Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tower_http::trace::{DefaultOnFailure, TraceLayer};
use tracing::Level;

#[cfg(feature = "forward")]
use query_server::forward::{self, Forward};

/// Live index handle. Handlers clone the outer Arc (cheap) and call
/// `.load()` to get the current inner `Arc<Index>`. A background task can
/// replace the inner value atomically when the index on disk changes.
type LiveIndex = Arc<ArcSwap<Index>>;

#[derive(Deserialize)]
struct QueryParams {
    lat: f64,
    lon: f64,
    /// Optional language code (ISO 639-1: `en`, `fr`, `de`, `es`, `ja`,
    /// `zh`, etc.). When present and the underlying entity has a matching
    /// `name:<lang>` tag in OSM, the returned city/state/country names are
    /// in that language. When the C++ builder has not yet emitted the
    /// i18n index (see ARCHITECTURE.md `Future work`), this param is
    /// silently accepted and ignored — clients can ship the param
    /// unconditionally without breaking.
    #[serde(default)]
    lang: Option<String>,
    /// Optional comma-separated H3 resolutions (0–15, max 4). When set,
    /// the response includes an `h3` map keyed by resolution. Rejects
    /// invalid input with HTTP 400 rather than silently dropping it;
    /// a typo in the query param is a bug to surface, not to swallow.
    #[serde(default)]
    h3_res: Option<String>,
}

#[derive(Deserialize)]
struct AutocompleteParams {
    q: String,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    h3_res: Option<String>,
}

#[derive(Deserialize)]
struct ValidateParams {
    /// Optional — when present, resolves the specific property via the
    /// address-point indexes and returns `verified` confidence.
    #[serde(default)]
    housenumber: Option<String>,
    street: String,
    /// Locality / suburb / city. Required.
    city: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    postcode: Option<String>,
    country_code: String,
    #[serde(default)]
    h3_res: Option<String>,
}

#[derive(Deserialize)]
struct IpParams {
    /// Optional override — defaults to the request's peer IP. Accepts
    /// either IPv4 or IPv6 in the usual textual form.
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    h3_res: Option<String>,
}

/// `/h3` query — pure (lat, lon) → H3 cell-map computation, no
/// reverse-geocode. The `h3_res` field is required here (unlike the
/// enrichment field on other endpoints): a request without resolutions
/// has no work to do, and silently 200-ing an empty body would be
/// confusing.
#[derive(Deserialize)]
struct H3Params {
    lat: f64,
    lon: f64,
    h3_res: String,
}

/// Hard-radius "what's near me" search. Distinct contract from
/// `/search`: `lat`/`lng`/`radius_km` are required, results are sorted
/// by distance ascending, and anything outside the radius is dropped
/// (not soft-penalised). Optional `q` and `kind` narrow the candidates
/// inside the radius.
#[cfg(feature = "forward")]
#[derive(Deserialize)]
struct NearbyParams {
    lat: f64,
    lng: f64,
    radius_km: f64,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    h3_res: Option<String>,
}

#[cfg(feature = "forward")]
#[derive(Deserialize)]
struct SearchParams {
    /// Freeform query. Either `q` alone or any of the structured fields below
    /// (or both) can be provided.
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    street: Option<String>,
    #[serde(default)]
    housenumber: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    h3_res: Option<String>,
    /// Optional proximity bias for ambiguous-name disambiguation.
    /// `bias_lat` and `bias_lng` must be supplied together. When set,
    /// hits are re-ranked so geographically-close matches outrank far
    /// ones at similar BM25 scores. Skips the FST fast-path (which
    /// returns one globally-prominent hit). See docs/SDK_PATTERNS.md
    /// for client-side patterns to source the coord (browser
    /// geolocation, mobile GPS, IP→coord chain through `/geocode/ip`).
    #[serde(default)]
    bias_lat: Option<f64>,
    #[serde(default)]
    bias_lng: Option<f64>,
}

/// Resolve the `h3_res` query param to a validated list of resolutions.
/// Returns an empty Vec when unset (no enrichment). Returns a 400 error
/// response when the input is malformed so the handler can short-circuit.
fn resolve_h3_res(raw: Option<&str>) -> Result<Vec<u8>, Response> {
    match raw {
        None => Ok(Vec::new()),
        Some(s) => query_server::h3_cell::parse_h3_res(s).map_err(|msg| {
            (StatusCode::BAD_REQUEST, msg).into_response()
        }),
    }
}

/// Check a single text param against `max` bytes. Returns `Some(400)`
/// when the input is too long so the handler can `if let Some(r) =
/// check_text(...) { return r; }`. Caps live in
/// `query_server::limits` — same set the gRPC handlers use.
fn check_text(name: &str, val: &str, max: usize) -> Option<Response> {
    query_server::limits::check(name, val, max)
        .err()
        .map(|msg| (StatusCode::BAD_REQUEST, msg).into_response())
}

/// Same as [`check_text`] but for `Option<String>` params — skips
/// validation when the field is absent. Convenience for the bulk
/// of structured-search params that are optional.
fn check_text_opt(name: &str, val: Option<&str>, max: usize) -> Option<Response> {
    val.and_then(|s| check_text(name, s, max))
}

/// Liveness probe. 200 + `{"status":"ok"}` the moment the process
/// can accept HTTP — no dependencies touched. Use this for k8s
/// liveness probes and bare-bones ALB checks. Kept as the default
/// `/healthz` route for backwards compatibility with scrapers
/// configured before we split liveness vs readiness.
async fn healthz() -> Response {
    healthz_live().await
}

async fn healthz_live() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        r#"{"status":"ok"}"#,
    )
        .into_response()
}

/// Readiness probe. **200 only after `Index::load` has succeeded**;
/// 503 while the index is still loading or after a swap that hasn't
/// completed. Use this for ALB target-group health checks and k8s
/// readiness probes so traffic doesn't get routed to instances that
/// would otherwise return 5XX during the cold-boot mmap-page-in
/// window.
///
/// Differs from `/healthz/live` deliberately: the live probe says
/// "the process is up"; the ready probe says "this instance can
/// serve real traffic right now." A k8s pod that's `live=true,
/// ready=false` is a normal cold-boot state — the kubelet leaves
/// the container running while the LB skips routing to it.
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

/// Prometheus scrape endpoint. Always available — no auth, no
/// disable knob — operators are expected to gate access at the
/// network layer (security-group / ingress rule). The body is the
/// Prometheus text exposition format produced by
/// `query_server::metrics::Metrics::render`.
async fn metrics_handler(
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> Response {
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

/// Per-index health. Reports which optional indexes are loaded and,
/// for the ones that are partitioned by country, which ISO 3166-1
/// alpha-2 codes are covered. Clients can use this for granular
/// traffic routing ("does this instance serve country X?") or for
/// diagnostics.
async fn healthz_indexes(
    index: axum::extract::Extension<LiveIndex>,
    #[cfg(feature = "forward")] forward_idx: axum::extract::Extension<Option<Arc<Forward>>>,
    autocomplete_idx: axum::extract::Extension<Option<Arc<Autocomplete>>>,
    ip_db: axum::extract::Extension<Option<Arc<IpGeo>>>,
) -> Response {
    let snapshot = index.load();

    let mut indexes = serde_json::Map::new();
    indexes.insert(
        "reverse".into(),
        serde_json::json!({ "loaded": true }),
    );
    indexes.insert(
        "postcode_lookup".into(),
        serde_json::json!({
            "loaded": snapshot.postcode_lookup.is_some(),
            // AU-only by construction — advertise that explicitly so
            // routing layers don't have to guess.
            "countries": if snapshot.postcode_lookup.is_some() { vec!["au"] } else { vec![] },
        }),
    );
    indexes.insert(
        "gnaf".into(),
        serde_json::json!({
            "loaded": snapshot.gnaf.is_some(),
            "countries": if snapshot.gnaf.is_some() { vec!["au"] } else { vec![] },
        }),
    );
    indexes.insert(
        "open_addresses".into(),
        serde_json::json!({
            "loaded": snapshot.open_addresses.is_some(),
            "countries": snapshot
                .open_addresses
                .as_ref()
                .map(|oa| oa.countries().map(cc_to_string).collect::<Vec<_>>())
                .unwrap_or_default(),
        }),
    );
    indexes.insert(
        "i18n_names".into(),
        serde_json::json!({ "loaded": snapshot.i18n_names.is_some() }),
    );

    indexes.insert(
        "autocomplete".into(),
        serde_json::json!({
            "loaded": autocomplete_idx.is_some(),
            "countries": autocomplete_idx
                .as_ref()
                .map(|a| a.countries().iter().map(cc_to_string).collect::<Vec<_>>())
                .unwrap_or_default(),
        }),
    );

    #[cfg(feature = "forward")]
    indexes.insert(
        "forward".into(),
        serde_json::json!({
            "loaded": forward_idx.is_some(),
            "countries": forward_idx
                .as_ref()
                .map(|f| f.countries().map(cc_to_string).collect::<Vec<_>>())
                .unwrap_or_default(),
            // A monolithic fallback covers any country baked into the
            // build, even when `countries` is empty. Routing layers can
            // treat `default=true` as "serves everything this build has,
            // but we can't enumerate it at runtime".
            "default": forward_idx.as_ref().is_some_and(|f| f.has_default()),
        }),
    );
    #[cfg(not(feature = "forward"))]
    indexes.insert(
        "forward".into(),
        serde_json::json!({ "loaded": false, "compiled_out": true }),
    );

    indexes.insert(
        "ip_geo".into(),
        serde_json::json!({ "loaded": ip_db.is_some() }),
    );

    let body = serde_json::json!({
        "status": "ok",
        "indexes": indexes,
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn cc_to_string(cc: &[u8; 2]) -> String {
    std::str::from_utf8(cc).unwrap_or("??").to_owned()
}

/// `/h3?lat=&lon=&h3_res=…` — pure h3o conversion, no reverse-geocode.
/// Resolves in microseconds (no mmap reads, no admin lookup). Useful
/// for clients doing high-volume spatial-join enrichment where the
/// address itself isn't needed and the per-request cost of a /reverse
/// would dominate.
///
/// Differences from the enrichment field on /reverse: `h3_res` is
/// required here (empty would 200 with an empty body — confusing) and
/// the response carries no address shape, just `{lat, lon, h3}`.
#[tracing::instrument(
    name = "h3",
    skip_all,
    fields(
        geocoder.lat = params.lat,
        geocoder.lon = params.lon,
        geocoder.h3_resolutions = tracing::field::Empty,
    )
)]
async fn h3_endpoint(
    Query(params): Query<H3Params>,
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> Response {
    let started = std::time::Instant::now();
    let resolutions = match query_server::h3_cell::parse_h3_res(&params.h3_res) {
        Ok(r) => r,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    if resolutions.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "h3_res: at least one resolution required",
        )
            .into_response();
    }
    tracing::Span::current().record("geocoder.h3_resolutions", resolutions.len());

    let Some(map) = query_server::h3_cell::build_h3_map(params.lat, params.lon, &resolutions)
    else {
        // Every requested resolution failed (typically NaN/infinity input
        // — out-of-range lat/lng are normalised by h3o, not rejected).
        return (
            StatusCode::BAD_REQUEST,
            "h3: no resolution produced a cell — check the coord (NaN/infinity rejected)",
        )
            .into_response();
    };

    // /h3 has no reverse-geocode step → no country to attribute.
    metrics.record_request("h3", None, started.elapsed().as_secs_f64());

    let body = serde_json::json!({
        "lat": params.lat,
        "lon": params.lon,
        "h3": map,
    });
    axum::Json(body).into_response()
}

#[tracing::instrument(
    name = "reverse_geocode",
    skip_all,
    fields(
        geocoder.lat = params.lat,
        geocoder.lon = params.lon,
        geocoder.lang = params.lang.as_deref().unwrap_or(""),
        geocoder.h3_resolutions = tracing::field::Empty,
    )
)]
async fn reverse_geocode(
    Query(params): Query<QueryParams>,
    index: axum::extract::Extension<LiveIndex>,
    shadow: axum::extract::Extension<Option<Arc<ShadowDispatcher>>>,
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> Response {
    let started = std::time::Instant::now();
    if let Some(r) = check_text_opt("lang", params.lang.as_deref(), query_server::limits::LANG) {
        return r;
    }
    let h3_resolutions = match resolve_h3_res(params.h3_res.as_deref()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    tracing::Span::current().record("geocoder.h3_resolutions", h3_resolutions.len());

    let snapshot = index.load();
    let mut address = snapshot.query_with_lang(params.lat, params.lon, params.lang.as_deref());
    // H3 describes the caller's query coord (the thing they asked about),
    // not any refined match — /reverse has no refinement step.
    address.h3 = query_server::h3_cell::build_h3_map(params.lat, params.lon, &h3_resolutions);

    // Sample-shadow against Google before returning. The dispatcher's
    // probability gate short-circuits 99.9 % of calls in microseconds;
    // on a sampled call, takes one snapshot clone of the small admin
    // fields and a single try_send. Never awaits Google.
    if let Some(d) = shadow.0.as_ref() {
        d.shadow_reverse(params.lat, params.lon, &address);
    }

    // Record metrics with the resolved country before consuming
    // `address` into the response. record_request normalises the
    // country to lowercase and clamps anything weird to "unknown".
    metrics.record_request(
        "reverse",
        address.address.country_code.as_deref(),
        started.elapsed().as_secs_f64(),
    );

    // axum::Json writes directly to a BytesMut; skips the intermediate
    // `String` allocation + UTF-8 copy the old `to_string() -> tuple
    // response` path forced.
    axum::Json(address).into_response()
}

/// Address validation — given structured components, confirm whether the
/// address exists in our authoritative sources and return the canonical
/// normalised form. Radar's `/v1/addresses/validate` counterpart.
#[cfg(feature = "forward")]
#[tracing::instrument(
    name = "validate_address",
    skip_all,
    fields(
        geocoder.country_code = %params.country_code,
        geocoder.has_housenumber = params.housenumber.is_some(),
        geocoder.outcome = tracing::field::Empty,
    )
)]
async fn validate_address(
    Query(params): Query<ValidateParams>,
    index: axum::extract::Extension<LiveIndex>,
    forward_idx: axum::extract::Extension<Option<Arc<Forward>>>,
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> Response {
    let started = std::time::Instant::now();
    use query_server::limits as L;
    if let Some(r) = [
        check_text("street", &params.street, L::STRUCTURED_FIELD),
        check_text("city", &params.city, L::STRUCTURED_FIELD),
        check_text("country_code", &params.country_code, L::COUNTRY_CODE_LIST),
        check_text_opt("housenumber", params.housenumber.as_deref(), L::HOUSENUMBER),
        check_text_opt("state", params.state.as_deref(), L::STRUCTURED_FIELD),
        check_text_opt("postcode", params.postcode.as_deref(), L::POSTCODE),
    ]
    .into_iter()
    .flatten()
    .next()
    {
        return r;
    }
    let h3_resolutions = match resolve_h3_res(params.h3_res.as_deref()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let Some(fwd) = forward_idx.as_ref() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "Address validation requires the forward index. Run build-forward-index.",
        )
            .into_response();
    };

    // Step 1: locate the street within the named city, for a coarse
    // candidate coord. forward::search_structured applies the same fallback
    // ladder as /search so slightly-wrong inputs still resolve.
    let structured = forward::StructuredQuery {
        street: Some(&params.street),
        city: Some(&params.city),
        state: params.state.as_deref(),
        country_code: Some(&params.country_code),
        kind: Some(forward::KIND_STREET),
        limit: 5,
        ..Default::default()
    };
    let hits = match fwd.search_structured(structured) {
        Ok(h) => h,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("search failed: {e}"))
                .into_response()
        }
    };
    let Some(top) = hits.first() else {
        tracing::Span::current().record("geocoder.outcome", "street_not_found");
        let body = serde_json::json!({
            "verified": false,
            "confidence": "fallback",
            "reason": "street not found in the requested city/state/country",
        });
        return (StatusCode::OK, axum::Json(body)).into_response();
    };

    // Step 2: refine to the exact property if a housenumber was supplied.
    let idx_snap = index.load();
    let cc_bytes: Option<[u8; 2]> = parse_country_code_bytes(&params.country_code);
    let (final_lat, final_lng, verified, confidence_reason) = if let Some(hn) = params.housenumber.as_deref()
    {
        let refine_span = tracing::info_span!(
            target: "query_server::validate",
            "validate.house_number_refine",
            geocoder.stage = "house_number_refine",
        );
        let resolved = refine_span.in_scope(|| {
            idx_snap.find_addr_point_in_country(
                hn,
                Some(&top.name),
                top.lat,
                top.lng,
                cc_bytes.as_ref(),
            )
        });
        match resolved {
            Some(m) => (m.lat, m.lng, true, "exact"),
            None => (top.lat, top.lng, false, "fallback: street found, house number not in index"),
        }
    } else {
        (top.lat, top.lng, false, "interpolated: street resolved, no house number to verify")
    };
    tracing::Span::current().record("geocoder.outcome", confidence_reason);

    // Step 3: reverse-geocode the final coord to build the canonical form.
    let canonical = idx_snap.query(final_lat, final_lng);

    let body = serde_json::json!({
        "verified": verified,
        "confidence": if verified { "exact" } else { confidence_reason },
        "input": {
            "housenumber": params.housenumber,
            "street": params.street,
            "city": params.city,
            "state": params.state,
            "postcode": params.postcode,
            "country_code": params.country_code,
        },
        "normalized": {
            "display_name": canonical.display_name,
            "address": canonical.address,
        },
        "lat": final_lat,
        "lon": final_lng,
        // H3 describes the final (refined) coord — the thing the client
        // would act on — not the pre-refinement street centroid. Skip
        // the map entirely when h3_res wasn't requested.
        "h3": query_server::h3_cell::build_h3_map(final_lat, final_lng, &h3_resolutions),
    });
    metrics.record_request(
        "validate",
        Some(params.country_code.as_str()),
        started.elapsed().as_secs_f64(),
    );
    axum::Json(body).into_response()
}

/// Build a `/search`-response-shaped record from a FST fast-path hit.
/// Reverse-geocodes the coord to fill in the full address so clients see
/// the same response shape whether the hit came from FST or tantivy.
#[cfg(feature = "forward")]
fn enrich_fst_hit(
    fst_hit: query_server::autocomplete::Hit,
    index: &Index,
    h3_resolutions: &[u8],
) -> serde_json::Value {
    let addr = index.query(fst_hit.lat, fst_hit.lng);
    let d = &addr.address;
    serde_json::json!({
        "name": fst_hit.name,
        "kind": fst_hit.kind,
        "rank": fst_hit.rank,
        // Fast-path doesn't run BM25; report a sentinel score that clients
        // can use to detect FST hits for telemetry. Prominence-sorted by
        // rank on return, so "score" isn't meaningful here.
        "score": 0.0,
        "source": "fst",
        "lat": fst_hit.lat,
        "lon": fst_hit.lng,
        "display_name": addr.display_name,
        "address": {
            "house_number": d.house_number.as_deref().map(str::to_owned),
            "road": d.road,
            "city": d.city,
            "state": d.state,
            "county": d.county,
            "postcode": d.postcode,
            "country": d.country,
            "country_code": d.country_code,
        },
        "confidence": addr.confidence,
        // FST hits and tantivy hits share the same response shape —
        // if one carries `h3` both must, otherwise clients see silent
        // inconsistency per query path.
        "h3": query_server::h3_cell::build_h3_map(fst_hit.lat, fst_hit.lng, h3_resolutions),
    })
}

/// Parse a user-supplied country code string into the 2-byte uppercase
/// pair our `find_addr_point_in_country` uses.
fn parse_country_code_bytes(s: &str) -> Option<[u8; 2]> {
    let b = s.trim().as_bytes();
    if b.len() != 2 || !b[0].is_ascii_alphabetic() || !b[1].is_ascii_alphabetic() {
        return None;
    }
    Some([b[0].to_ascii_uppercase(), b[1].to_ascii_uppercase()])
}

#[tracing::instrument(
    name = "autocomplete",
    skip_all,
    fields(
        geocoder.q = %params.q,
        geocoder.country_code = params.country_code.as_deref().unwrap_or(""),
        geocoder.limit = tracing::field::Empty,
        geocoder.match_count = tracing::field::Empty,
        geocoder.autocomplete.fst_variant = tracing::field::Empty,
    )
)]
async fn autocomplete(
    Query(params): Query<AutocompleteParams>,
    autocomplete_idx: axum::extract::Extension<Option<Arc<Autocomplete>>>,
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> Response {
    let started = std::time::Instant::now();
    use query_server::limits as L;
    if let Some(r) = [
        check_text("q", &params.q, L::AUTOCOMPLETE_Q),
        check_text_opt("country_code", params.country_code.as_deref(), L::COUNTRY_CODE),
    ]
    .into_iter()
    .flatten()
    .next()
    {
        return r;
    }
    let h3_resolutions = match resolve_h3_res(params.h3_res.as_deref()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let Some(autoc) = autocomplete_idx.as_ref() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "Autocomplete not built. Run build-autocomplete-fst.",
        )
            .into_response();
    };

    let limit = params.limit.unwrap_or(10).clamp(1, 50);
    tracing::Span::current().record("geocoder.limit", limit);
    let fst_span = tracing::info_span!(
        target: "query_server::autocomplete",
        "autocomplete.fst_lookup",
        geocoder.stage = "fst_lookup",
        geocoder.autocomplete.fst_variant = tracing::field::Empty,
    );
    let results: Vec<query_server::autocomplete::Hit> = fst_span.in_scope(|| {
        match params.country_code.as_deref().and_then(parse_country_code_bytes) {
            Some(cc_upper) => {
                let cc_lower = [cc_upper[0].to_ascii_lowercase(), cc_upper[1].to_ascii_lowercase()];
                autoc.search(&cc_lower, &params.q, limit)
            }
            None => autoc.search_any(&params.q, limit),
        }
    });
    tracing::Span::current().record("geocoder.match_count", results.len());

    // Transform hits into JSON values so we can attach `h3` per-result
    // without coupling the autocomplete::Hit struct to H3.
    let enriched: Vec<serde_json::Value> = results
        .into_iter()
        .map(|hit| {
            let lat = hit.lat;
            let lng = hit.lng;
            let mut v = serde_json::to_value(hit).expect("autocomplete hit serialisable");
            if let Some(obj) = v.as_object_mut() {
                if let Some(h3) = query_server::h3_cell::build_h3_map(lat, lng, &h3_resolutions) {
                    obj.insert(
                        "h3".to_string(),
                        serde_json::to_value(h3).expect("h3 map serialisable"),
                    );
                }
            }
            v
        })
        .collect();

    metrics.record_request(
        "autocomplete",
        params.country_code.as_deref(),
        started.elapsed().as_secs_f64(),
    );

    let body = serde_json::json!({ "results": enriched });
    axum::Json(body).into_response()
}

#[tracing::instrument(
    name = "ip_geocode",
    skip_all,
    fields(
        geocoder.client_ip = tracing::field::Empty,
        geocoder.outcome = tracing::field::Empty,
    )
)]
async fn ip_geocode(
    Query(params): Query<IpParams>,
    index: axum::extract::Extension<LiveIndex>,
    ip_db: axum::extract::Extension<Option<Arc<IpGeo>>>,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> Response {
    let started = std::time::Instant::now();
    if let Some(r) = check_text_opt("ip", params.ip.as_deref(), query_server::limits::IP) {
        return r;
    }
    let h3_resolutions = match resolve_h3_res(params.h3_res.as_deref()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // If the caller passes `?ip=...` accept that (convenient for admin
    // testing), otherwise default to the socket's peer address.
    let ip: std::net::IpAddr = match params.ip.as_deref() {
        Some(s) => match s.parse() {
            Ok(ip) => ip,
            Err(_) => {
                tracing::Span::current().record("geocoder.outcome", "invalid_ip");
                return (StatusCode::BAD_REQUEST, format!("invalid ip {s:?}")).into_response();
            }
        },
        None => connect_info.0.ip(),
    };
    tracing::Span::current().record("geocoder.client_ip", tracing::field::display(&ip));

    let Some(db) = ip_db.as_ref() else {
        tracing::Span::current().record("geocoder.outcome", "disabled");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "IP geocoding not enabled. Put GeoLite2-City.mmdb under the data dir or set GEOLITE2_DB.",
        )
            .into_response();
    };

    let mmdb_span = tracing::info_span!(
        target: "query_server::ip",
        "ip_geocode.mmdb_lookup",
        geocoder.stage = "mmdb_lookup",
    );
    let lookup = mmdb_span.in_scope(|| db.lookup(ip));
    let Some((lat, lon)) = lookup else {
        tracing::Span::current().record("geocoder.outcome", "not_found");
        return (StatusCode::NOT_FOUND, format!("no location for ip {ip}")).into_response();
    };
    tracing::Span::current().record("geocoder.outcome", "ok");

    let snapshot = index.load();
    let address = snapshot.query(lat, lon);
    metrics.record_request(
        "ip_geocode",
        address.address.country_code.as_deref(),
        started.elapsed().as_secs_f64(),
    );
    let body = serde_json::json!({
        "ip": ip.to_string(),
        "lat": lat,
        "lon": lon,
        "display_name": address.display_name,
        "address": address.address,
        "confidence": address.confidence,
        "h3": query_server::h3_cell::build_h3_map(lat, lon, &h3_resolutions),
    });
    axum::Json(body).into_response()
}

#[cfg(feature = "forward")]
#[tracing::instrument(
    name = "search",
    skip_all,
    fields(
        geocoder.q = params.q.as_deref().unwrap_or(""),
        geocoder.street = params.street.as_deref().unwrap_or(""),
        geocoder.city = params.city.as_deref().unwrap_or(""),
        geocoder.state = params.state.as_deref().unwrap_or(""),
        geocoder.country_code = params.country_code.as_deref().unwrap_or(""),
        geocoder.kind = params.kind.as_deref().unwrap_or(""),
        geocoder.path = tracing::field::Empty,
        geocoder.match_count = tracing::field::Empty,
    )
)]
async fn search(
    Query(params): Query<SearchParams>,
    index: axum::extract::Extension<LiveIndex>,
    forward_idx: axum::extract::Extension<Option<Arc<Forward>>>,
    autocomplete_idx: axum::extract::Extension<Option<Arc<Autocomplete>>>,
    shadow: axum::extract::Extension<Option<Arc<ShadowDispatcher>>>,
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> Response {
    let started = std::time::Instant::now();
    use query_server::limits as L;
    if let Some(r) = [
        check_text_opt("q", params.q.as_deref(), L::SEARCH_Q),
        check_text_opt("street", params.street.as_deref(), L::STRUCTURED_FIELD),
        check_text_opt("housenumber", params.housenumber.as_deref(), L::HOUSENUMBER),
        check_text_opt("city", params.city.as_deref(), L::STRUCTURED_FIELD),
        check_text_opt("state", params.state.as_deref(), L::STRUCTURED_FIELD),
        check_text_opt("country_code", params.country_code.as_deref(), L::COUNTRY_CODE_LIST),
    ]
    .into_iter()
    .flatten()
    .next()
    {
        return r;
    }

    // Proximity bias: both bias_lat and bias_lng must be supplied or
    // neither — half a coord is a programming error, not a useful
    // partial signal. Range-validate via BiasCoord::try_new so the
    // search path can rely on these being in spec.
    let bias = match (params.bias_lat, params.bias_lng) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                "bias_lat and bias_lng must be supplied together",
            )
                .into_response();
        }
        (Some(lat), Some(lng)) => match forward::BiasCoord::try_new(lat, lng) {
            Ok(b) => Some(b),
            Err(field) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("{field}: out of range (lat ∈ [-90,90], lng ∈ [-180,180])"),
                )
                    .into_response();
            }
        },
    };

    let h3_resolutions = match resolve_h3_res(params.h3_res.as_deref()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let Some(fwd) = forward_idx.as_ref() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "Forward geocoding index not loaded; run `build-forward-index <data>/index` to enable /search",
        )
            .into_response();
    };

    let kind_filter = match params.kind.as_deref() {
        Some("place") => Some(forward::KIND_PLACE),
        Some("street") => Some(forward::KIND_STREET),
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid kind {other:?}; expected 'place' or 'street'"),
            )
                .into_response()
        }
        None => None,
    };

    // If the caller sent a freeform `q`, pre-parse to surface house number /
    // state / postcode hints. Structured params take precedence when both
    // are present.
    let parsed = params.q.as_deref().map(forward::parse_freeform_query);
    let housenumber = params
        .housenumber
        .clone()
        .or_else(|| parsed.as_ref().and_then(|p| p.house_number.clone()));

    let limit = params.limit.unwrap_or(10).clamp(1, 50);

    // Support Radar-style multi-country filter: `country_code=US,CA,MX`.
    // When multiple codes are present we run one search per code against
    // its per-country index and merge the top-N by score. Single-value
    // queries take the common path with no extra work.
    let country_codes: Vec<&str> = params
        .country_code
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // FST fast-path: a simple freeform query that exactly matches an FST
    // key (e.g. `"sydney"` → the Sydney place record) resolves in ~5 µs
    // without touching tantivy. Radar's public architecture attributes
    // ~80% of their traffic to this fast-path. Skip when the query is
    // structured (street/city/state set), when the caller asked for
    // multiple countries, when no FST is loaded, when caller asked
    // for limit > 1 (fast-path returns exactly one hit), or when a
    // proximity bias is set (fast-path returns the globally-prominent
    // pick, which contradicts the bias intent).
    let is_simple_freeform = params.street.is_none()
        && params.city.is_none()
        && params.state.is_none()
        && country_codes.len() <= 1
        && limit == 1
        && bias.is_none()
        && params.q.as_deref().is_some_and(|s| !s.trim().is_empty());
    if is_simple_freeform {
        if let Some(autoc) = autocomplete_idx.as_ref() {
            let q_text = params
                .q
                .as_deref()
                .expect("is_simple_freeform guarantees q is Some and non-empty");
            let fst_span = tracing::info_span!(
                target: "query_server::search",
                "search.fst_fast_path",
                geocoder.stage = "fst_fast_path",
                geocoder.match = tracing::field::Empty,
                geocoder.autocomplete.fst_variant = tracing::field::Empty,
            );
            let fst_hit = fst_span.in_scope(|| match country_codes.first().copied() {
                Some(cc) => parse_country_code_bytes(cc).map(|code| {
                    let cc_lower = [code[0].to_ascii_lowercase(), code[1].to_ascii_lowercase()];
                    autoc.exact_match(&cc_lower, q_text)
                }).unwrap_or(None),
                None => autoc.exact_match_any(q_text).map(|(_, h)| h),
            });
            fst_span.record("geocoder.match", fst_hit.is_some());
            if let Some(fst_hit) = fst_hit {
                // Honour kind filter even on the fast-path.
                let wanted_kind = match params.kind.as_deref() {
                    Some("place") => Some(forward::KIND_PLACE as u64),
                    Some("street") => Some(forward::KIND_STREET as u64),
                    _ => None,
                };
                let kind_ok = wanted_kind
                    .map(|k| k == fst_hit.kind as u64)
                    .unwrap_or(true);
                if kind_ok {
                    let idx_snap = index.load();
                    let enriched = enrich_fst_hit(fst_hit, &idx_snap, &h3_resolutions);
                    tracing::Span::current().record("geocoder.path", "fst_fast_path");
                    tracing::Span::current().record("geocoder.match_count", 1);
                    if let Some(d) = shadow.0.as_ref() {
                        let snap = snapshot_from_search_hit(&enriched);
                        d.shadow_search_with_snapshot(
                            params.q.as_deref().unwrap_or(""),
                            country_codes.first().copied(),
                            snap,
                        );
                    }
                    // Country derived from the top hit's resolved
                    // country_code so a query like "sydney" with no
                    // explicit country_code still labels correctly.
                    let cc = enriched
                        .get("address")
                        .and_then(|a| a.get("country_code"))
                        .and_then(|v| v.as_str());
                    metrics.record_request("search", cc, started.elapsed().as_secs_f64());
                    let body = serde_json::json!({ "results": [enriched] });
                    return axum::Json(body).into_response();
                }
            }
        }
    }

    let hits = if country_codes.len() > 1 {
        tracing::Span::current().record("geocoder.path", "tantivy_multi_country");
        let mut merged: Vec<forward::Hit> = Vec::new();
        for cc in &country_codes {
            let structured = forward::StructuredQuery {
                q: params.q.as_deref(),
                street: params.street.as_deref(),
                city: params.city.as_deref(),
                state: params.state.as_deref(),
                country_code: Some(cc),
                kind: kind_filter,
                limit,
                bias,
            };
            match fwd.search_structured(structured) {
                Ok(mut h) => merged.append(&mut h),
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, format!("search failed: {e}"))
                        .into_response()
                }
            }
        }
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged.truncate(limit);
        merged
    } else {
        tracing::Span::current().record("geocoder.path", "tantivy");
        let structured = forward::StructuredQuery {
            q: params.q.as_deref(),
            street: params.street.as_deref(),
            city: params.city.as_deref(),
            state: params.state.as_deref(),
            country_code: country_codes.first().copied(),
            kind: kind_filter,
            limit,
            bias,
        };
        match fwd.search_structured(structured) {
            Ok(h) => h,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("search failed: {e}"))
                    .into_response()
            }
        }
    };

    tracing::Span::current().record("geocoder.match_count", hits.len());

    // Enrich each hit: if a house number is known, try to refine the street
    // result to the specific addr_point; in any case reverse-geocode the
    // final coordinate so callers get the full display_name + structured
    // address fields.
    let enrich_span = tracing::info_span!(
        target: "query_server::search",
        "search.enrich",
        geocoder.stage = "enrich",
        geocoder.hit_count = hits.len(),
        geocoder.address.source = tracing::field::Empty,
    );
    let idx_snapshot = index.load();
    let enriched: Vec<serde_json::Value> = enrich_span.in_scope(|| {
        hits.into_iter()
            .map(|hit| enrich_hit(hit, housenumber.as_deref(), &idx_snapshot, &h3_resolutions))
            .collect()
    });

    if let Some(d) = shadow.0.as_ref() {
        let snap = enriched.first().and_then(snapshot_from_search_hit);
        d.shadow_search_with_snapshot(
            params.q.as_deref().unwrap_or(""),
            country_codes.first().copied(),
            snap,
        );
    }

    // Prefer the top-hit's resolved country_code over the request
    // param — a query without an explicit country still gets
    // labelled correctly. Falls back to the param when the hit
    // doesn't carry one.
    let cc_from_top = enriched
        .first()
        .and_then(|h| h.get("address"))
        .and_then(|a| a.get("country_code"))
        .and_then(|v| v.as_str());
    let cc = cc_from_top.or_else(|| country_codes.first().copied());
    metrics.record_request("search", cc, started.elapsed().as_secs_f64());

    let body = serde_json::json!({ "results": enriched });
    axum::Json(body).into_response()
}

/// Hard-radius geographic search. Returns hits within `radius_km` of
/// (`lat`, `lng`), sorted by distance ascending. Optional `q`, `kind`,
/// and `country_code` narrow the candidates inside the radius.
#[cfg(feature = "forward")]
#[tracing::instrument(
    name = "http.nearby",
    skip_all,
    fields(
        geocoder.lat = params.lat,
        geocoder.lng = params.lng,
        geocoder.radius_km = params.radius_km,
        geocoder.q = params.q.as_deref().unwrap_or(""),
        geocoder.kind = params.kind.as_deref().unwrap_or(""),
        geocoder.country_code = params.country_code.as_deref().unwrap_or(""),
        geocoder.match_count = tracing::field::Empty,
    )
)]
async fn nearby(
    Query(params): Query<NearbyParams>,
    index: axum::extract::Extension<LiveIndex>,
    forward_idx: axum::extract::Extension<Option<Arc<Forward>>>,
    metrics: axum::extract::Extension<Arc<Metrics>>,
) -> Response {
    let started = std::time::Instant::now();
    use query_server::limits as L;
    if let Some(r) = [
        check_text_opt("q", params.q.as_deref(), L::SEARCH_Q),
        check_text_opt("country_code", params.country_code.as_deref(), L::COUNTRY_CODE_LIST),
    ]
    .into_iter()
    .flatten()
    .next()
    {
        return r;
    }

    let h3_resolutions = match resolve_h3_res(params.h3_res.as_deref()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let Some(fwd) = forward_idx.as_ref() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "Forward geocoding index not loaded; run `build-forward-index <data>/index` to enable /nearby",
        )
            .into_response();
    };

    let kind_filter = match params.kind.as_deref() {
        Some("place") => Some(forward::KIND_PLACE),
        Some("street") => Some(forward::KIND_STREET),
        Some("poi") => Some(forward::KIND_POI),
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid kind {other:?}; expected 'place', 'street', or 'poi'"),
            )
                .into_response()
        }
        None => None,
    };

    let limit = params.limit.unwrap_or(10).clamp(1, 50);

    let nearby_q = forward::NearbyQuery {
        lat: params.lat,
        lng: params.lng,
        radius_km: params.radius_km,
        q: params.q.as_deref(),
        country_code: params.country_code.as_deref(),
        kind: kind_filter,
        limit,
    };

    if let Err(field) = nearby_q.validate() {
        let detail = match field {
            "lat" => "lat: out of range (must be finite, in [-90, 90])",
            "lng" => "lng: out of range (must be finite, in [-180, 180])",
            "radius_km" => "radius_km: must be finite, > 0, ≤ 100",
            other => other,
        };
        return (StatusCode::BAD_REQUEST, detail).into_response();
    }

    let hits = match fwd.search_nearby(nearby_q) {
        Ok(h) => h,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("nearby failed: {e}"))
                .into_response()
        }
    };

    tracing::Span::current().record("geocoder.match_count", hits.len());

    let idx_snapshot = index.load();
    let enriched: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|hit| {
            // Compute the radial distance once we know the final coord
            // and stamp it on the response so callers can rank/render
            // without re-doing haversine themselves.
            let d_m = query_server::geo::haversine_m(hit.lat, hit.lng, params.lat, params.lng);
            let mut v = enrich_hit(hit, None, &idx_snapshot, &h3_resolutions);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("distance_m".into(), serde_json::json!(d_m.round()));
            }
            v
        })
        .collect();

    let cc = enriched
        .first()
        .and_then(|h| h.get("address"))
        .and_then(|a| a.get("country_code"))
        .and_then(|v| v.as_str())
        .or(params.country_code.as_deref());
    metrics.record_request("nearby", cc, started.elapsed().as_secs_f64());

    let body = serde_json::json!({ "results": enriched });
    axum::Json(body).into_response()
}

/// Pull the (lat, lon, country_code, state, city, road, display_name)
/// shape out of a `/search` enriched hit JSON value into the owned
/// `OurSnapshot` form the shadow worker needs. Returns `None` when the
/// hit lacks coords — `enrich_hit` always produces them, but the JSON
/// shape isn't enforced at the type level so we tolerate the missing
/// case rather than panic.
#[cfg(feature = "forward")]
fn snapshot_from_search_hit(
    hit: &serde_json::Value,
) -> Option<(f64, f64, query_server::shadow::OurSnapshot)> {
    let lat = hit.get("lat")?.as_f64()?;
    let lng = hit.get("lon")?.as_f64()?;
    let addr = hit.get("address");
    let display_name = hit
        .get("display_name")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let snap = query_server::shadow::OurSnapshot {
        country_code: addr
            .and_then(|a| a.get("country_code"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        state: addr
            .and_then(|a| a.get("state"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        city: addr
            .and_then(|a| a.get("city"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        road: addr
            .and_then(|a| a.get("road"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        display_name,
        lat: Some(lat),
        lng: Some(lng),
    };
    Some((lat, lng, snap))
}

/// Upgrade a forward `Hit` into a dispatch-grade record: apply house-number
/// refinement when a number was parsed from the query and the hit is a
/// street, then reverse-geocode the final coord so the response carries
/// normalised admin fields (country, state, city, postcode).
#[cfg(feature = "forward")]
fn enrich_hit(
    hit: forward::Hit,
    housenumber: Option<&str>,
    index: &Index,
    h3_resolutions: &[u8],
) -> serde_json::Value {
    use serde_json::json;

    let (final_lat, final_lng, matched_hn) = match (hit.kind, housenumber) {
        (forward::KIND_STREET, Some(hn)) => {
            match index.find_addr_point(hn, Some(&hit.name), hit.lat, hit.lng) {
                Some(m) => (m.lat, m.lng, Some(m.housenumber.to_owned())),
                None => (hit.lat, hit.lng, None),
            }
        }
        _ => (hit.lat, hit.lng, None),
    };

    let addr = index.query(final_lat, final_lng);
    let details = &addr.address;

    json!({
        "name": hit.name,
        "kind": hit.kind,
        "rank": hit.rank,
        "score": hit.score,
        "lat": final_lat,
        "lon": final_lng,
        "display_name": addr.display_name,
        "address": {
            "house_number": matched_hn.or_else(|| details.house_number.as_deref().map(str::to_owned)),
            "road": details.road,
            "city": details.city,
            "state": details.state,
            "county": details.county,
            "postcode": details.postcode,
            "country": details.country,
            "country_code": details.country_code,
        },
        // Always use the refined coord (post house-number match) — that's
        // the coord the client will act on and should spatial-join by.
        "h3": query_server::h3_cell::build_h3_map(final_lat, final_lng, h3_resolutions),
    })
}

/// Try to open the tantivy forward-geocoding index. Pass the
/// **data directory** (the parent that contains either a single
/// `tantivy/` subdirectory, or sibling `tantivy_<cc>/` subdirs from
/// a `--partition-by-country` build, or both); `Forward::open`
/// handles both layouts. Returns `None` (and logs) when no forward
/// index is present — the server still starts, but `/search` will
/// respond 501 until an index is built.
///
/// Earlier versions of this function joined `tantivy` to the data
/// dir before calling `Forward::open`. That worked for the
/// monolithic single-`tantivy/` layout but silently broke
/// partitioned builds: the per-country `tantivy_<cc>/` dirs are
/// siblings of `tantivy/`, not children, so opening from
/// `<data_dir>/tantivy` makes them invisible. The result was the
/// `Forward::pick()` call returning `no_index` and every search
/// returning `{"results": []}` despite the per-country indexes
/// being intact on disk.
#[cfg(feature = "forward")]
fn load_forward_index(data_dir: &str) -> Option<Arc<Forward>> {
    let dir = Path::new(data_dir);
    if !dir.exists() {
        tracing::warn!(
            target: "query_server::startup",
            path = %dir.display(),
            data_dir = %data_dir,
            "data directory not found; /search disabled"
        );
        return None;
    }
    match Forward::open(dir) {
        Ok(fwd) if fwd.is_empty() => {
            tracing::warn!(
                target: "query_server::startup",
                path = %dir.display(),
                "no forward index found (no tantivy/ or tantivy_<cc>/ \
                 subdirectories); /search disabled — run build-forward-index"
            );
            None
        }
        Ok(fwd) => {
            tracing::info!(
                target: "query_server::startup",
                path = %dir.display(),
                "loaded forward-geocoding index"
            );
            Some(Arc::new(fwd))
        }
        Err(e) => {
            tracing::error!(
                target: "query_server::startup",
                path = %dir.display(),
                error = %e,
                "failed to open forward index; /search disabled"
            );
            None
        }
    }
}

/// Load the admin-level mapping config. If `GEOCODER_ADMIN_CONFIG` points
/// at a JSON file, use that; otherwise fall back to the embedded default.
fn load_admin_config() -> AdminConfig {
    match std::env::var("GEOCODER_ADMIN_CONFIG").ok() {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(src) => match AdminConfig::from_json(&src) {
                Ok(cfg) => {
                    tracing::info!(
                        target: "query_server::startup",
                        path = %path,
                        "loaded admin-mapping config"
                    );
                    cfg
                }
                Err(e) => {
                    tracing::warn!(
                        target: "query_server::startup",
                        path = %path,
                        error = %e,
                        "invalid admin-mapping config — using embedded default"
                    );
                    AdminConfig::embedded_default()
                }
            },
            Err(e) => {
                tracing::warn!(
                    target: "query_server::startup",
                    path = %path,
                    error = %e,
                    "failed to read admin-mapping config — using embedded default"
                );
                AdminConfig::embedded_default()
            }
        },
        None => AdminConfig::embedded_default(),
    }
}

/// Spawn a background task that polls a marker file (`<data_dir>/.reload`
/// by default, override via GEOCODER_RELOAD_MARKER env var) and reloads the
/// index atomically when the marker's mtime changes. Poll interval is 5 s
/// by default, override via GEOCODER_RELOAD_INTERVAL_SEC.
fn spawn_index_reloader(
    live: LiveIndex,
    data_dir: String,
    street_level: u64,
    admin_level: u64,
    search_distance: f64,
) {
    let marker = std::env::var("GEOCODER_RELOAD_MARKER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&data_dir).join(".reload"));
    let interval_sec: u64 = std::env::var("GEOCODER_RELOAD_INTERVAL_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    tracing::info!(
        target: "query_server::reloader",
        marker = %marker.display(),
        interval_sec = interval_sec,
        "index reloader watching marker"
    );

    tokio::spawn(async move {
        let mut last_mtime: Option<SystemTime> = std::fs::metadata(&marker)
            .ok()
            .and_then(|m| m.modified().ok());

        loop {
            tokio::time::sleep(Duration::from_secs(interval_sec)).await;

            let Some(current) = std::fs::metadata(&marker)
                .ok()
                .and_then(|m| m.modified().ok())
            else {
                continue;
            };

            if last_mtime == Some(current) {
                continue;
            }

            tracing::info!(
                target: "query_server::reloader",
                data_dir = %data_dir,
                "marker changed — reloading index"
            );
            let admin_config = load_admin_config();
            match Index::load_with_admin_config(
                &data_dir,
                street_level,
                admin_level,
                search_distance,
                admin_config,
            ) {
                Ok(new_idx) => {
                    live.store(Arc::new(new_idx));
                    last_mtime = Some(current);
                    tracing::info!(
                        target: "query_server::reloader",
                        "swap complete"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        target: "query_server::reloader",
                        error = %e,
                        "load failed — keeping previous index"
                    );
                }
            }
        }
    });
}

#[tokio::main]
async fn main() {
    // Telemetry first — every line that follows lands in stdout via tracing
    // and (when OTEL_TRACE_ENABLED + endpoint are set) any startup spans
    // are eligible for OTLP export. Hold the guard for the lifetime of
    // main() so the batch span processor + metrics reader flush on shutdown.
    let telemetry_guard = telemetry::init();

    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map(|s| s.as_str()).unwrap_or(".");

    let arg_value = |flag: &str| -> Option<&String> {
        args.iter().position(|a| a == flag).and_then(|p| args.get(p + 1))
    };
    let street_cell_level = arg_value("--street-level").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_STREET_CELL_LEVEL);
    let admin_cell_level = arg_value("--admin-level").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_ADMIN_CELL_LEVEL);
    let search_distance = arg_value("--search-distance").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_SEARCH_DISTANCE);

    tracing::info!(
        target: "query_server::startup",
        data_dir = %data_dir,
        street_cell_level,
        admin_cell_level,
        search_distance,
        "loading index"
    );
    let admin_config = load_admin_config();
    // Readiness flag — flipped to `true` once the initial Index::load
    // succeeds. The /healthz/ready probe gates ALB / k8s traffic
    // routing on this. The flag is also flipped *back* to false if a
    // hot-reload loses the index (currently unreachable since the
    // reloader keeps the previous Arc on failure, but worth wiring
    // through for future failure modes).
    let ready = Arc::new(AtomicBool::new(false));

    let index = match Index::load_with_admin_config(
        data_dir,
        street_cell_level,
        admin_cell_level,
        search_distance,
        admin_config,
    ) {
        Ok(idx) => {
            ready.store(true, Ordering::Relaxed);
            Arc::new(ArcSwap::from(Arc::new(idx)))
        }
        Err(e) => {
            tracing::error!(
                target: "query_server::startup",
                data_dir = %data_dir,
                error = %e,
                "failed to load index — exiting"
            );
            std::process::exit(1);
        }
    };

    // Background reloader: watches a marker file and atomically swaps the
    // Index when the marker's mtime changes. Writer side (update-index.sh)
    // `touch`es the marker after moving a fresh index into place.
    spawn_index_reloader(
        index.clone(),
        data_dir.to_string(),
        street_cell_level,
        admin_cell_level,
        search_distance,
    );

    // Prometheus + OTel metrics. Always created (no env-var gate) for
    // the Prometheus side — the /metrics endpoint costs nothing when
    // nobody scrapes it, and the per-handler observation overhead is
    // sub-microsecond. The OTel side is wired only when telemetry::init
    // produced a meter provider (OTEL_METRICS_ENABLED + endpoint).
    let metrics = Metrics::with_optional_meter(telemetry_guard.meter_provider());
    let _ = canonical_country(None); // warm the static table on startup

    // Optional shadow validator against Google's Geocoding API.
    // `None` when GOOGLE_GEOCODING_ENABLED=false, or the master switch
    // is implicit-on but no API key is set, or any other disabling
    // condition documented in the env-var truth matrix. Either way the
    // handlers see Option::None and skip the shadow path entirely —
    // zero overhead in the disabled state.
    let shadow_dispatcher: Option<Arc<ShadowDispatcher>> = ShadowConfig::from_env()
        .map(|cfg| ShadowDispatcher::spawn(cfg, Some(metrics.clone())));

    // Optional MaxMind GeoLite2 loader. Missing DB → /geocode/ip returns 503.
    let ip_db: Option<Arc<IpGeo>> = match IpGeo::open(Path::new(data_dir)) {
        Ok(Some(db)) => {
            tracing::info!(target: "query_server::startup", "loaded GeoLite2 DB — /geocode/ip enabled");
            Some(Arc::new(db))
        }
        Ok(None) => {
            tracing::info!(
                target: "query_server::startup",
                "no GeoLite2-City.mmdb — /geocode/ip disabled"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                target: "query_server::startup",
                error = %e,
                "failed to open GeoLite2 DB — /geocode/ip disabled"
            );
            None
        }
    };

    // Optional per-country FST autocomplete indexes.
    let autocomplete_idx: Option<Arc<Autocomplete>> = match Autocomplete::open(Path::new(data_dir)) {
        Ok(Some(a)) => {
            tracing::info!(
                target: "query_server::startup",
                country_count = a.countries().len(),
                "loaded FST autocomplete"
            );
            Some(Arc::new(a))
        }
        Ok(None) => {
            tracing::info!(
                target: "query_server::startup",
                "no FST autocomplete indexes — /autocomplete disabled"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                target: "query_server::startup",
                error = %e,
                "failed to open FST indexes — /autocomplete disabled"
            );
            None
        }
    };

    // Forward geocoding index (optional — loaded from <data_dir>/tantivy if
    // build-forward-index has been run; otherwise /search returns 501).
    #[cfg(feature = "forward")]
    let forward_idx: Option<Arc<Forward>> = load_forward_index(data_dir);
    #[cfg(not(feature = "forward"))]
    let forward_idx: Option<()> = None;

    // Per-request server span. The closure builds a span with semconv-named
    // attributes (http.request.method, url.path, otel.kind=server) at INFO,
    // and on_response stamps http.response.status_code + duration in ms
    // when the handler returns. Health checks stay at DEBUG so they don't
    // dominate prod logs.
    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(|req: &http::Request<_>| {
            let path = req.uri().path();
            let level = if path == "/healthz" || path.starts_with("/healthz/") {
                Level::DEBUG
            } else {
                Level::INFO
            };
            let span = tracing::span!(
                target: "query_server::http",
                Level::INFO,
                "http.request",
                "otel.kind" = "server",
                "otel.name" = %format!("{} {}", req.method(), path),
                "http.request.method" = %req.method(),
                "url.path" = %path,
                "url.query" = tracing::field::Empty,
                "http.response.status_code" = tracing::field::Empty,
                "http.response.body.size" = tracing::field::Empty,
            );
            if let Some(q) = req.uri().query() {
                span.record("url.query", q);
            }
            // Tower-http only honours the level via the layer's on_request
            // hook for log lines, but our make_span fixes the span level
            // here. Stash the per-request log level on the span for the
            // OnResponse hook below.
            let _ = level;
            span
        })
        .on_response(|res: &http::Response<_>, latency: std::time::Duration, span: &tracing::Span| {
            span.record("http.response.status_code", res.status().as_u16());
            tracing::debug!(
                target: "query_server::http",
                status = res.status().as_u16(),
                duration_ms = latency.as_secs_f64() * 1000.0,
                "request complete"
            );
        })
        .on_failure(DefaultOnFailure::new().level(Level::WARN));

    // Global request-body cap. Geocoder endpoints are GET-only today
    // (URL-bounded by upstream LBs to ~8 KB) but the cap is belt-and-
    // braces against future POSTs and against any axum-internal path
    // that might read a body. 64 KiB is generous — every per-field
    // text cap in `query_server::limits` fits comfortably under it.
    let body_limit = axum::extract::DefaultBodyLimit::max(64 * 1024);

    #[cfg(feature = "forward")]
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/healthz/live", get(healthz_live))
        .route("/healthz/ready", get(healthz_ready))
        .route("/healthz/indexes", get(healthz_indexes))
        .route("/metrics", get(metrics_handler))
        .route("/reverse", get(reverse_geocode))
        .route("/search", get(search))
        .route("/nearby", get(nearby))
        .route("/validate", get(validate_address))
        .route("/autocomplete", get(autocomplete))
        .route("/geocode/ip", get(ip_geocode))
        .route("/h3", get(h3_endpoint))
        .layer(body_limit)
        .layer(trace_layer.clone())
        .layer(axum::Extension(index.clone()))
        .layer(axum::Extension(forward_idx.clone()))
        .layer(axum::Extension(autocomplete_idx.clone()))
        .layer(axum::Extension(ip_db.clone()))
        .layer(axum::Extension(shadow_dispatcher.clone()))
        .layer(axum::Extension(metrics.clone()))
        .layer(axum::Extension(ready.clone()));
    #[cfg(not(feature = "forward"))]
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/healthz/live", get(healthz_live))
        .route("/healthz/ready", get(healthz_ready))
        .route("/healthz/indexes", get(healthz_indexes))
        .route("/metrics", get(metrics_handler))
        .route("/reverse", get(reverse_geocode))
        .route("/autocomplete", get(autocomplete))
        .route("/geocode/ip", get(ip_geocode))
        .route("/h3", get(h3_endpoint))
        .layer(body_limit)
        .layer(trace_layer.clone())
        .layer(axum::Extension(index.clone()))
        .layer(axum::Extension(autocomplete_idx.clone()))
        .layer(axum::Extension(ip_db.clone()))
        .layer(axum::Extension(shadow_dispatcher.clone()))
        .layer(axum::Extension(metrics.clone()))
        .layer(axum::Extension(ready.clone()));

    let _ = forward_idx; // silence unused when feature disabled

    let domain_pos = args.iter().position(|a| a == "--domain");
    if let Some(pos) = domain_pos {
        let domain = args.get(pos + 1).expect("--domain requires a value").clone();
        let cache_dir = args.iter().position(|a| a == "--cache")
            .and_then(|p| args.get(p + 1).cloned())
            .unwrap_or_else(|| "acme-cache".to_string());

        use rustls_acme::caches::DirCache;
        use rustls_acme::AcmeConfig;
        use tokio_stream::StreamExt;

        let mut state = AcmeConfig::new([domain.clone()])
            .cache(DirCache::new(cache_dir.clone()))
            .directory_lets_encrypt(true)
            .state();
        let acceptor = state.axum_acceptor(state.default_rustls_config());

        tokio::spawn(async move {
            loop {
                match state.next().await {
                    Some(Ok(ok)) => tracing::info!(target: "query_server::acme", event = ?ok, "ACME event"),
                    Some(Err(err)) => tracing::warn!(target: "query_server::acme", error = ?err, "ACME error"),
                    None => break,
                }
            }
        });

        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 443));
        tracing::info!(target: "query_server::startup", domain = %domain, addr = %addr, "starting HTTPS server");
        if let Err(e) = axum_server::bind(addr)
            .acceptor(acceptor)
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
        {
            tracing::error!(target: "query_server::startup", error = %e, "HTTPS server exited");
            std::process::exit(1);
        }
    } else {
        let bind_addr = args.get(2).map(|s| s.as_str()).unwrap_or("0.0.0.0:3000");
        tracing::info!(target: "query_server::startup", addr = %bind_addr, "starting HTTP server");

        // Optional gRPC server alongside REST. Default port 3001; override
        // with --grpc-port or GEOCODER_GRPC_ADDR. Bind 0.0.0.0 so the
        // container exposes the port predictably.
        #[cfg(feature = "grpc")]
        {
            spawn_grpc_server(
                &args,
                index.clone(),
                #[cfg(feature = "forward")]
                forward_idx.clone(),
                #[cfg(feature = "forward")]
                autocomplete_idx.clone(),
                ip_db.clone(),
            );
        }

        let listener = match tokio::net::TcpListener::bind(bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(
                    target: "query_server::startup",
                    addr = %bind_addr,
                    error = %e,
                    "failed to bind HTTP listener"
                );
                std::process::exit(1);
            }
        };
        if let Err(e) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        {
            tracing::error!(target: "query_server::startup", error = %e, "HTTP server exited");
            std::process::exit(1);
        }
    }
}

/// Spawn the gRPC server as a background tokio task. Reads the address
/// from `--grpc-addr` (cli) / `GEOCODER_GRPC_ADDR` (env), defaulting to
/// `0.0.0.0:3001`. Shares the same `Index`/`Forward`/etc. the REST side
/// uses — no duplicate mmap.
#[cfg(feature = "grpc")]
fn spawn_grpc_server(
    args: &[String],
    index: LiveIndex,
    #[cfg(feature = "forward")] forward_idx: Option<Arc<query_server::forward::Forward>>,
    #[cfg(feature = "forward")] autocomplete_idx: Option<Arc<Autocomplete>>,
    ip_db: Option<Arc<IpGeo>>,
) {
    let grpc_addr = args
        .iter()
        .position(|a| a == "--grpc-addr")
        .and_then(|p| args.get(p + 1).cloned())
        .or_else(|| std::env::var("GEOCODER_GRPC_ADDR").ok())
        .unwrap_or_else(|| "0.0.0.0:3001".to_string());

    let service = query_server::grpc_service::GeocoderService {
        index,
        #[cfg(feature = "forward")]
        forward: forward_idx,
        #[cfg(feature = "forward")]
        autocomplete: autocomplete_idx,
        ip_db,
    };

    let addr: std::net::SocketAddr = match grpc_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(
                target: "query_server::startup",
                grpc_addr = %grpc_addr,
                error = %e,
                "invalid gRPC address — gRPC server disabled"
            );
            return;
        }
    };

    // tower-http's grpc trace layer creates a server span per RPC and
    // labels it with rpc.system + rpc.service.method, mirroring the
    // semconv naming clients of OTel collectors expect.
    let grpc_trace_layer = TraceLayer::new_for_grpc()
        .make_span_with(|req: &http::Request<_>| {
            let path = req.uri().path();
            // gRPC paths are `/<package>.<Service>/<Method>` — split for
            // semconv-friendly fields.
            let (service, method) = path
                .strip_prefix('/')
                .and_then(|p| p.split_once('/'))
                .unwrap_or(("", ""));
            tracing::span!(
                target: "query_server::grpc",
                Level::INFO,
                "rpc.server",
                "otel.kind" = "server",
                "otel.name" = %format!("{service}/{method}"),
                "rpc.system" = "grpc",
                "rpc.service" = %service,
                "rpc.method" = %method,
                "rpc.grpc.status_code" = tracing::field::Empty,
            )
        })
        .on_response(|res: &http::Response<_>, latency: std::time::Duration, span: &tracing::Span| {
            // grpc-status arrives as a trailer most of the time, but
            // some libraries set it as an initial-metadata header for
            // unary fast-fails. Fall back to "OK" when neither is present.
            if let Some(code) = res
                .headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<i32>().ok())
            {
                span.record("rpc.grpc.status_code", code);
            }
            tracing::debug!(
                target: "query_server::grpc",
                duration_ms = latency.as_secs_f64() * 1000.0,
                "rpc complete"
            );
        });

    tokio::spawn(async move {
        tracing::info!(target: "query_server::startup", addr = %addr, "starting gRPC server");
        if let Err(e) = tonic::transport::Server::builder()
            .layer(grpc_trace_layer)
            .add_service(query_server::grpc_service::GeocoderServer::new(service))
            .serve(addr)
            .await
        {
            tracing::error!(target: "query_server::startup", error = %e, "gRPC server error");
        }
    });
}
