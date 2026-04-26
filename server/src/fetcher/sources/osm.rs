//! OSM PBF download from Geofabrik / planet.osm.org.
//!
//! Wires the region preset table (`fetcher::region`) into the shared
//! HTTP core (`fetcher::http`). Per-PBF artifacts on disk:
//!
//!   data/pbf/<region>-latest.osm.pbf            the PBF
//!   data/pbf/<region>-latest.osm.pbf.etag       conditional-GET sidecar
//!   data/pbf/<region>-latest.osm.pbf.partial    in-flight (atomic-renamed)
//!   data/pbf/<region>-latest.osm.pbf.state.txt  Osmosis replication state
//!
//! The `.state.txt` fetch is best-effort — a 404 from upstream
//! (sub-region URLs sometimes lack one) yields a warning and proceeds.
//! The PBF and `.etag` are load-bearing; everything else is sidecar.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use tokio::sync::Semaphore;
use url::Url;

use crate::fetcher::http::{fetch, FetchOpts};
use crate::fetcher::mismatch::{check_pbf_directory_consistency, Mismatch};
use crate::fetcher::region::Region;
use crate::fetcher::state::ReplicationState;
use crate::fetcher::{FetchOutcome, FetchTarget};

/// Per-PBF outcome for the orchestrator's summary table.
#[derive(Debug)]
pub struct OsmFetchResult {
    pub url: Url,
    pub dest: PathBuf,
    pub outcome: Result<FetchOutcome>,
    pub state: Option<Result<ReplicationState>>,
}

/// Top-level entry point. Resolves region presets to URL set, runs
/// the planet/continent mismatch check, then fans out via a tokio
/// semaphore.
pub async fn fetch_osm(
    client: &Client,
    data_dir: &Path,
    region: Region,
    parallel: usize,
    opts: &FetchOpts,
) -> Result<Vec<OsmFetchResult>> {
    let pbf_dir = data_dir.join("pbf");
    tokio::fs::create_dir_all(&pbf_dir)
        .await
        .with_context(|| format!("creating {}", pbf_dir.display()))?;

    if let Err(mismatch) = check_pbf_directory_consistency(&pbf_dir, region) {
        // Render the human-readable remediation; the binary will
        // surface it on the error path.
        return Err(anyhow::anyhow!("{mismatch}").context(MismatchSentinel));
    }

    let urls = region.urls();
    let semaphore = Arc::new(Semaphore::new(parallel.max(1)));
    let client = client.clone();

    let mut tasks = FuturesUnordered::new();
    for url in urls {
        let permit = semaphore.clone().acquire_owned().await?;
        let client = client.clone();
        let pbf_dir = pbf_dir.clone();
        let opts = opts.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permit; // released on drop
            let target = target_for(&pbf_dir, &url);
            let outcome = fetch(&client, &target, &opts).await;
            // Best-effort state.txt fetch; never fail the OSM fetch on it.
            let state = match Region::state_url(&url) {
                Some(state_url) => Some(fetch_state(&client, &state_url, &target.dest).await),
                None => None,
            };
            OsmFetchResult {
                url: target.url,
                dest: target.dest,
                outcome,
                state,
            }
        }));
    }

    let mut results = Vec::new();
    while let Some(joined) = tasks.next().await {
        results.push(joined.context("task panicked")?);
    }

    Ok(results)
}

/// Marker error type so callers can tell mismatch detection apart
/// from network errors and exit with a different status / message.
#[derive(Debug)]
pub struct MismatchSentinel;
impl std::fmt::Display for MismatchSentinel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PBF directory mismatch")
    }
}
impl std::error::Error for MismatchSentinel {}

fn target_for(pbf_dir: &Path, url: &Url) -> FetchTarget {
    // Save under the URL basename — `australia-oceania-latest.osm.pbf`,
    // `planet-latest.osm.pbf`, etc.
    let filename = url
        .path_segments()
        .and_then(|segs| segs.last())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.osm.pbf");
    let dest = pbf_dir.join(filename);
    let md5_url = Region::md5_url(url);
    FetchTarget {
        url: url.clone(),
        dest,
        md5_url,
    }
}

/// Download the `<region>-updates/state.txt` sidecar and persist it
/// next to the PBF. Returns the parsed state for the orchestrator's
/// summary table.
async fn fetch_state(
    client: &Client,
    url: &Url,
    pbf_dest: &Path,
) -> Result<ReplicationState> {
    let resp = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("state.txt {url} returned {}", resp.status());
    }
    let body = resp.text().await?;
    let state = ReplicationState::parse(&body)
        .with_context(|| format!("parsing state.txt from {url}"))?;
    let mut state_path = pbf_dest.as_os_str().to_owned();
    state_path.push(".state.txt");
    tokio::fs::write(PathBuf::from(state_path), state.to_string_wire())
        .await
        .context("persisting state.txt sidecar")?;
    Ok(state)
}

/// Returns true if the wrapped error chain contains a MismatchSentinel.
pub fn is_mismatch(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<MismatchSentinel>())
}

/// Render a Mismatch directly when we held a typed reference to it.
/// Used for tests; the runtime path renders via Display on the
/// underlying error.
pub fn format_mismatch(m: &Mismatch) -> String {
    format!("{m}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_filename_matches_url_basename() {
        let url =
            Url::parse("https://download.geofabrik.de/australia-oceania-latest.osm.pbf").unwrap();
        let target = target_for(Path::new("/data/pbf"), &url);
        assert_eq!(
            target.dest,
            PathBuf::from("/data/pbf/australia-oceania-latest.osm.pbf")
        );
        assert!(target.md5_url.is_some());
    }

    #[test]
    fn target_for_planet_has_no_md5_sidecar() {
        let url =
            Url::parse("https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf").unwrap();
        let target = target_for(Path::new("/data/pbf"), &url);
        assert_eq!(target.dest, PathBuf::from("/data/pbf/planet-latest.osm.pbf"));
        assert!(
            target.md5_url.is_none(),
            "planet.osm.org doesn't publish .md5 sidecars"
        );
    }

    #[test]
    fn target_filename_falls_back_when_url_has_no_basename() {
        // Malformed URL with trailing slash. Shouldn't happen via
        // Region::urls(), but we don't want to panic if a caller
        // constructs a weird URL by hand.
        let url = Url::parse("https://example.com/").unwrap();
        let target = target_for(Path::new("/data/pbf"), &url);
        assert_eq!(target.dest, PathBuf::from("/data/pbf/download.osm.pbf"));
    }

    /// I11 (QA review): not all OSM endpoints publish a `state.txt`
    /// sidecar (some sub-region URLs lack one). When upstream 404s
    /// the orchestrator must surface a warning and continue — the
    /// PBF download itself is the load-bearing artifact; the state
    /// sidecar is bonus metadata.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_state_404_returns_err_for_orchestrator_to_warn() {
        use axum::routing::get;
        use axum::Router;
        use std::net::SocketAddr;
        use std::time::Duration;
        use tokio::net::TcpListener;
        use tokio::sync::oneshot;

        let app = Router::new().route(
            "/state.txt",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "") }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let join = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let tmp = tempfile::tempdir().unwrap();
        let pbf_dest = tmp.path().join("placeholder-latest.osm.pbf");
        let url = Url::parse(&format!("http://{addr}/state.txt")).unwrap();

        let result = fetch_state(&reqwest::Client::new(), &url, &pbf_dest).await;
        assert!(
            result.is_err(),
            "404 must surface as Err for orchestrator to log"
        );
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("404"),
            "error message must include the upstream status, got: {err_msg}"
        );
        // The .state.txt sidecar must NOT have been written.
        let state_path = format!("{}.state.txt", pbf_dest.display());
        assert!(
            std::fs::metadata(&state_path).is_err(),
            "404 path should never persist a sidecar"
        );

        let _ = tx.send(());
        let _ = join.await;
    }
}
