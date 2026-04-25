//! Black-box regression runner for the geocoder.
//!
//! Reads a JSON corpus file, fires one HTTP request per case at a
//! running server, evaluates a list of declarative expectations, and
//! emits a machine-readable report plus a human summary. Exits 0 when
//! every case passes, non-zero otherwise.
//!
//! Invoked by `scripts/run-regression.sh`, which owns the
//! start/stop-server lifecycle. This binary deliberately knows nothing
//! about the server process — it only speaks HTTP.

// mimalloc global allocator — see build-pipeline-perf-plan stage 2.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Corpus schema
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Corpus {
    #[serde(default)]
    name: String,
    /// Free-form description of the corpus. Not rendered today but
    /// kept so tooling can echo it in per-corpus banners.
    #[allow(dead_code)]
    #[serde(default)]
    description: String,
    #[serde(default)]
    defaults: Defaults,
    cases: Vec<Case>,
}

#[derive(Debug, Default, Deserialize)]
struct Defaults {
    /// Applied to `?key=` of every request when the case doesn't set
    /// `auth: false`. Saves writing the token into every case.
    #[serde(default)]
    auth_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    #[serde(default)]
    tags: Vec<String>,
    request: Request,
    expect: Expect,
    /// When `false`, do not append the default auth_key — used for the
    /// health endpoints and any future unauthenticated routes.
    #[serde(default = "default_true")]
    auth: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default = "default_get")]
    method: String,
    path: String,
    #[serde(default)]
    query: HashMap<String, String>,
}

fn default_get() -> String {
    "GET".to_string()
}

#[derive(Debug, Deserialize)]
struct Expect {
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    checks: Vec<Check>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Check {
    /// Equality against a JSON path (dot-notation, supports numeric
    /// indexes: `results.0.name`).
    JsonPathEquals { path: String, value: Value },
    /// Case-insensitive substring match.
    JsonPathContainsCi { path: String, value: String },
    /// Path resolves to a non-null value.
    JsonPathExists { path: String },
    /// Path missing or null.
    JsonPathAbsent { path: String },
    /// Array at path has at least N elements.
    JsonPathMinCount { path: String, min: usize },
    /// Haversine distance between the coord at
    /// (`lat_path`, `lng_path`) and the expected lat/lng is ≤ `max_m`
    /// metres. Uses WGS-84-on-a-sphere, which is accurate to a fraction
    /// of a percent at any distance small enough to matter for geocoder
    /// tolerances.
    CoordWithinM {
        lat_path: String,
        lng_path: String,
        expected_lat: f64,
        expected_lng: f64,
        max_m: f64,
    },
}

// -----------------------------------------------------------------------------
// Report
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Report {
    base_url: String,
    corpus: String,
    summary: Summary,
    cases: Vec<CaseReport>,
}

#[derive(Debug, Serialize)]
struct Summary {
    total: usize,
    passed: usize,
    failed: usize,
    /// Elapsed wall-time in milliseconds, across all cases.
    wall_ms: u64,
}

#[derive(Debug, Serialize)]
struct CaseReport {
    id: String,
    tags: Vec<String>,
    passed: bool,
    elapsed_ms: u64,
    http_status: Option<u16>,
    failures: Vec<String>,
}

// -----------------------------------------------------------------------------
// CLI args
// -----------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Args {
    corpus: PathBuf,
    base_url: String,
    report: Option<PathBuf>,
    filter: Option<String>,
    quiet: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        base_url: "http://127.0.0.1:3000".to_string(),
        ..Args::default()
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--corpus" => {
                args.corpus = PathBuf::from(raw.get(i + 1).ok_or("--corpus needs a path")?);
                i += 2;
            }
            "--base-url" => {
                args.base_url = raw.get(i + 1).ok_or("--base-url needs a value")?.clone();
                i += 2;
            }
            "--report" => {
                args.report = Some(PathBuf::from(
                    raw.get(i + 1).ok_or("--report needs a path")?,
                ));
                i += 2;
            }
            "--filter" => {
                args.filter = Some(raw.get(i + 1).ok_or("--filter needs a value")?.clone());
                i += 2;
            }
            "--quiet" | "-q" => {
                args.quiet = true;
                i += 1;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    if args.corpus.as_os_str().is_empty() {
        return Err("--corpus is required".into());
    }
    Ok(args)
}

fn print_help() {
    println!(
        "regression-runner — black-box geocoder regression tests\n\n\
         USAGE:\n  \
         regression-runner --corpus <path> [--base-url URL] [--report PATH] [--filter SUB] [--quiet]\n\n\
         FLAGS:\n  \
         --corpus    JSON corpus file (required)\n  \
         --base-url  geocoder URL (default http://127.0.0.1:3000)\n  \
         --report    write a machine-readable JSON report\n  \
         --filter    run only cases whose id or tag contains the given substring\n  \
         --quiet     suppress per-case stdout; only emit the summary\n"
    );
}

// -----------------------------------------------------------------------------
// Main
// -----------------------------------------------------------------------------

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let corpus_bytes = match std::fs::read(&args.corpus) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: read corpus {}: {e}", args.corpus.display());
            return ExitCode::from(2);
        }
    };
    let corpus: Corpus = match serde_json::from_slice(&corpus_bytes) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: parse corpus: {e}");
            return ExitCode::from(2);
        }
    };

    if !args.quiet {
        println!(
            "== regression-runner ==\n  corpus:   {} ({} cases)\n  base_url: {}\n",
            corpus.name, corpus.cases.len(), args.base_url
        );
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();

    let t0 = Instant::now();
    let mut reports: Vec<CaseReport> = Vec::with_capacity(corpus.cases.len());
    let mut passed = 0usize;
    let mut failed = 0usize;

    for case in &corpus.cases {
        if let Some(filter) = args.filter.as_deref() {
            if !case.id.contains(filter) && !case.tags.iter().any(|t| t.contains(filter)) {
                continue;
            }
        }
        let (report, ok) = run_case(&agent, &args.base_url, &corpus.defaults, case);
        if ok {
            passed += 1;
            if !args.quiet {
                println!("  [PASS] {}  ({} ms)", report.id, report.elapsed_ms);
            }
        } else {
            failed += 1;
            println!("  [FAIL] {}  ({} ms)", report.id, report.elapsed_ms);
            for f in &report.failures {
                println!("         - {f}");
            }
        }
        reports.push(report);
    }

    let wall_ms = t0.elapsed().as_millis() as u64;
    let summary = Summary {
        total: passed + failed,
        passed,
        failed,
        wall_ms,
    };
    let report = Report {
        base_url: args.base_url.clone(),
        corpus: corpus.name.clone(),
        summary,
        cases: reports,
    };

    println!(
        "\n== summary ==\n  total:  {}\n  passed: {}\n  failed: {}\n  wall:   {} ms",
        report.summary.total, report.summary.passed, report.summary.failed, report.summary.wall_ms
    );

    if let Some(path) = args.report.as_ref() {
        match serde_json::to_string_pretty(&report).and_then(|s| {
            std::fs::write(path, s).map_err(serde_json::Error::io)
        }) {
            Ok(()) => {
                if !args.quiet {
                    println!("  report: {}", path.display());
                }
            }
            Err(e) => eprintln!("warning: failed to write report: {e}"),
        }
    }

    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// -----------------------------------------------------------------------------
// Per-case execution
// -----------------------------------------------------------------------------

fn run_case(
    agent: &ureq::Agent,
    base_url: &str,
    defaults: &Defaults,
    case: &Case,
) -> (CaseReport, bool) {
    let t0 = Instant::now();
    let url = format!("{}{}", base_url.trim_end_matches('/'), case.request.path);

    let mut req = match case.request.method.as_str() {
        "GET" => agent.get(&url),
        other => {
            return (
                CaseReport {
                    id: case.id.clone(),
                    tags: case.tags.clone(),
                    passed: false,
                    elapsed_ms: 0,
                    http_status: None,
                    failures: vec![format!("unsupported method {other}")],
                },
                false,
            );
        }
    };

    for (k, v) in &case.request.query {
        req = req.query(k, v);
    }
    if case.auth {
        if let Some(key) = defaults.auth_key.as_ref() {
            if !case.request.query.contains_key("key") {
                req = req.query("key", key);
            }
        }
    }

    let (status, body_json) = match req.call() {
        Ok(resp) => {
            let status = resp.status();
            let body: Value = resp.into_json().unwrap_or(Value::Null);
            (status, body)
        }
        Err(ureq::Error::Status(status, resp)) => {
            let body: Value = resp.into_json().unwrap_or(Value::Null);
            (status, body)
        }
        Err(e) => {
            return (
                CaseReport {
                    id: case.id.clone(),
                    tags: case.tags.clone(),
                    passed: false,
                    elapsed_ms: t0.elapsed().as_millis() as u64,
                    http_status: None,
                    failures: vec![format!("request failed: {e}")],
                },
                false,
            );
        }
    };

    let mut failures: Vec<String> = Vec::new();

    if let Some(expected) = case.expect.status {
        if status != expected {
            failures.push(format!("status: expected {expected}, got {status}"));
        }
    }

    for check in &case.expect.checks {
        if let Err(msg) = evaluate_check(&body_json, check) {
            failures.push(msg);
        }
    }

    let passed = failures.is_empty();
    (
        CaseReport {
            id: case.id.clone(),
            tags: case.tags.clone(),
            passed,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            http_status: Some(status),
            failures,
        },
        passed,
    )
}

// -----------------------------------------------------------------------------
// Check evaluation
// -----------------------------------------------------------------------------

fn evaluate_check(body: &Value, check: &Check) -> Result<(), String> {
    match check {
        Check::JsonPathEquals { path, value } => {
            let actual = json_path_lookup(body, path)
                .ok_or_else(|| format!("path `{path}` missing"))?;
            if actual != value {
                return Err(format!(
                    "json_path_equals `{path}`: expected {value}, got {actual}"
                ));
            }
            Ok(())
        }
        Check::JsonPathContainsCi { path, value } => {
            let actual = json_path_lookup(body, path)
                .ok_or_else(|| format!("path `{path}` missing"))?;
            let s = actual
                .as_str()
                .ok_or_else(|| format!("path `{path}` is not a string (for contains_ci)"))?;
            if !s.to_ascii_lowercase().contains(&value.to_ascii_lowercase()) {
                return Err(format!(
                    "json_path_contains_ci `{path}`: {s:?} does not contain {value:?}"
                ));
            }
            Ok(())
        }
        Check::JsonPathExists { path } => {
            let v = json_path_lookup(body, path);
            match v {
                None | Some(Value::Null) => Err(format!("json_path_exists `{path}`: missing/null")),
                _ => Ok(()),
            }
        }
        Check::JsonPathAbsent { path } => match json_path_lookup(body, path) {
            None | Some(Value::Null) => Ok(()),
            Some(v) => Err(format!("json_path_absent `{path}`: got {v}")),
        },
        Check::JsonPathMinCount { path, min } => {
            let v = json_path_lookup(body, path)
                .ok_or_else(|| format!("path `{path}` missing"))?;
            let arr = v
                .as_array()
                .ok_or_else(|| format!("path `{path}` is not an array"))?;
            if arr.len() < *min {
                return Err(format!(
                    "json_path_min_count `{path}`: expected >= {min}, got {}",
                    arr.len()
                ));
            }
            Ok(())
        }
        Check::CoordWithinM {
            lat_path,
            lng_path,
            expected_lat,
            expected_lng,
            max_m,
        } => {
            let lat = json_path_number(body, lat_path)
                .ok_or_else(|| format!("lat path `{lat_path}` missing or not a number"))?;
            let lng = json_path_number(body, lng_path)
                .ok_or_else(|| format!("lng path `{lng_path}` missing or not a number"))?;
            let d = haversine_m(lat, lng, *expected_lat, *expected_lng);
            if d > *max_m {
                return Err(format!(
                    "coord_within_m: actual ({lat:.5}, {lng:.5}) is {d:.0} m from expected ({expected_lat:.5}, {expected_lng:.5}) — max {max_m:.0} m"
                ));
            }
            Ok(())
        }
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Dot-notation path lookup over a `serde_json::Value`. Numeric segments
/// index into arrays; string segments into objects. `results.0.name` →
/// `value["results"][0]["name"]`.
fn json_path_lookup<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        if let Ok(idx) = seg.parse::<usize>() {
            cur = cur.as_array()?.get(idx)?;
        } else {
            cur = cur.as_object()?.get(seg)?;
        }
    }
    Some(cur)
}

fn json_path_number(root: &Value, path: &str) -> Option<f64> {
    json_path_lookup(root, path).and_then(|v| v.as_f64())
}

/// Spherical-earth great-circle distance in metres. Accurate to ~0.3 %
/// at any distance; plenty for geocoder tolerances which are measured
/// in 10s–1000s of metres.
fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lng2 - lng1).to_radians();
    let a = (dphi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_known_sydney_melbourne() {
        // Sydney Opera House to Flinders Street Station, Melbourne.
        let d = haversine_m(-33.8568, 151.2153, -37.8183, 144.9671);
        // Real distance ~713 km. Spherical approximation is within a
        // few km of the ellipsoidal value.
        assert!((d - 713_000.0).abs() < 5_000.0, "got {d}");
    }

    #[test]
    fn json_path_lookup_numeric_and_string() {
        let v: Value = serde_json::from_str(
            r#"{"results":[{"name":"Sydney","lat":-33.87}]}"#,
        )
        .expect("test fixture parses");
        assert_eq!(
            json_path_lookup(&v, "results.0.name"),
            Some(&Value::String("Sydney".to_string()))
        );
        assert_eq!(json_path_number(&v, "results.0.lat"), Some(-33.87));
        assert_eq!(json_path_lookup(&v, "results.7.name"), None);
    }
}
