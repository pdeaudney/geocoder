//! MaxMind GeoLite2-City: license-key URL + tar.gz extract.
//!
//! Fetches `https://download.maxmind.com/app/geoip_download?...` and
//! extracts `*/GeoLite2-City.mmdb` to `data/GeoLite2-City.mmdb`.
//! Skipped (with a structured warning) when `MAXMIND_LICENSE_KEY`
//! isn't set — MaxMind is an optional source for the /geocode/ip
//! endpoint.
//!
//! Tarball extract is pure-Rust (`flate2` + `tar`). Tarball is
//! ~100 MB, so single-threaded gunzip is fine; the bzip2-decoder
//! priority chain doesn't apply here.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use reqwest::Client;
use url::Url;

use crate::fetcher::http::{fetch, FetchOpts};
use crate::fetcher::{FetchOutcome, FetchTarget};

const URL_TEMPLATE: &str = "https://download.maxmind.com/app/geoip_download?edition_id=GeoLite2-City&license_key={KEY}&suffix=tar.gz";

pub async fn fetch_maxmind(
    client: &Client,
    data_dir: &Path,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    let key = match std::env::var("MAXMIND_LICENSE_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            return Ok(FetchOutcome::Skipped {
                reason: "MAXMIND_LICENSE_KEY unset (sign up at \
                         https://www.maxmind.com/en/geolite2/signup)"
                    .to_string(),
            })
        }
    };

    tokio::fs::create_dir_all(data_dir).await?;
    let final_dest = data_dir.join("GeoLite2-City.mmdb");
    let tarball_dest = data_dir.join(".maxmind-geolite2-city.tar.gz");

    let url = Url::parse(&URL_TEMPLATE.replace("{KEY}", &key))
        .context("constructing MaxMind URL")?;

    // .mmdb already in place from a previous run + not forced =>
    // skip the network round-trip entirely.
    if final_dest.exists() && !opts.force {
        return Ok(FetchOutcome::Cached);
    }

    let target = FetchTarget {
        url,
        dest: tarball_dest.clone(),
        // MaxMind doesn't publish .md5 sidecars on this endpoint.
        md5_url: None,
    };

    let outcome = fetch(client, &target, opts).await?;
    extract_geolite2_city_from_tarball(&tarball_dest, &final_dest).await?;
    let _ = tokio::fs::remove_file(&tarball_dest).await;
    Ok(outcome)
}

/// Tar.gz format: `GeoLite2-City_<DATE>/GeoLite2-City.mmdb` (and a
/// few license / readme files we don't care about). We scan the
/// archive for the first entry whose filename ends with
/// `GeoLite2-City.mmdb` and write it to the canonical location.
async fn extract_geolite2_city_from_tarball(tarball: &Path, dest: &Path) -> Result<()> {
    let tarball = tarball.to_path_buf();
    let dest = dest.to_path_buf();

    // tar + flate2 are sync APIs; off-load to a blocking task.
    tokio::task::spawn_blocking(move || -> Result<()> {
        let f = std::fs::File::open(&tarball)
            .with_context(|| format!("opening {}", tarball.display()))?;
        let gz = GzDecoder::new(f);
        let mut archive = tar::Archive::new(gz);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s == "GeoLite2-City.mmdb")
                .unwrap_or(false)
            {
                let mut bytes = Vec::with_capacity(64 * 1024 * 1024);
                entry.read_to_end(&mut bytes)?;
                let tmp = with_extension(&dest, "tmp");
                std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
                std::fs::rename(&tmp, &dest)
                    .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
                return Ok(());
            }
        }
        Err(anyhow!(
            "GeoLite2-City.mmdb not found inside {}",
            tarball.display()
        ))
    })
    .await
    .context("tarball-extract task panicked")?
}

fn with_extension(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}
