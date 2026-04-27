//! IP-to-city MMDB acquisition for `/geocode/ip`.
//!
//! Two datasets, one canonical on-disk filename:
//!
//!   1. **MaxMind GeoLite2-City** (preferred). Fetches
//!      `https://download.maxmind.com/app/geoip_download?...` when
//!      `MAXMIND_LICENSE_KEY` is set; extracts the .mmdb out of the
//!      tar.gz to `data/GeoLite2-City.mmdb`.
//!
//!   2. **DB-IP IP-to-City Lite** (fallback). When the license key
//!      isn't set, fetches the current month's free lite database
//!      from `https://download.db-ip.com/free/dbip-city-lite-YYYY-MM.mmdb.gz`
//!      (gunzipping directly into the same canonical filename).
//!      Free, no signup, CC-BY 4.0 licensed, MMDB-format-compatible
//!      with the maxminddb reader. Schema is a near-subset of
//!      GeoLite2 — `country`, `city`, `location.latitude`,
//!      `location.longitude` all present.
//!
//! Either path writes a `<dest>.mmdb.source` sidecar recording
//! which dataset is on disk (audit trail; lets you see at a glance
//! whether the runtime is serving MaxMind or DB-IP data).
//!
//! Override flags:
//!   - `MAXMIND_FALLBACK_TO_DBIP=false|0|no|off` to disable the
//!     fallback (strict MaxMind-only mode for compliance reasons).
//!     Without the license key this skips with a structured warning.
//!
//! Tar.gz / .gz extraction is pure-Rust (`flate2` + `tar`). The
//! MaxMind tarball is ~100 MB; the DB-IP gzip is ~62 MB. Both are
//! single-stream so the bzip2-decoder priority chain doesn't apply.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, Utc};
use flate2::read::GzDecoder;
use reqwest::Client;
use url::Url;

use crate::fetcher::http::{fetch, FetchOpts};
use crate::fetcher::{FetchOutcome, FetchTarget};

const MAXMIND_URL_TEMPLATE: &str = "https://download.maxmind.com/app/geoip_download?edition_id=GeoLite2-City&license_key={KEY}&suffix=tar.gz";
const DBIP_URL_TEMPLATE: &str = "https://download.db-ip.com/free/dbip-city-lite-{YYYY}-{MM}.mmdb.gz";

const MAX_MMDB_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

pub async fn fetch_maxmind(
    client: &Client,
    data_dir: &Path,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    tokio::fs::create_dir_all(data_dir).await?;
    let final_dest = data_dir.join("GeoLite2-City.mmdb");
    let source_sidecar = with_suffix(&final_dest, "source");

    // 1. MaxMind primary path when the license key is configured.
    if let Some(key) = read_env_nonempty("MAXMIND_LICENSE_KEY") {
        return fetch_maxmind_primary(
            client,
            data_dir,
            &final_dest,
            &source_sidecar,
            &key,
            opts,
        )
        .await;
    }

    // 2. Fallback path: DB-IP free city-lite, unless explicitly disabled.
    if fallback_disabled() {
        return Ok(FetchOutcome::Skipped {
            reason: "MAXMIND_LICENSE_KEY unset and MAXMIND_FALLBACK_TO_DBIP \
                     disabled (sign up at https://www.maxmind.com/en/geolite2/signup \
                     or set MAXMIND_FALLBACK_TO_DBIP=true to use the DB-IP free \
                     city-lite dataset)"
                .to_string(),
        });
    }

    tracing::warn!(
        "MAXMIND_LICENSE_KEY unset; falling back to DB-IP IP-to-City Lite \
         (free, no signup, CC-BY 4.0). Schema is a near-subset of GeoLite2 \
         but verify your /geocode/ip output matches expectations. \
         Set MAXMIND_FALLBACK_TO_DBIP=false to disable this fallback."
    );

    fetch_dbip_fallback(client, data_dir, &final_dest, &source_sidecar, opts).await
}

async fn fetch_maxmind_primary(
    client: &Client,
    data_dir: &Path,
    final_dest: &Path,
    source_sidecar: &Path,
    key: &str,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    let tarball_dest = data_dir.join(".maxmind-geolite2-city.tar.gz");
    let url = Url::parse(&MAXMIND_URL_TEMPLATE.replace("{KEY}", key))
        .context("constructing MaxMind URL")?;

    // .mmdb already in place from a previous run + not forced =>
    // skip the network round-trip entirely. Source sidecar is
    // refreshed unconditionally so an operator who flipped from
    // DB-IP to MaxMind sees the truth without a forced re-download.
    if final_dest.exists() && !opts.force {
        write_source_sidecar(source_sidecar, "maxmind-geolite2-city", "license-key").await;
        return Ok(FetchOutcome::Cached);
    }

    let target = FetchTarget {
        url,
        dest: tarball_dest.clone(),
        md5_url: None, // MaxMind doesn't publish .md5 on this endpoint.
    };

    let outcome = fetch(client, &target, opts).await?;
    extract_geolite2_city_from_tarball(&tarball_dest, final_dest).await?;
    let _ = tokio::fs::remove_file(&tarball_dest).await;
    write_source_sidecar(source_sidecar, "maxmind-geolite2-city", "license-key").await;
    Ok(outcome)
}

async fn fetch_dbip_fallback(
    client: &Client,
    data_dir: &Path,
    final_dest: &Path,
    source_sidecar: &Path,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    // DB-IP publishes monthly snapshots. New month files appear in
    // the first few days, so try current month first then fall back
    // to the previous month if not yet published.
    let now = Utc::now();
    let (cur_y, cur_m) = (now.year() as u32, now.month());
    let (prev_y, prev_m) = if cur_m == 1 {
        (cur_y - 1, 12)
    } else {
        (cur_y, cur_m - 1)
    };

    let candidates = [
        (cur_y, cur_m),
        (prev_y, prev_m),
    ];

    let gz_dest = data_dir.join(".dbip-city-lite.mmdb.gz");
    let mut last_err: Option<anyhow::Error> = None;

    for (year, month) in candidates {
        let url_str = DBIP_URL_TEMPLATE
            .replace("{YYYY}", &format!("{year:04}"))
            .replace("{MM}", &format!("{month:02}"));
        let url = match Url::parse(&url_str) {
            Ok(u) => u,
            Err(e) => {
                last_err = Some(e.into());
                continue;
            }
        };

        // Check whether the canonical .mmdb is already on disk and
        // tagged with this same month — short-circuit re-download.
        // The sidecar is the canonical source-of-truth for "what's
        // currently installed".
        if !opts.force && final_dest.exists() {
            if let Ok(existing) = tokio::fs::read_to_string(source_sidecar).await {
                let needle = format!("dbip-city-lite {year:04}-{month:02}");
                if existing.contains(&needle) {
                    return Ok(FetchOutcome::Cached);
                }
            }
        }

        let target = FetchTarget {
            url: url.clone(),
            dest: gz_dest.clone(),
            md5_url: None, // DB-IP doesn't publish per-file md5.
        };

        match fetch(client, &target, opts).await {
            Ok(outcome) => {
                tracing::info!(
                    url = %url,
                    "DB-IP free city-lite snapshot {year:04}-{month:02} fetched; \
                     decompressing to {}",
                    final_dest.display(),
                );
                gunzip_to_file(&gz_dest, final_dest).await?;
                let _ = tokio::fs::remove_file(&gz_dest).await;
                write_source_sidecar(
                    source_sidecar,
                    &format!("dbip-city-lite {year:04}-{month:02}"),
                    "fallback (no MAXMIND_LICENSE_KEY)",
                )
                .await;
                return Ok(outcome);
            }
            Err(e) => {
                tracing::debug!(
                    url = %url,
                    error = %e,
                    "DB-IP {year:04}-{month:02} not available; trying previous month",
                );
                last_err = Some(e);
                continue;
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("DB-IP fallback exhausted candidates")))
}

/// Tar.gz format: `GeoLite2-City_<DATE>/GeoLite2-City.mmdb` (and a
/// few license / readme files we don't care about). We scan the
/// archive for the first entry whose filename ends with
/// `GeoLite2-City.mmdb` and stream it to the canonical location.
///
/// Hard cap on the extracted size: 1 GiB. The real .mmdb is ~70 MB;
/// the cap is generous headroom for a few years of growth but keeps
/// us safe against a decompression-bomb tarball.
async fn extract_geolite2_city_from_tarball(tarball: &Path, dest: &Path) -> Result<()> {
    let tarball = tarball.to_path_buf();
    let dest = dest.to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let f = std::fs::File::open(&tarball)
            .with_context(|| format!("opening {}", tarball.display()))?;
        let gz = GzDecoder::new(f);
        let mut archive = tar::Archive::new(gz);
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s == "GeoLite2-City.mmdb")
                .unwrap_or(false)
            {
                if found {
                    tracing::warn!(
                        "tarball contains multiple GeoLite2-City.mmdb entries; using the first"
                    );
                    break;
                }
                found = true;
                let tmp = with_suffix(&dest, "tmp");
                let mut writer = std::fs::File::create(&tmp)
                    .with_context(|| format!("creating {}", tmp.display()))?;
                let mut limited = (&mut entry).take(MAX_MMDB_BYTES + 1);
                let written = std::io::copy(&mut limited, &mut writer)
                    .with_context(|| format!("streaming entry to {}", tmp.display()))?;
                if written > MAX_MMDB_BYTES {
                    drop(writer);
                    let _ = std::fs::remove_file(&tmp);
                    return Err(anyhow!(
                        "GeoLite2-City.mmdb exceeds {} byte cap (decompression bomb?)",
                        MAX_MMDB_BYTES
                    ));
                }
                writer.sync_all()?;
                drop(writer);
                std::fs::rename(&tmp, &dest)
                    .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
            }
        }
        if !found {
            return Err(anyhow!(
                "GeoLite2-City.mmdb not found inside {}",
                tarball.display()
            ));
        }
        Ok(())
    })
    .await
    .context("tarball-extract task panicked")?
}

/// Gunzip a single-stream `.mmdb.gz` to `dest` with the same 1 GiB
/// decompression-bomb safety cap as the tarball path. Atomic via
/// rename from `<dest>.tmp`.
async fn gunzip_to_file(input_gz: &Path, dest: &Path) -> Result<()> {
    let input_gz = input_gz.to_path_buf();
    let dest = dest.to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let f = std::fs::File::open(&input_gz)
            .with_context(|| format!("opening {}", input_gz.display()))?;
        let mut gz = GzDecoder::new(f).take(MAX_MMDB_BYTES + 1);
        let tmp = with_suffix(&dest, "tmp");
        let mut writer = std::fs::File::create(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let written = std::io::copy(&mut gz, &mut writer)
            .with_context(|| format!("gunzip {} -> {}", input_gz.display(), tmp.display()))?;
        if written > MAX_MMDB_BYTES {
            drop(writer);
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow!(
                "decompressed mmdb exceeds {} byte cap (decompression bomb?)",
                MAX_MMDB_BYTES
            ));
        }
        writer.sync_all()?;
        drop(writer);
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
        Ok(())
    })
    .await
    .context("gunzip task panicked")?
}

/// Persist a small text sidecar next to the .mmdb describing which
/// dataset is currently installed. Format:
///
///   ```text
///   source = dbip-city-lite 2026-04
///   acquired_via = fallback (no MAXMIND_LICENSE_KEY)
///   acquired_at = 2026-04-27T00:11:33Z
///   ```
///
/// Best-effort — a write failure here doesn't fail the fetch.
async fn write_source_sidecar(path: &Path, source: &str, acquired_via: &str) {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let body = format!(
        "source = {source}\nacquired_via = {acquired_via}\nacquired_at = {now}\n"
    );
    if let Err(e) = tokio::fs::write(path, body).await {
        tracing::warn!(
            sidecar = %path.display(),
            error = %e,
            "failed to write mmdb source sidecar (non-fatal)"
        );
    }
}

fn read_env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn fallback_disabled() -> bool {
    std::env::var("MAXMIND_FALLBACK_TO_DBIP")
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false)
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}
