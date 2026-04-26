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
//! AWS auth uses the standard credential chain (env, profile, EC2
//! IMDS). Skip with a graceful warning when no creds are available.

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

#[derive(Debug, Deserialize)]
struct OaApiRow {
    job: Option<u64>,
    source: Option<String>,
    output: Option<OaOutput>,
}

#[derive(Debug, Deserialize)]
struct OaOutput {
    cache: Option<bool>,
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

    // Build the S3 client. If the credential chain is empty, fall
    // through gracefully — the orchestrator surfaces a Skipped
    // outcome rather than failing the whole fetch.
    let s3 = match build_s3_client().await {
        Ok(c) => c,
        Err(e) => {
            return Ok(vec![OaResult {
                dest: out_dir.clone(),
                outcome: Ok(FetchOutcome::Skipped {
                    reason: format!("no AWS credentials available: {e}"),
                }),
            }])
        }
    };

    // Resolve scope filter.
    let prefixes = parse_scopes(sources);
    let mut rows: Vec<OaApiRow> = Vec::new();
    if prefixes.is_empty() {
        // "all" / empty — list everything globally.
        rows.extend(list_oa_sources(http, None).await.context("listing OA sources")?);
    } else {
        for prefix in &prefixes {
            let chunk = list_oa_sources(http, Some(prefix.as_str()))
                .await
                .with_context(|| format!("listing OA sources for prefix '{prefix}'"))?;
            rows.extend(chunk);
        }
    }

    // Fan out per-row downloads under a tokio Semaphore.
    let semaphore = Arc::new(Semaphore::new(parallel.max(1)));
    let s3 = Arc::new(s3);
    let mut tasks: FuturesUnordered<_> = FuturesUnordered::new();
    for row in rows {
        let permit = semaphore.clone().acquire_owned().await?;
        let s3 = s3.clone();
        let out_dir = out_dir.clone();
        let opts = opts.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            download_oa_row(s3.as_ref(), &out_dir, row, &opts).await
        }));
    }

    let mut results = Vec::new();
    while let Some(joined) = tasks.next().await {
        match joined {
            Ok(r) => results.push(r),
            Err(e) => results.push(OaResult {
                dest: out_dir.clone(),
                outcome: Err(anyhow!("OA download task panicked: {e}")),
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

    let cfg = aws_config::defaults(BehaviorVersion::latest())
        .region(AwsRegion::new(OA_BUCKET_REGION))
        .load()
        .await;
    // Fail fast if no credentials are configured — the chain returns
    // a NoCredentials provider rather than panicking, so we probe it
    // here so the orchestrator can show a helpful "skipped" message.
    let credentials = cfg
        .credentials_provider()
        .ok_or_else(|| anyhow!("no AWS credentials provider"))?;
    use aws_credential_types::provider::ProvideCredentials;
    credentials
        .provide_credentials()
        .await
        .context("AWS credential chain returned no credentials")?;
    Ok(S3Client::new(&cfg))
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
    let has_cache = row
        .output
        .as_ref()
        .and_then(|o| o.cache)
        .unwrap_or(false);

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

    if dest.exists() && !opts.force {
        return OaResult {
            dest,
            outcome: Ok(FetchOutcome::Cached),
        };
    }

    tracing::info!(source = %source, job, label, dest = %dest.display(), "OA download");
    match s3_get_to_file(s3, &key, &dest).await {
        Ok(bytes) => OaResult {
            dest,
            outcome: Ok(FetchOutcome::Downloaded { bytes }),
        },
        Err(e) => OaResult {
            dest,
            outcome: Err(e),
        },
    }
}

async fn s3_get_to_file(s3: &S3Client, key: &str, dest: &Path) -> Result<u64> {
    let resp = s3
        .get_object()
        .bucket(OA_BUCKET)
        .key(key)
        .request_payer(RequestPayer::Requester)
        .send()
        .await
        .with_context(|| format!("S3 GET s3://{OA_BUCKET}/{key}"))?;

    let partial = with_extension(dest, "partial");
    let mut writer = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;

    // Stream the body chunk-by-chunk so large objects (cache.zip
    // can be hundreds of MB) don't sit in RAM. ByteStream::next()
    // yields `Option<Result<Bytes, ByteStreamError>>`.
    let mut body = resp.body;
    let mut total = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("reading S3 object body")?;
        writer.write_all(&chunk).await?;
        total += chunk.len() as u64;
    }
    writer.sync_all().await?;
    drop(writer);

    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("rename {} -> {}", partial.display(), dest.display()))?;
    Ok(total)
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
}
