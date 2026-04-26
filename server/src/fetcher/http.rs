//! HTTP fetch core: conditional GET + resumable downloads + MD5 verify.
//!
//! Single state machine. Every per-file fetch (Geofabrik PBF, WoF
//! .bz2, MaxMind tarball, S3 GetObject for OA) routes through here so
//! the correctness invariants are pinned in one place:
//!
//!   - Atomic on-disk rename: `<dest>.partial` → `<dest>`. A killed
//!     process never leaves a half-written file at the final path.
//!   - ETag persisted as `<dest>.etag` sidecar so the next run can
//!     send `If-None-Match` and short-circuit on 304.
//!   - `--remote-time` equivalent: file mtime is set from
//!     `Last-Modified` so `If-Modified-Since` checks stay honest
//!     across runs.
//!   - Resumable: when `<dest>.partial` survived from a prior run we
//!     send `Range: bytes=N-` + `If-Range: <saved-etag>`; the server
//!     returns 206 to continue or 200 to restart cleanly.
//!   - MD5 verification streaming: hash is computed concurrently
//!     with disk write, no second-pass read.
//!
//! Failure surfaces:
//!
//!   - 304: Cached. No-op, return.
//!   - 200 (fresh or 200-after-If-Range): full download.
//!   - 206: resume partial.
//!   - 416 (range not satisfiable): partial corrupt; delete and retry.
//!   - 412 (precondition failed, S3 only): treat as if 200 — restart.
//!   - Anything else: typed error with response status, body trimmed
//!     to the first 1 KiB for diagnostics.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use md5::{Digest, Md5};
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE,
    LAST_MODIFIED, RANGE,
};
use reqwest::{Client, StatusCode};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{FetchOutcome, FetchTarget};

#[derive(Debug, Clone)]
pub struct FetchOpts {
    /// Bypass conditional GET; always re-download.
    pub force: bool,
    /// Verify MD5 against the upstream sidecar (when one exists).
    pub verify_md5: bool,
    /// Resume from `<dest>.partial` if present.
    pub resume: bool,
    /// Show terminal progress bar (suppressed in non-tty / `--quiet`).
    pub show_progress: bool,
}

impl Default for FetchOpts {
    fn default() -> Self {
        Self {
            force: false,
            verify_md5: true,
            resume: true,
            show_progress: true,
        }
    }
}

pub fn etag_sidecar(dest: &Path) -> PathBuf {
    sidecar(dest, "etag")
}

pub fn partial_sidecar(dest: &Path) -> PathBuf {
    sidecar(dest, "partial")
}

fn sidecar(dest: &Path, suffix: &str) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

/// Run the full fetch state machine for one target.
pub async fn fetch(
    client: &Client,
    target: &FetchTarget,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    if let Some(parent) = target.dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating parent of {}", target.dest.display()))?;
    }

    let etag_path = etag_sidecar(&target.dest);
    let partial_path = partial_sidecar(&target.dest);

    if opts.force {
        let _ = tokio::fs::remove_file(&etag_path).await;
        let _ = tokio::fs::remove_file(&partial_path).await;
    }

    // Resume path: a leftover partial means a prior run was killed.
    if opts.resume && tokio::fs::metadata(&partial_path).await.is_ok() {
        return resume_partial(client, target, &partial_path, &etag_path, opts).await;
    }

    // Conditional GET: send If-None-Match / If-Modified-Since when
    // we have prior signals; the server short-circuits with 304 when
    // the local copy is fresh.
    let saved_etag = read_etag(&etag_path).await;
    let local_mtime = tokio::fs::metadata(&target.dest)
        .await
        .ok()
        .and_then(|m| m.modified().ok());

    let mut req = client.get(target.url.clone());
    if let Some(etag) = &saved_etag {
        req = req.header(IF_NONE_MATCH, etag.as_str());
    }
    if let Some(t) = local_mtime {
        req = req.header(IF_MODIFIED_SINCE, fmt_http_date(t));
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("GET {}", target.url))?;

    match resp.status() {
        StatusCode::NOT_MODIFIED => Ok(FetchOutcome::Cached),
        StatusCode::OK => {
            stream_full_to_partial(resp, target, &partial_path, &etag_path, opts).await
        }
        // S3 surfaces a precondition mismatch as 412; treat as
        // "content changed, redo from scratch" — we'll lose any
        // saved etag and refetch on the next call.
        StatusCode::PRECONDITION_FAILED => {
            let _ = tokio::fs::remove_file(&etag_path).await;
            anyhow::bail!(
                "precondition failed for {}; clearing cached etag — re-run to refetch",
                target.url
            )
        }
        s => bail_with_body("unexpected status", &target.url, resp, s).await,
    }
}

/// Resume a download whose previous attempt left a `<dest>.partial`
/// behind. Uses `Range` + `If-Range` so the server tells us whether
/// the partial is still valid.
async fn resume_partial(
    client: &Client,
    target: &FetchTarget,
    partial_path: &Path,
    etag_path: &Path,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    let saved_etag = read_etag(etag_path).await;
    let partial_size = tokio::fs::metadata(partial_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let mut req = client
        .get(target.url.clone())
        .header(RANGE, format!("bytes={partial_size}-"));
    if let Some(etag) = &saved_etag {
        req = req.header(IF_RANGE, etag.as_str());
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("range GET {}", target.url))?;

    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {
            // Tee bytes to .partial (append) and the running hash.
            // Seed the hash with bytes already on disk so the final
            // digest covers the whole file — that's what the upstream
            // .md5 is computed against.
            let mut hasher = Md5::new();
            seed_hasher_with_partial(partial_path, &mut hasher).await?;

            let total_len = resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|remaining| partial_size + remaining);
            let progress = make_progress(total_len, opts);
            progress.set_position(partial_size);

            let new_etag = pick_etag(&resp, &saved_etag);
            let last_modified = parse_last_modified(&resp);

            let mut writer = OpenOptions::new()
                .append(true)
                .create(false)
                .open(partial_path)
                .await
                .with_context(|| format!("appending to {}", partial_path.display()))?;
            let mut total_appended = 0u64;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.with_context(|| format!("streaming {}", target.url))?;
                hasher.update(&chunk);
                writer.write_all(&chunk).await?;
                total_appended += chunk.len() as u64;
                progress.set_position(partial_size + total_appended);
            }
            writer.sync_all().await?;
            drop(writer);
            progress.finish_and_clear();

            let local_md5 = format!("{:x}", hasher.finalize());
            verify_md5_if_requested(client, target, &local_md5, partial_path, opts).await?;

            tokio::fs::rename(partial_path, &target.dest)
                .await
                .with_context(|| {
                    format!(
                        "rename {} -> {}",
                        partial_path.display(),
                        target.dest.display()
                    )
                })?;
            apply_mtime_if_set(&target.dest, last_modified).await;
            persist_etag(etag_path, new_etag.as_deref()).await;

            Ok(FetchOutcome::Resumed {
                bytes: total_appended,
                total: partial_size + total_appended,
            })
        }
        // Server says content has changed since we cached the etag;
        // throw the partial away and stream the full body it just
        // sent us into a fresh .partial.
        StatusCode::OK => {
            let _ = tokio::fs::remove_file(partial_path).await;
            stream_full_to_partial(resp, target, partial_path, etag_path, opts).await
        }
        // Range out of bounds — partial probably corrupt or zero-size.
        // Wipe and retry from scratch; recursing once is safe because
        // the next call has no partial to find.
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let _ = tokio::fs::remove_file(partial_path).await;
            Box::pin(fetch(client, target, opts)).await
        }
        s => bail_with_body("range fetch unexpected status", &target.url, resp, s).await,
    }
}

/// Stream a 200 OK response body into `<dest>.partial` while
/// simultaneously hashing for MD5 verification. Atomic-rename to
/// final on success.
async fn stream_full_to_partial(
    resp: reqwest::Response,
    target: &FetchTarget,
    partial_path: &Path,
    etag_path: &Path,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    let total_len = resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let progress = make_progress(total_len, opts);

    let new_etag = resp
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let last_modified = parse_last_modified(&resp);

    // Truncate-or-create the .partial — we're starting clean (a
    // resume path would have routed through resume_partial).
    let mut writer = File::create(partial_path)
        .await
        .with_context(|| format!("creating {}", partial_path.display()))?;
    let mut hasher = Md5::new();
    let mut total = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("streaming {}", target.url))?;
        hasher.update(&chunk);
        writer.write_all(&chunk).await?;
        total += chunk.len() as u64;
        progress.set_position(total);
    }
    writer.sync_all().await?;
    drop(writer);
    progress.finish_and_clear();

    let local_md5 = format!("{:x}", hasher.finalize());
    verify_md5_if_requested(crate_client(), target, &local_md5, partial_path, opts).await?;

    tokio::fs::rename(partial_path, &target.dest)
        .await
        .with_context(|| {
            format!(
                "rename {} -> {}",
                partial_path.display(),
                target.dest.display()
            )
        })?;
    apply_mtime_if_set(&target.dest, last_modified).await;
    persist_etag(etag_path, new_etag.as_deref()).await;

    Ok(FetchOutcome::Downloaded { bytes: total })
}

/// Verify MD5 against the upstream sidecar when one is configured
/// and verification isn't disabled. Removes `<dest>.partial` on
/// mismatch so callers don't bake in corruption.
async fn verify_md5_if_requested(
    client: &Client,
    target: &FetchTarget,
    local_md5: &str,
    partial_path: &Path,
    opts: &FetchOpts,
) -> Result<()> {
    if !opts.verify_md5 {
        return Ok(());
    }
    let Some(md5_url) = target.md5_url.as_ref() else {
        return Ok(());
    };
    let body = client
        .get(md5_url.clone())
        .send()
        .await
        .with_context(|| format!("GET {md5_url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {md5_url}"))?
        .text()
        .await?;
    // Format is `<md5>  <filename>` per BSD md5sum convention.
    let server_md5 = body
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("empty md5 sidecar at {md5_url}"))?
        .to_lowercase();
    if server_md5 != local_md5 {
        let _ = tokio::fs::remove_file(partial_path).await;
        return Err(anyhow!(
            "md5 mismatch on {}: server={server_md5} local={local_md5} (partial removed)",
            target.dest.display()
        ));
    }
    Ok(())
}

async fn read_etag(etag_path: &Path) -> Option<String> {
    tokio::fs::read_to_string(etag_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn persist_etag(etag_path: &Path, etag: Option<&str>) {
    let Some(etag) = etag else {
        return;
    };
    if let Err(e) = tokio::fs::write(etag_path, etag).await {
        tracing::warn!(?etag_path, error = %e, "failed to persist etag sidecar");
    }
}

async fn apply_mtime_if_set(dest: &Path, last_modified: Option<SystemTime>) {
    let Some(t) = last_modified else { return };
    let dest = dest.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        let f = std::fs::File::options().write(true).open(&dest);
        if let Ok(f) = f {
            let _ = f.set_modified(t);
        }
    })
    .await;
}

/// Read whatever bytes are already on disk in `<dest>.partial` into
/// the running hasher so the final digest covers the entire file
/// once the resumed bytes are appended. No streaming smarts — the
/// partial is finite and we'd otherwise have to either re-download
/// from byte 0 or skip MD5 verification entirely.
async fn seed_hasher_with_partial(partial_path: &Path, hasher: &mut Md5) -> Result<()> {
    let mut f = File::open(partial_path).await?;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(())
}

fn pick_etag(resp: &reqwest::Response, fallback: &Option<String>) -> Option<String> {
    if let Some(v) = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()) {
        return Some(v.to_string());
    }
    fallback.clone()
}

fn parse_last_modified(resp: &reqwest::Response) -> Option<SystemTime> {
    let hv = resp.headers().get(LAST_MODIFIED)?;
    let s = hv.to_str().ok()?;
    httpdate::parse_http_date(s).ok()
}

fn fmt_http_date(t: SystemTime) -> String {
    httpdate::fmt_http_date(t)
}

fn make_progress(total: Option<u64>, opts: &FetchOpts) -> ProgressBar {
    if !opts.show_progress {
        return ProgressBar::hidden();
    }
    let pb = match total {
        Some(t) => ProgressBar::new(t),
        None => ProgressBar::new_spinner(),
    };
    let _ = pb.set_style(
        ProgressStyle::with_template(
            "  {bar:40.cyan/blue} {bytes:>10}/{total_bytes:>10} {bytes_per_sec:>12} eta {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
    pb
}

async fn bail_with_body(
    label: &str,
    url: &url::Url,
    resp: reqwest::Response,
    status: StatusCode,
) -> Result<FetchOutcome> {
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
    let trimmed: String = body.chars().take(1024).collect();
    Err(anyhow!(
        "{label}: GET {url} returned {status}\nbody (first 1 KiB): {trimmed}"
    ))
}

/// Owned reqwest client used by `stream_full_to_partial` for the
/// MD5 sidecar fetch on its non-resume code path. Keeps the call site
/// signature simple at the cost of one extra Client instance per
/// download (cheap; reqwest's Client pools internally per-thread).
fn crate_client() -> &'static Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent(concat!(env!("CARGO_PKG_NAME"), "/fetch-data"))
            .https_only(false)
            .build()
            .expect("static reqwest client builds")
    })
}

/// Heuristic: does this URL's response advertise byte-range support?
/// Used by the OSM source module to skip the partial-resume branch
/// when the server doesn't support it (we'd 200 OK on every Range
/// request and waste bandwidth on every resume).
pub async fn supports_range(client: &Client, url: &url::Url) -> bool {
    match client.head(url.clone()).send().await {
        Ok(resp) => resp
            .headers()
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false),
        Err(_) => false,
    }
}
