//! `fetch-data` — acquire every external data source the geocoder
//! build pipeline consumes (OSM PBF, WhosOnFirst admin SQLite,
//! OpenAddresses, MaxMind GeoLite2, G-NAF).
//!
//! Architecture overview lives in `query_server::fetcher`. This file
//! is the CLI surface and dispatch.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use reqwest::Client;
use tracing::{info, warn};

use query_server::fetcher::http::FetchOpts;
use query_server::fetcher::region::Region;
use query_server::fetcher::sources::{maxmind, openaddresses, osm, wof};
use query_server::fetcher::FetchOutcome;

#[derive(Debug, Parser)]
#[command(
    name = "fetch-data",
    version,
    about = "Acquire OSM PBF + WoF + OpenAddresses + MaxMind + G-NAF for the geocoder build pipeline.",
    long_about = "
Re-runs are cheap: conditional GET (If-None-Match / If-Modified-Since)
short-circuits to 304 Not Modified when local data is fresh, and partial
downloads from a killed run resume cleanly via HTTP Range + If-Range.

OpenAddresses uses S3 Requester-Pays — needs AWS creds (env, profile, or
EC2 IAM). MaxMind needs MAXMIND_LICENSE_KEY. G-NAF needs GNAF_ARCHIVE_URL
(license-accepted by the operator at data.gov.au)."
)]
struct Cli {
    /// Output root. Layout: <dir>/pbf/, <dir>/openaddresses/, etc.
    #[arg(long, env = "DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// OSM region preset. See the README for the full list.
    #[arg(long, value_name = "REGION")]
    region: Option<String>,

    /// Fetch WhosOnFirst admin SQLite.
    #[arg(long, default_value_t = false)]
    wof: bool,

    /// WoF scope: "planet" (default), "none", or space-separated alpha-2 codes.
    #[arg(long, env = "WOF_COUNTRIES", default_value = "planet")]
    wof_countries: String,

    /// Fetch OpenAddresses (requires AWS creds for s3://v2.openaddresses.io).
    #[arg(long, default_value_t = false)]
    openaddresses: bool,

    /// OA source filter — "all" (default) or space-/comma-separated alpha-2 codes.
    #[arg(long, env = "OA_SOURCES", default_value = "all")]
    oa_sources: String,

    /// Fetch MaxMind GeoLite2-City (requires MAXMIND_LICENSE_KEY).
    #[arg(long, default_value_t = false)]
    maxmind: bool,

    /// Fetch G-NAF (requires GNAF_ARCHIVE_URL).
    #[arg(long, default_value_t = false)]
    gnaf: bool,

    /// Concurrent download streams for parallel fetch (e.g. all-continents).
    #[arg(long, env = "FETCH_PARALLEL", default_value_t = 4)]
    parallel: usize,

    /// Re-download even if local data appears fresh.
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Skip MD5 verification against upstream sidecar (Geofabrik publishes one).
    #[arg(long, default_value_t = false)]
    no_verify_md5: bool,

    /// Disable resuming from <dest>.partial files; re-download from scratch.
    #[arg(long, default_value_t = false)]
    no_resume: bool,

    /// Quiet mode: no terminal progress bars (logs are unaffected).
    #[arg(long, default_value_t = false)]
    quiet: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let opts = FetchOpts {
        force: cli.force,
        verify_md5: !cli.no_verify_md5,
        resume: !cli.no_resume,
        show_progress: !cli.quiet && std::io::IsTerminal::is_terminal(&std::io::stderr()),
    };

    let client = build_client()?;
    let mut had_failure = false;

    if let Some(region_str) = &cli.region {
        let region: Region = region_str.parse()?;
        info!(?region, parallel = cli.parallel, "fetching OSM PBF");
        match osm::fetch_osm(&client, &cli.data_dir, region, cli.parallel, &opts).await {
            Ok(results) => {
                for r in results {
                    summarize_osm(&r);
                    if r.outcome.is_err() {
                        had_failure = true;
                    }
                }
            }
            Err(e) if osm::is_mismatch(&e) => {
                eprintln!("{e:#}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("OSM fetch failed: {e:#}");
                had_failure = true;
            }
        }
    }

    if cli.wof {
        info!(scope = %cli.wof_countries, "fetching WhosOnFirst admin SQLite");
        match wof::fetch_wof(&client, &cli.data_dir, &cli.wof_countries, &opts).await {
            Ok(reports) => {
                for r in reports {
                    summarize_simple("WoF", &r.dest, &r.outcome);
                    if r.outcome.is_err() {
                        had_failure = true;
                    }
                }
            }
            Err(e) => {
                eprintln!("WoF fetch failed: {e:#}");
                had_failure = true;
            }
        }
    }

    if cli.openaddresses {
        info!(sources = %cli.oa_sources, "fetching OpenAddresses (S3 Requester-Pays)");
        match openaddresses::fetch_openaddresses(
            &client,
            &cli.data_dir,
            &cli.oa_sources,
            cli.parallel,
            &opts,
        )
        .await
        {
            Ok(reports) => {
                for r in reports {
                    summarize_simple("OA", &r.dest, &r.outcome);
                    if r.outcome.is_err() {
                        had_failure = true;
                    }
                }
            }
            Err(e) => {
                eprintln!("OpenAddresses fetch failed: {e:#}");
                had_failure = true;
            }
        }
    }

    if cli.maxmind {
        info!("fetching MaxMind GeoLite2-City");
        match maxmind::fetch_maxmind(&client, &cli.data_dir, &opts).await {
            Ok(outcome) => summarize_simple(
                "MaxMind",
                &cli.data_dir.join("GeoLite2-City.mmdb"),
                &Ok(outcome),
            ),
            Err(e) => {
                eprintln!("MaxMind fetch failed: {e:#}");
                had_failure = true;
            }
        }
    }

    if cli.gnaf {
        info!("fetching G-NAF");
        match query_server::fetcher::sources::gnaf::fetch_gnaf(
            &client,
            &cli.data_dir,
            &opts,
        )
        .await
        {
            Ok(outcome) => summarize_simple(
                "G-NAF",
                &cli.data_dir.join("gnaf"),
                &Ok(outcome),
            ),
            Err(e) => {
                eprintln!("G-NAF fetch failed: {e:#}");
                had_failure = true;
            }
        }
    }

    if had_failure {
        std::process::exit(1);
    }
    Ok(())
}

fn build_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!(env!("CARGO_PKG_NAME"), "/fetch-data"))
        .https_only(false)
        .build()?)
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,fetch_data=info,query_server::fetcher=info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

fn summarize_osm(r: &osm::OsmFetchResult) {
    match &r.outcome {
        Ok(FetchOutcome::Cached) => info!(
            url = %r.url,
            dest = %r.dest.display(),
            "cached (304 Not Modified)"
        ),
        Ok(FetchOutcome::Downloaded { bytes }) => info!(
            url = %r.url,
            dest = %r.dest.display(),
            bytes,
            "downloaded"
        ),
        Ok(FetchOutcome::Resumed { bytes, total }) => info!(
            url = %r.url,
            dest = %r.dest.display(),
            bytes_appended = bytes,
            total,
            "resumed partial download"
        ),
        Ok(FetchOutcome::Skipped { reason }) => info!(url = %r.url, reason, "skipped"),
        Err(e) => warn!(url = %r.url, error = %e, "fetch failed"),
    }
    if let Some(state) = &r.state {
        match state {
            Ok(s) => info!(
                timestamp = %s.timestamp,
                sequence = s.sequence_number,
                "replication state"
            ),
            Err(e) => warn!(error = %e, "state.txt not available (PBF still usable)"),
        }
    }
}

fn summarize_simple(label: &str, dest: &std::path::Path, outcome: &Result<FetchOutcome>) {
    match outcome {
        Ok(FetchOutcome::Cached) => info!(label, dest = %dest.display(), "cached"),
        Ok(FetchOutcome::Downloaded { bytes }) => {
            info!(label, dest = %dest.display(), bytes, "downloaded")
        }
        Ok(FetchOutcome::Resumed { bytes, total }) => {
            info!(label, dest = %dest.display(), bytes_appended = bytes, total, "resumed")
        }
        Ok(FetchOutcome::Skipped { reason }) => {
            info!(label, dest = %dest.display(), reason, "skipped")
        }
        Err(e) => warn!(label, dest = %dest.display(), error = %e, "fetch failed"),
    }
}
