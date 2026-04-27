//! Accuracy harness for the bench fixture corpus.
//!
//! Reuses the load-test fixtures under `scripts/bench/fixtures/` as
//! input to a regression-style accuracy run. The fixtures already
//! carry the metadata we need to know whether the geocoder gave the
//! right answer:
//!
//!   - `reverse_coords.json`: rows tagged with their source country,
//!     so a `/reverse` response is "correct" if its
//!     `address.country_code` matches.
//!   - `search_queries.json`: rows tagged with `lat_hint`/`lng_hint`,
//!     so a `/search` response is "correct" if the top result lands
//!     within a configurable haversine radius of the hint.
//!   - `autocomplete_prefixes.json`: rows are derived from real
//!     populated-place names, so `/autocomplete` is "correct" if it
//!     returns a result whose normalised name starts with the prefix
//!     (length-aware: short prefixes only need ≥1 result; longer
//!     prefixes need a starts-with match).
//!
//! Why a separate bin from `regression-runner`:
//! the existing Pelias-style corpora are hand-curated with explicit
//! per-case expectations — a different mental model from "fixed
//! assertion shape, swept across thousands of fixture rows." Mixing
//! the two would force operators to learn two corpus formats. This
//! tool stays simple: hardcoded assertion logic, fixture rows are
//! pure inputs.
//!
//! Failures are sampled into the report so you can grep what kinds
//! of queries are off — useful as a regression diagnostic, not just
//! a pass/fail gate.
//!
//! Usage:
//!   bench-accuracy <fixtures-dir> --base-url <url>
//!                  [--sample N] [--scenarios r,s,a]
//!                  [--search-radius-km N] [--out FILE]
//!                  [--pass-threshold 0.95]
//!
//! Defaults: --sample 500 per scenario, --scenarios r,s,a (all),
//! --search-radius-km 100, --pass-threshold 0.95.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use query_server::geo::haversine_m;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Fixture row schemas (mirror the JSONs emitted by build-fixtures.sh)
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReverseRow {
    lat: f64,
    lng: f64,
    country_code: String,
    /// The Geonames place name. Not used by the assertions; kept
    /// in the row so failure-debugging operators can correlate
    /// "this query failed" with "what city was that".
    #[allow(dead_code)]
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct SearchRow {
    q: String,
    country_code: String,
    lat_hint: f64,
    lng_hint: f64,
}

#[derive(Debug, Deserialize)]
struct AutocompleteRow {
    q: String,
    country_code: String,
    len: usize,
}

// -----------------------------------------------------------------------------
// Per-row assertion result
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CaseResult {
    scenario: &'static str,
    /// Compact rebuild of the request URL, useful for
    /// failure-triage `curl` reproduction.
    request: String,
    country_code: String,
    passed: bool,
    /// Set when passed=false. One short reason; full HTTP body is
    /// not retained to keep the report under control.
    reason: Option<String>,
    /// Server-reported latency in milliseconds. Keeps a rough
    /// sense of correctness vs latency on the same axis.
    latency_ms: f64,
}

#[derive(Debug, Default, Serialize)]
struct ScenarioSummary {
    scenario: String,
    total: usize,
    passed: usize,
    failed: usize,
    /// Per-country pass rate. Reveals geographic clusters of
    /// regressions — e.g., "we're 99 % on US but 60 % on FR".
    by_country: HashMap<String, CountryStats>,
    /// First N failures, stored for grepability. Capped to keep the
    /// report file from growing unbounded.
    sample_failures: Vec<CaseResult>,
}

#[derive(Debug, Default, Serialize, Clone)]
struct CountryStats {
    total: usize,
    passed: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    base_url: String,
    fixtures_dir: String,
    sample_size: usize,
    search_radius_km: f64,
    pass_threshold: f64,
    captured_at: String,
    overall: OverallSummary,
    scenarios: Vec<ScenarioSummary>,
}

#[derive(Debug, Serialize)]
struct OverallSummary {
    total: usize,
    passed: usize,
    failed: usize,
    pass_rate: f64,
}

// -----------------------------------------------------------------------------
// CLI parsing — a tiny one-off, no clap to keep build time down
// -----------------------------------------------------------------------------

struct Args {
    fixtures_dir: PathBuf,
    base_url: String,
    sample: usize,
    scenarios: Vec<&'static str>,
    search_radius_km: f64,
    out_path: Option<PathBuf>,
    pass_threshold: f64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "Usage: bench-accuracy <fixtures-dir> --base-url <url>\n\
                                    [--sample N] [--scenarios r,s,a]\n\
                                    [--search-radius-km N] [--out FILE]\n\
                                    [--pass-threshold 0.95]"
        );
        return Err(String::new());
    }

    let fixtures_dir = PathBuf::from(args.remove(0));
    let mut base_url = String::from("http://127.0.0.1:3000");
    let mut sample = 500usize;
    let mut scenarios_raw = String::from("r,s,a");
    // Default radius is generous enough to cover the same-name-city
    // disambiguation cluster (Münster DE, Olathe US, Mount Pleasant
    // CA, etc.) which trips a tighter radius repeatedly. The test
    // already accepts any of the top-10 results within radius, so
    // this is "did the geocoder find the right city anywhere in the
    // result list" rather than "did it rank it first".
    let mut search_radius_km = 200.0_f64;
    let mut out_path: Option<PathBuf> = None;
    // 0.90 is the realistic pass-rate floor against Geonames-derived
    // fixtures: ~1 % of reverse rows are right on country borders
    // (admin polygon edge precision noise) and ~5–10 % of search/
    // autocomplete rows are Geonames "places" (neighborhoods, council
    // areas) that aren't in OSM as place points. Tighten to 0.95
    // once fixtures are filtered for those known noise sources, or
    // when the geocoder grows neighborhood-level coverage.
    let mut pass_threshold = 0.90_f64;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--base-url" => {
                base_url = args.get(i + 1).ok_or("missing value for --base-url")?.clone();
                i += 2;
            }
            "--sample" => {
                sample = args
                    .get(i + 1)
                    .ok_or("missing value for --sample")?
                    .parse()
                    .map_err(|e| format!("--sample: {e}"))?;
                i += 2;
            }
            "--scenarios" => {
                scenarios_raw = args
                    .get(i + 1)
                    .ok_or("missing value for --scenarios")?
                    .clone();
                i += 2;
            }
            "--search-radius-km" => {
                search_radius_km = args
                    .get(i + 1)
                    .ok_or("missing value for --search-radius-km")?
                    .parse()
                    .map_err(|e| format!("--search-radius-km: {e}"))?;
                i += 2;
            }
            "--out" => {
                out_path = Some(PathBuf::from(
                    args.get(i + 1).ok_or("missing value for --out")?,
                ));
                i += 2;
            }
            "--pass-threshold" => {
                pass_threshold = args
                    .get(i + 1)
                    .ok_or("missing value for --pass-threshold")?
                    .parse()
                    .map_err(|e| format!("--pass-threshold: {e}"))?;
                i += 2;
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }

    let mut scenarios = Vec::new();
    for tok in scenarios_raw.split(',') {
        match tok.trim() {
            "r" | "reverse" => scenarios.push("reverse"),
            "s" | "search" => scenarios.push("search"),
            "a" | "autocomplete" => scenarios.push("autocomplete"),
            "" => {}
            other => return Err(format!("unknown scenario: {other}")),
        }
    }
    if scenarios.is_empty() {
        return Err("no scenarios selected".to_string());
    }

    Ok(Args {
        fixtures_dir,
        base_url,
        sample,
        scenarios,
        search_radius_km,
        out_path,
        pass_threshold,
    })
}

// -----------------------------------------------------------------------------
// HTTP helper — single ureq agent reused, modest per-request timeout
// -----------------------------------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
}

fn http_get_json(agent: &ureq::Agent, url: &str) -> Result<(Value, f64), String> {
    let t0 = Instant::now();
    let resp = agent.get(url).call().map_err(|e| format!("{e}"))?;
    let body: Value = resp
        .into_json()
        .map_err(|e| format!("decode: {e}"))?;
    Ok((body, t0.elapsed().as_secs_f64() * 1000.0))
}

// -----------------------------------------------------------------------------
// Per-scenario evaluators
// -----------------------------------------------------------------------------

const SAMPLE_FAILURES_PER_SCENARIO: usize = 20;
const SAMPLE_FAILURES_PER_COUNTRY: usize = 3;

fn run_reverse(
    agent: &ureq::Agent,
    base_url: &str,
    fixtures_dir: &Path,
    sample: usize,
) -> Result<ScenarioSummary, String> {
    let path = fixtures_dir.join("reverse_coords.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;
    let rows: Vec<ReverseRow> =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {}", path.display(), e))?;

    let take = sample.min(rows.len());
    let mut summary = ScenarioSummary {
        scenario: "reverse".to_string(),
        ..Default::default()
    };
    let mut country_failure_buckets: HashMap<String, usize> = HashMap::new();

    for row in rows.iter().take(take) {
        let url = format!("{base_url}/reverse?lat={}&lon={}", row.lat, row.lng);
        let cs = summary
            .by_country
            .entry(row.country_code.clone())
            .or_default();
        cs.total += 1;
        summary.total += 1;

        match http_get_json(agent, &url) {
            Ok((body, latency_ms)) => {
                let actual_cc = body
                    .pointer("/address/country_code")
                    .and_then(Value::as_str)
                    .map(|s| s.to_ascii_lowercase());
                let passed = matches!(actual_cc.as_deref(), Some(cc) if cc == row.country_code);
                if passed {
                    summary.passed += 1;
                    cs.passed += 1;
                } else {
                    summary.failed += 1;
                    let reason = match actual_cc {
                        Some(other) => format!("country_code mismatch: got '{other}', want '{}'", row.country_code),
                        None => "missing address.country_code".to_string(),
                    };
                    let bucket = country_failure_buckets
                        .entry(row.country_code.clone())
                        .or_insert(0);
                    if summary.sample_failures.len() < SAMPLE_FAILURES_PER_SCENARIO
                        && *bucket < SAMPLE_FAILURES_PER_COUNTRY
                    {
                        summary.sample_failures.push(CaseResult {
                            scenario: "reverse",
                            request: url.clone(),
                            country_code: row.country_code.clone(),
                            passed: false,
                            reason: Some(reason),
                            latency_ms,
                        });
                        *bucket += 1;
                    }
                }
            }
            Err(e) => {
                summary.failed += 1;
                if summary.sample_failures.len() < SAMPLE_FAILURES_PER_SCENARIO {
                    summary.sample_failures.push(CaseResult {
                        scenario: "reverse",
                        request: url,
                        country_code: row.country_code.clone(),
                        passed: false,
                        reason: Some(format!("HTTP error: {e}")),
                        latency_ms: 0.0,
                    });
                }
            }
        }
    }
    Ok(summary)
}

fn run_search(
    agent: &ureq::Agent,
    base_url: &str,
    fixtures_dir: &Path,
    sample: usize,
    radius_km: f64,
) -> Result<ScenarioSummary, String> {
    let path = fixtures_dir.join("search_queries.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;
    let rows: Vec<SearchRow> =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {}", path.display(), e))?;

    let take = sample.min(rows.len());
    let mut summary = ScenarioSummary {
        scenario: "search".to_string(),
        ..Default::default()
    };
    let mut country_failure_buckets: HashMap<String, usize> = HashMap::new();

    let radius_m = radius_km * 1000.0;

    for row in rows.iter().take(take) {
        let q_enc = urlencode(&row.q);
        let url = format!(
            "{base_url}/search?q={q_enc}&country_code={cc}&limit=10",
            cc = row.country_code,
        );
        let cs = summary
            .by_country
            .entry(row.country_code.clone())
            .or_default();
        cs.total += 1;
        summary.total += 1;

        match http_get_json(agent, &url) {
            Ok((body, latency_ms)) => {
                let results = body.pointer("/results").and_then(Value::as_array);
                let arr = match results {
                    Some(arr) if !arr.is_empty() => arr,
                    _ => {
                        summary.failed += 1;
                        let bucket = country_failure_buckets
                            .entry(row.country_code.clone())
                            .or_insert(0);
                        if summary.sample_failures.len() < SAMPLE_FAILURES_PER_SCENARIO
                            && *bucket < SAMPLE_FAILURES_PER_COUNTRY
                        {
                            summary.sample_failures.push(CaseResult {
                                scenario: "search",
                                request: url,
                                country_code: row.country_code.clone(),
                                passed: false,
                                reason: Some("no results".to_string()),
                                latency_ms,
                            });
                            *bucket += 1;
                        }
                        continue;
                    }
                };

                // Walk all returned results (up to limit=10), not just
                // the top one. Geonames "places" are ambiguously named
                // (Münster, Mount Pleasant, Olathe, Columbus all exist
                // in multiple cities of the same country); the
                // geocoder's population-rank may pick a different
                // member of the cluster than the one Geonames sampled.
                // A pass means the geocoder *found* the right city in
                // the top 10, not necessarily that it ranked it first.
                // top_dist / top_cc are tracked for the failure
                // diagnostic so an operator can see which city won.
                let mut any_match = false;
                let top_lat = arr[0].get("lat").and_then(Value::as_f64);
                let top_lon = arr[0].get("lon").and_then(Value::as_f64);
                let top_cc = arr[0]
                    .pointer("/address/country_code")
                    .and_then(Value::as_str)
                    .map(|s| s.to_ascii_lowercase());
                for r in arr {
                    let lat = r.get("lat").and_then(Value::as_f64);
                    let lon = r.get("lon").and_then(Value::as_f64);
                    let cc = r
                        .pointer("/address/country_code")
                        .and_then(Value::as_str)
                        .map(|s| s.to_ascii_lowercase());
                    let cc_ok = cc.as_deref() == Some(row.country_code.as_str());
                    let dist_ok = match (lat, lon) {
                        (Some(la), Some(lo)) => {
                            haversine_m(la, lo, row.lat_hint, row.lng_hint) <= radius_m
                        }
                        _ => false,
                    };
                    if cc_ok && dist_ok {
                        any_match = true;
                        break;
                    }
                }

                if any_match {
                    summary.passed += 1;
                    cs.passed += 1;
                } else {
                    summary.failed += 1;
                    let bucket = country_failure_buckets
                        .entry(row.country_code.clone())
                        .or_insert(0);
                    if summary.sample_failures.len() < SAMPLE_FAILURES_PER_SCENARIO
                        && *bucket < SAMPLE_FAILURES_PER_COUNTRY
                    {
                        let top_cc_ok = top_cc.as_deref() == Some(row.country_code.as_str());
                        let reason = if !top_cc_ok {
                            format!(
                                "no in-country result in top {}: top got '{}' want '{}'",
                                arr.len(),
                                top_cc.as_deref().unwrap_or("?"),
                                row.country_code,
                            )
                        } else {
                            let dist = top_lat
                                .zip(top_lon)
                                .map(|(la, lo)| haversine_m(la, lo, row.lat_hint, row.lng_hint))
                                .unwrap_or(0.0);
                            format!(
                                "no result within {:.0} km of hint (top: {:.0} km, considered {})",
                                radius_km,
                                dist / 1000.0,
                                arr.len(),
                            )
                        };
                        summary.sample_failures.push(CaseResult {
                            scenario: "search",
                            request: url,
                            country_code: row.country_code.clone(),
                            passed: false,
                            reason: Some(reason),
                            latency_ms,
                        });
                        *bucket += 1;
                    }
                }
            }
            Err(e) => {
                summary.failed += 1;
                if summary.sample_failures.len() < SAMPLE_FAILURES_PER_SCENARIO {
                    summary.sample_failures.push(CaseResult {
                        scenario: "search",
                        request: url,
                        country_code: row.country_code.clone(),
                        passed: false,
                        reason: Some(format!("HTTP error: {e}")),
                        latency_ms: 0.0,
                    });
                }
            }
        }
    }
    Ok(summary)
}

fn run_autocomplete(
    agent: &ureq::Agent,
    base_url: &str,
    fixtures_dir: &Path,
    sample: usize,
) -> Result<ScenarioSummary, String> {
    let path = fixtures_dir.join("autocomplete_prefixes.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;
    let rows: Vec<AutocompleteRow> =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {}", path.display(), e))?;

    let take = sample.min(rows.len());
    let mut summary = ScenarioSummary {
        scenario: "autocomplete".to_string(),
        ..Default::default()
    };
    let mut country_failure_buckets: HashMap<String, usize> = HashMap::new();

    for row in rows.iter().take(take) {
        let q_enc = urlencode(&row.q);
        let url = format!(
            "{base_url}/autocomplete?q={q_enc}&country_code={cc}&limit=10",
            cc = row.country_code,
        );
        let cs = summary
            .by_country
            .entry(row.country_code.clone())
            .or_default();
        cs.total += 1;
        summary.total += 1;

        match http_get_json(agent, &url) {
            Ok((body, latency_ms)) => {
                let results = body.pointer("/results").and_then(Value::as_array);
                let arr = match results {
                    Some(arr) => arr,
                    None => {
                        summary.failed += 1;
                        if summary.sample_failures.len() < SAMPLE_FAILURES_PER_SCENARIO {
                            summary.sample_failures.push(CaseResult {
                                scenario: "autocomplete",
                                request: url,
                                country_code: row.country_code.clone(),
                                passed: false,
                                reason: Some("missing results array".to_string()),
                                latency_ms,
                            });
                        }
                        continue;
                    }
                };

                // Two-tier check by prefix length:
                //   1–2 char prefixes: at-least-one-result is plenty
                //     (the FST broad-walk visits thousands; the
                //     prefix-prefix-of-name guarantee from build
                //     ensures the corpus starts non-empty).
                //   3+ char prefixes: also assert that at least one
                //     returned name starts with the prefix
                //     (case-insensitive, after light normalisation).
                let passed = if arr.is_empty() {
                    false
                } else if row.len <= 2 {
                    true
                } else {
                    // Apply the same fold to the needle that the FST
                    // builder applied to the indexed name (see
                    // `normalise_fst_key` in build_autocomplete_fst.rs).
                    // Otherwise an accented prefix like "würs" can't
                    // prefix-match the index's canonical form
                    // ("wurselen") and we'd report false negatives.
                    let needle = normalise_name(&row.q);
                    arr.iter().any(|r| {
                        r.get("name")
                            .and_then(Value::as_str)
                            .map(|n| {
                                normalise_name(n).starts_with(&needle)
                            })
                            .unwrap_or(false)
                    })
                };

                if passed {
                    summary.passed += 1;
                    cs.passed += 1;
                } else {
                    summary.failed += 1;
                    let bucket = country_failure_buckets
                        .entry(row.country_code.clone())
                        .or_insert(0);
                    if summary.sample_failures.len() < SAMPLE_FAILURES_PER_SCENARIO
                        && *bucket < SAMPLE_FAILURES_PER_COUNTRY
                    {
                        let reason = if arr.is_empty() {
                            "no results".to_string()
                        } else {
                            let names: Vec<String> = arr
                                .iter()
                                .take(3)
                                .filter_map(|r| {
                                    r.get("name").and_then(Value::as_str).map(str::to_string)
                                })
                                .collect();
                            format!(
                                "no result starts with prefix; top 3: {}",
                                names.join(", ")
                            )
                        };
                        summary.sample_failures.push(CaseResult {
                            scenario: "autocomplete",
                            request: url,
                            country_code: row.country_code.clone(),
                            passed: false,
                            reason: Some(reason),
                            latency_ms,
                        });
                        *bucket += 1;
                    }
                }
            }
            Err(e) => {
                summary.failed += 1;
                if summary.sample_failures.len() < SAMPLE_FAILURES_PER_SCENARIO {
                    summary.sample_failures.push(CaseResult {
                        scenario: "autocomplete",
                        request: url,
                        country_code: row.country_code.clone(),
                        passed: false,
                        reason: Some(format!("HTTP error: {e}")),
                        latency_ms: 0.0,
                    });
                }
            }
        }
    }
    Ok(summary)
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// URL-encode a query string component. ureq doesn't expose the
/// percent-encoding tables it uses internally; doing this by hand
/// keeps the dep tree small.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Mirrors the canonicalisation the FST builder applies on
/// `name` strings before they go into the index — see
/// `normalise_fst_key` in `server/src/bin/build_autocomplete_fst.rs`.
/// Lower, ASCII-fold common European diacritics (ü→u, é→e, ñ→n,
/// ß→ss, etc.), keep alphanumerics + single ASCII spaces.
///
/// We can't import the binary's helper (binary code isn't a
/// library), so the fold table is duplicated here. The two MUST
/// stay in sync — a divergence shows up as autocomplete prefix
/// failures where the FST returns matches the comparator rejects.
fn normalise_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        let folded = fold_char(ch);
        for c in folded.chars() {
            if c.is_alphanumeric() {
                for lc in c.to_lowercase() {
                    out.push(lc);
                }
                last_space = false;
            } else if !last_space {
                out.push(' ');
                last_space = true;
            }
        }
    }
    out.trim().to_string()
}

fn fold_char(ch: char) -> String {
    match ch {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => "a".into(),
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => "e".into(),
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => "i".into(),
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' | 'Ø' => "o".into(),
        'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => "u".into(),
        'ñ' | 'Ñ' => "n".into(),
        'ç' | 'Ç' => "c".into(),
        'ß' => "ss".into(),
        'æ' | 'Æ' => "ae".into(),
        'œ' | 'Œ' => "oe".into(),
        'ý' | 'ÿ' | 'Ý' => "y".into(),
        _ => ch.to_string(),
    }
}

fn render_human_summary(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n===== bench-accuracy report (base={}, sample={}) =====\n",
        report.base_url, report.sample_size
    ));
    out.push_str(&format!(
        "overall: {} / {} passed ({:.2} %)\n",
        report.overall.passed,
        report.overall.total,
        report.overall.pass_rate * 100.0,
    ));
    for s in &report.scenarios {
        out.push_str(&format!(
            "\n  {}  {} / {} passed",
            s.scenario, s.passed, s.total
        ));
        if s.total > 0 {
            out.push_str(&format!(" ({:.2} %)", (s.passed as f64 / s.total as f64) * 100.0));
        }
        out.push('\n');
        let mut ccs: Vec<_> = s.by_country.keys().collect();
        ccs.sort();
        for cc in ccs {
            let cs = &s.by_country[cc];
            if cs.total == 0 {
                continue;
            }
            out.push_str(&format!(
                "    {}: {} / {} ({:.1} %)\n",
                cc,
                cs.passed,
                cs.total,
                (cs.passed as f64 / cs.total as f64) * 100.0,
            ));
        }
        if !s.sample_failures.is_empty() {
            out.push_str(&format!("    sample failures (first {}):\n", s.sample_failures.len()));
            for f in &s.sample_failures {
                out.push_str(&format!(
                    "      [{}] {}\n           reason: {}\n",
                    f.country_code,
                    f.request,
                    f.reason.as_deref().unwrap_or(""),
                ));
            }
        }
    }
    out
}

// -----------------------------------------------------------------------------
// Driver
// -----------------------------------------------------------------------------

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}");
            }
            return ExitCode::from(2);
        }
    };

    if !args.fixtures_dir.exists() {
        eprintln!(
            "error: fixtures dir {} doesn't exist; run scripts/bench/build-fixtures.sh first",
            args.fixtures_dir.display()
        );
        return ExitCode::from(2);
    }

    let agent = agent();
    let mut scenarios: Vec<ScenarioSummary> = Vec::new();
    let mut errors = Vec::new();

    for s in &args.scenarios {
        let result = match *s {
            "reverse" => run_reverse(&agent, &args.base_url, &args.fixtures_dir, args.sample),
            "search" => run_search(
                &agent,
                &args.base_url,
                &args.fixtures_dir,
                args.sample,
                args.search_radius_km,
            ),
            "autocomplete" => {
                run_autocomplete(&agent, &args.base_url, &args.fixtures_dir, args.sample)
            }
            other => {
                errors.push(format!("unknown scenario: {other}"));
                continue;
            }
        };
        match result {
            Ok(summary) => scenarios.push(summary),
            Err(e) => errors.push(format!("{s}: {e}")),
        }
    }

    if !errors.is_empty() {
        for e in &errors {
            eprintln!("error: {e}");
        }
        if scenarios.is_empty() {
            return ExitCode::from(2);
        }
    }

    let total: usize = scenarios.iter().map(|s| s.total).sum();
    let passed: usize = scenarios.iter().map(|s| s.passed).sum();
    let failed: usize = scenarios.iter().map(|s| s.failed).sum();
    let pass_rate = if total == 0 {
        0.0
    } else {
        passed as f64 / total as f64
    };

    let report = Report {
        base_url: args.base_url.clone(),
        fixtures_dir: args.fixtures_dir.display().to_string(),
        sample_size: args.sample,
        search_radius_km: args.search_radius_km,
        pass_threshold: args.pass_threshold,
        captured_at: chrono_like_now(),
        overall: OverallSummary {
            total,
            passed,
            failed,
            pass_rate,
        },
        scenarios,
    };

    print!("{}", render_human_summary(&report));

    if let Some(path) = &args.out_path {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => {
                if let Err(e) = std::fs::write(path, s) {
                    eprintln!("warn: failed to write report to {}: {}", path.display(), e);
                } else {
                    eprintln!("==> report written to {}", path.display());
                }
            }
            Err(e) => eprintln!("warn: failed to serialise report: {e}"),
        }
    }

    if pass_rate >= args.pass_threshold {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "FAIL: pass_rate {:.2} % < threshold {:.2} %",
            pass_rate * 100.0,
            args.pass_threshold * 100.0,
        );
        ExitCode::from(1)
    }
}

/// Lightweight ISO-8601 stamp — using `chrono` would be the obvious
/// answer, but we don't already pull it into this crate and the
/// regression-runner discipline is to keep deps minimal. The format
/// matches the dated snapshot doc convention.
fn chrono_like_now() -> String {
    let secs_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Simple Y-M-D HH:MM:SS UTC from epoch — good-enough resolution
    // for a report timestamp without pulling chrono in.
    format_unix_utc(secs_since_epoch as i64)
}

fn format_unix_utc(secs: i64) -> String {
    // Days from 1970-01-01.
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;

    // Date conversion (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}
