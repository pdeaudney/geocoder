//! OpenAddresses fetch via AWS S3 Requester-Pays.
//!
//! Two-step flow:
//!
//!   1. List sources via the OA batch API:
//!      `https://batch.openaddresses.io/api/data?layer=addresses[&source=PREFIX]`
//!      Returns JSON: `[{"job":N, "source":"...", "output":{"cache":bool}}, ...]`
//!   2. For each row, GetObject from `s3://v2.openaddresses.io/jobs/<job>/<key>`
//!      with `request_payer=requester`. Prefer `cache.zip`; fall back to
//!      `source.geojson.gz` when the cache flag is false.
//!
//! Output filenames flatten the OA source path with `_` instead of
//! `/` (e.g. `us/ca/san_francisco` → `us_ca_san_francisco.cache.zip`)
//! so the `build-openaddresses-index` glob over `*.cache.zip` /
//! `*.source.geojson.gz` keeps working.
//!
//! ## AWS auth
//!
//! Uses the standard AWS credential chain (env, profile, EC2 IMDS).
//! Resolution is **lazy** — we don't probe creds at startup. On EC2,
//! IMDS lookup is slow enough that an eager probe could race with
//! IAM-role propagation and falsely conclude "no credentials." The
//! SDK caches credentials internally; the first real S3 call exercises
//! the chain through the actual code path. If that call fails with a
//! credential error the row's `OaResult` surfaces it; subsequent rows
//! either succeed (creds appeared) or fail individually with the same
//! error.
//!
//! ## Sidecar files
//!
//! Per cache.zip / source.geojson.gz we persist `<dest>.s3meta`:
//!
//! ```text
//! etag="..."
//! length=12345
//! ```
//!
//! This is the OA equivalent of the OSM path's `<dest>.etag` sidecar.
//! Re-runs check `head_object` against the sidecar's etag to avoid
//! re-downloading; partial downloads resume via `Range` + `if_match`.
//! Stale sidecars (server etag changed) trigger a full re-download.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use aws_sdk_s3::config::Region as AwsRegion;
use aws_sdk_s3::types::RequestPayer;
use aws_sdk_s3::Client as S3Client;
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::fetcher::http::FetchOpts;
use crate::fetcher::FetchOutcome;

const OA_API: &str = "https://batch.openaddresses.io/api/data?layer=addresses";
const OA_BUCKET: &str = "v2.openaddresses.io";
const OA_BUCKET_REGION: &str = "us-east-1";

#[derive(Debug)]
pub struct OaResult {
    pub dest: PathBuf,
    pub outcome: Result<FetchOutcome>,
}

#[derive(Debug, Deserialize, Clone)]
struct OaApiRow {
    job: Option<u64>,
    source: Option<String>,
    output: Option<OaOutput>,
}

#[derive(Debug, Deserialize, Clone)]
struct OaOutput {
    cache: Option<bool>,
}

/// Sidecar holding the S3 etag + content-length of the most recent
/// successful download. Used for conditional GET (304-style) and
/// resumable downloads via S3's `if_match`.
#[derive(Debug, Clone)]
struct S3Meta {
    etag: String,
    length: u64,
}

impl S3Meta {
    fn parse(s: &str) -> Option<Self> {
        let mut etag: Option<String> = None;
        let mut length: Option<u64> = None;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("etag=") {
                etag = Some(rest.trim().trim_matches('"').to_string());
            } else if let Some(rest) = line.strip_prefix("length=") {
                length = rest.trim().parse::<u64>().ok();
            }
        }
        Some(Self {
            etag: etag?,
            length: length?,
        })
    }

    fn render(&self) -> String {
        format!("etag=\"{}\"\nlength={}\n", self.etag.trim_matches('"'), self.length)
    }
}

pub async fn fetch_openaddresses(
    http: &Client,
    data_dir: &Path,
    sources: &str,
    parallel: usize,
    opts: &FetchOpts,
) -> Result<Vec<OaResult>> {
    let out_dir = data_dir.join("openaddresses");
    tokio::fs::create_dir_all(&out_dir).await?;

    // Build the S3 client with no eager credential probe. Credentials
    // resolve lazily through the standard chain on the first S3 call.
    let s3 = Arc::new(build_s3_client().await?);

    // Resolve scope filter. Empty / "all" → one global listing; otherwise
    // we fan listings out per prefix in parallel (I8).
    let prefixes = parse_scopes(sources);
    let rows = list_rows_parallel(http, &prefixes, parallel)
        .await
        .context("listing OA sources")?;

    // Fan out per-row downloads under a tokio Semaphore.
    let semaphore = Arc::new(Semaphore::new(parallel.max(1)));
    let mut tasks: FuturesUnordered<_> = FuturesUnordered::new();
    for row in rows {
        let permit = semaphore.clone().acquire_owned().await?;
        let s3 = s3.clone();
        let out_dir_inner = out_dir.clone();
        let opts = opts.clone();
        // Capture per-row identity outside the task so panic messages
        // include `source` + `job` instead of an opaque "task panicked"
        // (I12).
        let row_label = format!(
            "source={} job={}",
            row.source.as_deref().unwrap_or("?"),
            row.job.map(|j| j.to_string()).unwrap_or_else(|| "?".into()),
        );
        tasks.push(async move {
            let _permit = permit;
            let label = row_label.clone();
            let joined = tokio::spawn(async move {
                download_oa_row(s3.as_ref(), &out_dir_inner, row, &opts).await
            })
            .await;
            (label, joined)
        });
    }

    let mut results = Vec::new();
    while let Some((label, joined)) = tasks.next().await {
        match joined {
            Ok(r) => results.push(r),
            Err(e) => results.push(OaResult {
                dest: out_dir.clone(),
                outcome: Err(anyhow!("OA download task panicked [{label}]: {e}")),
            }),
        }
    }
    Ok(results)
}

fn parse_scopes(sources: &str) -> Vec<String> {
    let s = sources.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("all") {
        return vec![];
    }
    s.split([' ', ','])
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

async fn build_s3_client() -> Result<S3Client> {
    use aws_config::BehaviorVersion;

    // Lazy credential resolution — see the module-level "AWS auth"
    // section. The SDK's chain handles env, profile, and IMDS;
    // probing eagerly here would race against IMDS-lookup latency
    // on freshly-attached EC2 IAM roles (I6).
    let cfg = aws_config::defaults(BehaviorVersion::latest())
        .region(AwsRegion::new(OA_BUCKET_REGION))
        .load()
        .await;
    Ok(S3Client::new(&cfg))
}

/// List OA sources for one or more prefixes, in parallel. Empty
/// prefix list yields one global listing.
async fn list_rows_parallel(
    http: &Client,
    prefixes: &[String],
    parallel: usize,
) -> Result<Vec<OaApiRow>> {
    if prefixes.is_empty() {
        return list_oa_sources(http, None).await;
    }

    let semaphore = Arc::new(Semaphore::new(parallel.max(1)));
    let mut tasks: FuturesUnordered<_> = FuturesUnordered::new();
    for prefix in prefixes {
        let permit = semaphore.clone().acquire_owned().await?;
        let http = http.clone();
        let prefix_owned = prefix.clone();
        tasks.push(async move {
            let _permit = permit;
            (prefix_owned.clone(), list_oa_sources(&http, Some(&prefix_owned)).await)
        });
    }

    let mut all_rows = Vec::new();
    while let Some((prefix, result)) = tasks.next().await {
        let chunk = result.with_context(|| format!("listing OA sources for prefix '{prefix}'"))?;
        all_rows.extend(chunk);
    }
    Ok(all_rows)
}

async fn list_oa_sources(http: &Client, prefix: Option<&str>) -> Result<Vec<OaApiRow>> {
    let url = match prefix {
        Some(p) => format!("{OA_API}&source={p}"),
        None => OA_API.to_string(),
    };
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {url}"))?;
    let rows: Vec<OaApiRow> = resp.json().await.context("parsing OA API JSON")?;
    Ok(rows)
}

async fn download_oa_row(
    s3: &S3Client,
    out_dir: &Path,
    row: OaApiRow,
    opts: &FetchOpts,
) -> OaResult {
    let Some(job) = row.job else {
        return OaResult {
            dest: out_dir.to_path_buf(),
            outcome: Err(anyhow!("OA API row missing 'job' field")),
        };
    };
    let source = row.source.unwrap_or_default();
    let has_cache = row.output.as_ref().and_then(|o| o.cache).unwrap_or(false);

    let safe = source.replace('/', "_");
    let (key, dest, label) = if has_cache {
        let key = format!("jobs/{job}/cache.zip");
        let dest = out_dir.join(format!("{safe}.cache.zip"));
        (key, dest, "cache.zip")
    } else {
        let key = format!("jobs/{job}/source.geojson.gz");
        let dest = out_dir.join(format!("{safe}.source.geojson.gz"));
        (key, dest, "source.geojson.gz")
    };

    let outcome = fetch_one(s3, &key, &dest, &source, label, job, opts).await;
    OaResult { dest, outcome }
}

/// Per-key fetch with sidecar-driven cache + resume (I7, I9).
async fn fetch_one(
    s3: &S3Client,
    key: &str,
    dest: &Path,
    source: &str,
    label: &str,
    job: u64,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    let s3meta_path = with_extension(dest, "s3meta");
    let partial_path = with_extension(dest, "partial");

    if opts.force {
        let _ = tokio::fs::remove_file(&s3meta_path).await;
        let _ = tokio::fs::remove_file(&partial_path).await;
    }

    // Cached path: dest + sidecar both present, head_object's etag
    // matches the saved one. Avoids re-downloading megabytes when
    // the upstream object hasn't changed.
    if !opts.force && dest.exists() {
        if let Some(saved) = read_s3meta(&s3meta_path).await {
            match s3
                .head_object()
                .bucket(OA_BUCKET)
                .key(key)
                .request_payer(RequestPayer::Requester)
                .send()
                .await
            {
                Ok(head) => {
                    let server_etag = normalize_etag(head.e_tag.as_deref().unwrap_or(""));
                    if !server_etag.is_empty() && server_etag == saved.etag {
                        return Ok(FetchOutcome::Cached);
                    }
                }
                Err(e) => {
                    // head_object failure (creds, network, 404) — fall
                    // through to the full GetObject path which will
                    // surface a clearer error.
                    tracing::debug!(
                        ?e,
                        key,
                        "head_object failed; falling through to full GetObject"
                    );
                }
            }
        }
    }

    // Resume path: a leftover .partial + a sidecar means a prior run
    // was killed mid-stream. Send `if_match=<saved-etag>` + `range`;
    // S3 returns 412 if the etag changed (we restart) or 206 if still
    // valid (we append).
    if opts.resume && partial_path.exists() && !opts.force {
        if let Some(saved) = read_s3meta(&s3meta_path).await {
            let partial_size = tokio::fs::metadata(&partial_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            if partial_size > 0 && partial_size < saved.length {
                match resume_s3(
                    s3,
                    key,
                    &partial_path,
                    &s3meta_path,
                    dest,
                    partial_size,
                    &saved,
                    source,
                    label,
                    job,
                )
                .await
                {
                    Ok(outcome) => return Ok(outcome),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            key,
                            "S3 resume failed; falling back to full download"
                        );
                        let _ = tokio::fs::remove_file(&partial_path).await;
                    }
                }
            } else {
                // Size mismatch (over- or zero-sized partial); restart.
                let _ = tokio::fs::remove_file(&partial_path).await;
            }
        } else {
            // No sidecar = no If-Match protection → can't safely
            // resume (mirrors the http.rs C1 invariant).
            let _ = tokio::fs::remove_file(&partial_path).await;
        }
    }

    tracing::info!(source, job, label, dest = %dest.display(), "OA download");
    full_get_to_file(s3, key, dest, &partial_path, &s3meta_path).await
}

async fn full_get_to_file(
    s3: &S3Client,
    key: &str,
    dest: &Path,
    partial_path: &Path,
    s3meta_path: &Path,
) -> Result<FetchOutcome> {
    let resp = s3
        .get_object()
        .bucket(OA_BUCKET)
        .key(key)
        .request_payer(RequestPayer::Requester)
        .send()
        .await
        .with_context(|| format!("S3 GET s3://{OA_BUCKET}/{key}"))?;

    let etag = normalize_etag(resp.e_tag.as_deref().unwrap_or(""));
    let mut writer = tokio::fs::File::create(partial_path)
        .await
        .with_context(|| format!("creating {}", partial_path.display()))?;

    let mut body = resp.body;
    let mut total = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("reading S3 object body")?;
        writer.write_all(&chunk).await?;
        total += chunk.len() as u64;
    }
    writer.sync_all().await?;
    drop(writer);

    if !etag.is_empty() {
        let meta = S3Meta { etag, length: total };
        let _ = tokio::fs::write(s3meta_path, meta.render()).await;
    }
    tokio::fs::rename(partial_path, dest)
        .await
        .with_context(|| format!("rename {} -> {}", partial_path.display(), dest.display()))?;
    Ok(FetchOutcome::Downloaded { bytes: total })
}

#[allow(clippy::too_many_arguments)]
async fn resume_s3(
    s3: &S3Client,
    key: &str,
    partial_path: &Path,
    s3meta_path: &Path,
    dest: &Path,
    partial_size: u64,
    saved: &S3Meta,
    source: &str,
    label: &str,
    job: u64,
) -> Result<FetchOutcome> {
    tracing::info!(
        source, job, label,
        partial_size, total = saved.length,
        "OA resume from partial"
    );
    let resp = s3
        .get_object()
        .bucket(OA_BUCKET)
        .key(key)
        .request_payer(RequestPayer::Requester)
        .if_match(format!("\"{}\"", saved.etag))
        .range(format!("bytes={partial_size}-"))
        .send()
        .await
        .with_context(|| format!("S3 ranged GET s3://{OA_BUCKET}/{key}"))?;

    let mut writer = tokio::fs::OpenOptions::new()
        .append(true)
        .create(false)
        .open(partial_path)
        .await
        .with_context(|| format!("appending to {}", partial_path.display()))?;

    let mut body = resp.body;
    let mut appended = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("reading S3 object body")?;
        writer.write_all(&chunk).await?;
        appended += chunk.len() as u64;
    }
    writer.sync_all().await?;
    drop(writer);

    let total = partial_size + appended;
    if total != saved.length {
        let _ = tokio::fs::remove_file(partial_path).await;
        return Err(anyhow!(
            "OA resume length mismatch: got {total} bytes, sidecar says {expected}",
            expected = saved.length
        ));
    }
    // Etag is unchanged (`if_match` succeeded), so the sidecar stays
    // valid; just rename and we're done.
    let _ = s3meta_path; // silence unused warn — etag carries over
    tokio::fs::rename(partial_path, dest)
        .await
        .with_context(|| format!("rename {} -> {}", partial_path.display(), dest.display()))?;
    Ok(FetchOutcome::Resumed {
        bytes: appended,
        total,
    })
}

async fn read_s3meta(path: &Path) -> Option<S3Meta> {
    let body = tokio::fs::read_to_string(path).await.ok()?;
    S3Meta::parse(&body)
}

fn normalize_etag(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

fn with_extension(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scopes_all_or_empty_yields_global() {
        assert!(parse_scopes("").is_empty());
        assert!(parse_scopes("all").is_empty());
        assert!(parse_scopes("ALL").is_empty());
        assert!(parse_scopes("  ").is_empty());
    }

    #[test]
    fn parse_scopes_country_list() {
        assert_eq!(parse_scopes("au nz fj"), vec!["au", "nz", "fj"]);
        assert_eq!(parse_scopes("au, nz,fj"), vec!["au", "nz", "fj"]);
    }

    #[test]
    fn s3meta_roundtrip() {
        let meta = S3Meta {
            etag: "abc123".to_string(),
            length: 4096,
        };
        let parsed = S3Meta::parse(&meta.render()).expect("parse roundtrip");
        assert_eq!(parsed.etag, meta.etag);
        assert_eq!(parsed.length, meta.length);
    }

    #[test]
    fn s3meta_handles_quoted_etag() {
        let s = "etag=\"abc-123\"\nlength=42\n";
        let parsed = S3Meta::parse(s).expect("parse");
        assert_eq!(parsed.etag, "abc-123");
        assert_eq!(parsed.length, 42);
    }

    #[test]
    fn s3meta_missing_field_returns_none() {
        assert!(S3Meta::parse("etag=abc").is_none());
        assert!(S3Meta::parse("length=42").is_none());
        assert!(S3Meta::parse("").is_none());
    }
}
