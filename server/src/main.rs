use arc_swap::ArcSwap;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use query_server::admin_config::AdminConfig;
use query_server::autocomplete::Autocomplete;
use query_server::ip_geo::IpGeo;
use query_server::{Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

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

/// Liveness probe. 200 + `{"status":"ok"}` whenever the process can
/// accept HTTP — no dependencies touched. Suitable for ALB/NLB/k8s
/// liveness checks.
async fn healthz() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        r#"{"status":"ok"}"#,
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

async fn reverse_geocode(
    Query(params): Query<QueryParams>,
    index: axum::extract::Extension<LiveIndex>,
) -> Response {
    let h3_resolutions = match resolve_h3_res(params.h3_res.as_deref()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let snapshot = index.load();
    let mut address = snapshot.query_with_lang(params.lat, params.lon, params.lang.as_deref());
    // H3 describes the caller's query coord (the thing they asked about),
    // not any refined match — /reverse has no refinement step.
    address.h3 = query_server::h3_cell::build_h3_map(params.lat, params.lon, &h3_resolutions);
    // serde_json::to_string never fails for Address (no non-string map keys,
    // no Serialize impls that can return Err). An error here is a code bug,
    // not a runtime condition — panicking with a clear message is more
    // useful than silently returning "" to the client.
    let json = serde_json::to_string(&address).expect("Address is always serialisable");
    ([(axum::http::header::CONTENT_TYPE, "application/json")], json).into_response()
}

/// Address validation — given structured components, confirm whether the
/// address exists in our authoritative sources and return the canonical
/// normalised form. Radar's `/v1/addresses/validate` counterpart.
#[cfg(feature = "forward")]
async fn validate_address(
    Query(params): Query<ValidateParams>,
    index: axum::extract::Extension<LiveIndex>,
    forward_idx: axum::extract::Extension<Option<Arc<Forward>>>,
) -> Response {
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
        let body = serde_json::json!({
            "verified": false,
            "confidence": "fallback",
            "reason": "street not found in the requested city/state/country",
        });
        return (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&body).expect("serialise"),
        )
            .into_response();
    };

    // Step 2: refine to the exact property if a housenumber was supplied.
    let idx_snap = index.load();
    let cc_bytes: Option<[u8; 2]> = parse_country_code_bytes(&params.country_code);
    let (final_lat, final_lng, verified, confidence_reason) = if let Some(hn) = params.housenumber.as_deref()
    {
        let resolved = idx_snap.find_addr_point_in_country(
            hn,
            Some(&top.name),
            top.lat,
            top.lng,
            cc_bytes.as_ref(),
        );
        match resolved {
            Some(m) => (m.lat, m.lng, true, "exact"),
            None => (top.lat, top.lng, false, "fallback: street found, house number not in index"),
        }
    } else {
        (top.lat, top.lng, false, "interpolated: street resolved, no house number to verify")
    };

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
    let json = serde_json::to_string(&body).expect("validate response serialisable");
    ([(axum::http::header::CONTENT_TYPE, "application/json")], json).into_response()
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

async fn autocomplete(
    Query(params): Query<AutocompleteParams>,
    autocomplete_idx: axum::extract::Extension<Option<Arc<Autocomplete>>>,
) -> Response {
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
    let results: Vec<query_server::autocomplete::Hit> =
        match params.country_code.as_deref().and_then(parse_country_code_bytes) {
            Some(cc_upper) => {
                let cc_lower = [cc_upper[0].to_ascii_lowercase(), cc_upper[1].to_ascii_lowercase()];
                autoc.search(&cc_lower, &params.q, limit)
            }
            None => autoc.search_any(&params.q, limit),
        };

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

    let body = serde_json::json!({ "results": enriched });
    let json = serde_json::to_string(&body).expect("autocomplete response serialisable");
    ([(axum::http::header::CONTENT_TYPE, "application/json")], json).into_response()
}

async fn ip_geocode(
    Query(params): Query<IpParams>,
    index: axum::extract::Extension<LiveIndex>,
    ip_db: axum::extract::Extension<Option<Arc<IpGeo>>>,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response {
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
                return (StatusCode::BAD_REQUEST, format!("invalid ip {s:?}")).into_response();
            }
        },
        None => connect_info.0.ip(),
    };

    let Some(db) = ip_db.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "IP geocoding not enabled. Put GeoLite2-City.mmdb under the data dir or set GEOLITE2_DB.",
        )
            .into_response();
    };

    let Some((lat, lon)) = db.lookup(ip) else {
        return (StatusCode::NOT_FOUND, format!("no location for ip {ip}")).into_response();
    };

    let snapshot = index.load();
    let address = snapshot.query(lat, lon);
    let body = serde_json::json!({
        "ip": ip.to_string(),
        "lat": lat,
        "lon": lon,
        "display_name": address.display_name,
        "address": address.address,
        "confidence": address.confidence,
        "h3": query_server::h3_cell::build_h3_map(lat, lon, &h3_resolutions),
    });
    let json = serde_json::to_string(&body).expect("ip response serialisable");
    ([(axum::http::header::CONTENT_TYPE, "application/json")], json).into_response()
}

#[cfg(feature = "forward")]
async fn search(
    Query(params): Query<SearchParams>,
    index: axum::extract::Extension<LiveIndex>,
    forward_idx: axum::extract::Extension<Option<Arc<Forward>>>,
    autocomplete_idx: axum::extract::Extension<Option<Arc<Autocomplete>>>,
) -> Response {
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
    // multiple countries, when no FST is loaded, or when we'd need to
    // over-fetch (fast-path returns exactly one hit).
    let is_simple_freeform = params.street.is_none()
        && params.city.is_none()
        && params.state.is_none()
        && country_codes.len() <= 1
        && params.q.as_deref().is_some_and(|s| !s.trim().is_empty());
    if is_simple_freeform {
        if let Some(autoc) = autocomplete_idx.as_ref() {
            let q_text = params
                .q
                .as_deref()
                .expect("is_simple_freeform guarantees q is Some and non-empty");
            let fst_hit = match country_codes.first().copied() {
                Some(cc) => parse_country_code_bytes(cc).map(|code| {
                    let cc_lower = [code[0].to_ascii_lowercase(), code[1].to_ascii_lowercase()];
                    autoc.exact_match(&cc_lower, q_text)
                }).unwrap_or(None),
                None => autoc.exact_match_any(q_text).map(|(_, h)| h),
            };
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
                    let body = serde_json::json!({ "results": [enriched] });
                    let json =
                        serde_json::to_string(&body).expect("fst fast-path serialisable");
                    return ([(axum::http::header::CONTENT_TYPE, "application/json")], json)
                        .into_response();
                }
            }
        }
    }

    let hits = if country_codes.len() > 1 {
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
        let structured = forward::StructuredQuery {
            q: params.q.as_deref(),
            street: params.street.as_deref(),
            city: params.city.as_deref(),
            state: params.state.as_deref(),
            country_code: country_codes.first().copied(),
            kind: kind_filter,
            limit,
        };
        match fwd.search_structured(structured) {
            Ok(h) => h,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("search failed: {e}"))
                    .into_response()
            }
        }
    };

    // Enrich each hit: if a house number is known, try to refine the street
    // result to the specific addr_point; in any case reverse-geocode the
    // final coordinate so callers get the full display_name + structured
    // address fields.
    let idx_snapshot = index.load();
    let enriched: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|hit| enrich_hit(hit, housenumber.as_deref(), &idx_snapshot, &h3_resolutions))
        .collect();

    let body = serde_json::json!({ "results": enriched });
    let json = serde_json::to_string(&body).expect("enriched results are always serialisable");
    ([(axum::http::header::CONTENT_TYPE, "application/json")], json).into_response()
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

/// Try to open the tantivy forward-geocoding index at `<data_dir>/tantivy`.
/// Returns `None` (and logs) when the directory doesn't exist — the server
/// still starts, but `/search` will respond 501 until an index is built.
#[cfg(feature = "forward")]
fn load_forward_index(data_dir: &str) -> Option<Arc<Forward>> {
    let dir = Path::new(data_dir).join("tantivy");
    if !dir.exists() {
        eprintln!(
            "Forward index not found at {} — /search disabled. Run `build-forward-index {}` to enable.",
            dir.display(),
            data_dir
        );
        return None;
    }
    match Forward::open(&dir) {
        Ok(fwd) => {
            eprintln!("Loaded forward-geocoding index from {}", dir.display());
            Some(Arc::new(fwd))
        }
        Err(e) => {
            eprintln!("Failed to open forward index at {}: {} — /search disabled", dir.display(), e);
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
                    eprintln!("Loaded admin-mapping config from {}", path);
                    cfg
                }
                Err(e) => {
                    eprintln!("Invalid admin-mapping config at {}: {} — using default", path, e);
                    AdminConfig::embedded_default()
                }
            },
            Err(e) => {
                eprintln!("Failed to read {}: {} — using default", path, e);
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

    eprintln!(
        "Index reloader: watching {} every {}s",
        marker.display(),
        interval_sec
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

            eprintln!("Index reloader: marker changed, reloading from {}", data_dir);
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
                    eprintln!("Index reloader: swap complete");
                }
                Err(e) => {
                    eprintln!("Index reloader: load failed, keeping old index: {}", e);
                }
            }
        }
    });
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map(|s| s.as_str()).unwrap_or(".");

    let arg_value = |flag: &str| -> Option<&String> {
        args.iter().position(|a| a == flag).and_then(|p| args.get(p + 1))
    };
    let street_cell_level = arg_value("--street-level").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_STREET_CELL_LEVEL);
    let admin_cell_level = arg_value("--admin-level").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_ADMIN_CELL_LEVEL);
    let search_distance = arg_value("--search-distance").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_SEARCH_DISTANCE);

    eprintln!("Loading index from {}...", data_dir);
    let admin_config = load_admin_config();
    let index = match Index::load_with_admin_config(
        data_dir,
        street_cell_level,
        admin_cell_level,
        search_distance,
        admin_config,
    ) {
        Ok(idx) => Arc::new(ArcSwap::from(Arc::new(idx))),
        Err(e) => {
            eprintln!("Error: {}", e);
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

    // Optional MaxMind GeoLite2 loader. Missing DB → /geocode/ip returns 503.
    let ip_db: Option<Arc<IpGeo>> = match IpGeo::open(Path::new(data_dir)) {
        Ok(Some(db)) => Some(Arc::new(db)),
        Ok(None) => {
            eprintln!("No GeoLite2-City.mmdb — /geocode/ip disabled");
            None
        }
        Err(e) => {
            eprintln!("Failed to open GeoLite2 DB: {e} — /geocode/ip disabled");
            None
        }
    };

    // Optional per-country FST autocomplete indexes.
    let autocomplete_idx: Option<Arc<Autocomplete>> = match Autocomplete::open(Path::new(data_dir)) {
        Ok(Some(a)) => {
            eprintln!(
                "Loaded FST autocomplete for {} countries",
                a.countries().len()
            );
            Some(Arc::new(a))
        }
        Ok(None) => {
            eprintln!("No FST autocomplete indexes — /autocomplete disabled");
            None
        }
        Err(e) => {
            eprintln!("Failed to open FST indexes: {e} — /autocomplete disabled");
            None
        }
    };

    // Forward geocoding index (optional — loaded from <data_dir>/tantivy if
    // build-forward-index has been run; otherwise /search returns 501).
    #[cfg(feature = "forward")]
    let forward_idx: Option<Arc<Forward>> = load_forward_index(data_dir);
    #[cfg(not(feature = "forward"))]
    let forward_idx: Option<()> = None;

    #[cfg(feature = "forward")]
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/healthz/indexes", get(healthz_indexes))
        .route("/reverse", get(reverse_geocode))
        .route("/search", get(search))
        .route("/validate", get(validate_address))
        .route("/autocomplete", get(autocomplete))
        .route("/geocode/ip", get(ip_geocode))
        .layer(axum::Extension(index.clone()))
        .layer(axum::Extension(forward_idx.clone()))
        .layer(axum::Extension(autocomplete_idx.clone()))
        .layer(axum::Extension(ip_db.clone()));
    #[cfg(not(feature = "forward"))]
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/healthz/indexes", get(healthz_indexes))
        .route("/reverse", get(reverse_geocode))
        .route("/autocomplete", get(autocomplete))
        .route("/geocode/ip", get(ip_geocode))
        .layer(axum::Extension(index.clone()))
        .layer(axum::Extension(autocomplete_idx.clone()))
        .layer(axum::Extension(ip_db.clone()));

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
                    Some(Ok(ok)) => eprintln!("ACME event: {:?}", ok),
                    Some(Err(err)) => eprintln!("ACME error: {:?}", err),
                    None => break,
                }
            }
        });

        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 443));
        eprintln!("Starting HTTPS server on :443 for {}...", domain);
        if let Err(e) = axum_server::bind(addr)
            .acceptor(acceptor)
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
        {
            eprintln!("HTTPS server exited: {e}");
            std::process::exit(1);
        }
    } else {
        let bind_addr = args.get(2).map(|s| s.as_str()).unwrap_or("0.0.0.0:3000");
        eprintln!("Starting HTTP server on {}...", bind_addr);

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
                eprintln!("Failed to bind {bind_addr}: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        {
            eprintln!("HTTP server exited: {e}");
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
            eprintln!("Invalid gRPC address {grpc_addr:?}: {e} — gRPC server disabled");
            return;
        }
    };

    tokio::spawn(async move {
        eprintln!("Starting gRPC server on {}...", addr);
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(query_server::grpc_service::GeocoderServer::new(service))
            .serve(addr)
            .await
        {
            eprintln!("gRPC server error: {e}");
        }
    });
}
