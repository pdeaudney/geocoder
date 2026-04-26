//! WhosOnFirst admin SQLite (download + bz2 decompress).
//!
//! Fetches `whosonfirst-data-admin-<scope>-latest.db.bz2` from
//! data.geocode.earth, then decompresses to
//! `data/whosonfirst-data-admin-<scope>-latest.db`. Decompression
//! shells out to the fastest available bzip2 decoder
//! (`lbzip2 → pbzip2 → bzip2`). Pure-Rust bzip2 decoders are
//! single-threaded, and the WoF planet snapshot is 8.6 GB
//! (compressed) → ~30 GB (decompressed), so the parallel decoder
//! matters.
//!
//! Scope semantics (`--wof-countries` / `WOF_COUNTRIES`):
//!   - "planet" — admin-latest (8.6 GB)
//!   - "none"   — skipped at orchestration layer
//!   - "<cc> <cc>..." — per-country files at the same URL prefix

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use tokio::process::Command;
use url::Url;

use crate::fetcher::http::{fetch, FetchOpts};
use crate::fetcher::{FetchOutcome, FetchTarget};

const WOF_BASE: &str = "https://data.geocode.earth/wof/dist/sqlite";

#[derive(Debug)]
pub struct WofResult {
    pub dest: PathBuf,
    pub outcome: Result<FetchOutcome>,
}

pub async fn fetch_wof(
    client: &Client,
    data_dir: &Path,
    scope: &str,
    opts: &FetchOpts,
) -> Result<Vec<WofResult>> {
    tokio::fs::create_dir_all(data_dir)
        .await
        .with_context(|| format!("creating {}", data_dir.display()))?;

    let scopes = parse_scope(scope);
    if scopes.is_empty() {
        return Ok(vec![]);
    }

    let mut out = Vec::with_capacity(scopes.len());
    for cc in scopes {
        out.push(fetch_one(client, data_dir, &cc, opts).await);
    }
    Ok(out)
}

fn parse_scope(scope: &str) -> Vec<String> {
    match scope.trim() {
        "" | "none" => vec![],
        "planet" => vec!["admin".to_string()],
        // Country list: "au nz" → ["admin-au", "admin-nz"]. Split on
        // whitespace and commas so either form works.
        list => list
            .split([' ', ','])
            .filter(|s| !s.is_empty())
            .map(|cc| format!("admin-{}", cc.to_lowercase()))
            .collect(),
    }
}

async fn fetch_one(
    client: &Client,
    data_dir: &Path,
    suffix: &str,
    opts: &FetchOpts,
) -> WofResult {
    // suffix examples: "admin" (planet), "admin-au" (per-country)
    let filename_compressed = format!("whosonfirst-data-{suffix}-latest.db.bz2");
    let filename_db = format!("whosonfirst-data-{suffix}-latest.db");
    let url = match Url::parse(&format!("{WOF_BASE}/{filename_compressed}")) {
        Ok(u) => u,
        Err(e) => {
            return WofResult {
                dest: data_dir.join(&filename_db),
                outcome: Err(e.into()),
            }
        }
    };
    let dest_db = data_dir.join(&filename_db);
    let dest_bz2 = data_dir.join(&filename_compressed);

    // Short-circuit: decompressed DB already exists from a previous
    // run. Don't re-fetch the .bz2 just to throw it away. (lbzip2 /
    // pbzip2 / bzip2 -d consume the .bz2 on success — default
    // behaviour, no -k — so the .bz2 won't be on disk to reuse.)
    //
    // I10: clean up any orphan `<bz2>.partial` left by a prior run
    // that completed the decompress but was killed before its own
    // re-run could rotate them. The partial files are useless once
    // the .db is in place; leaving them on disk leaks GBs.
    if dest_db.exists() && !opts.force {
        let bz2_partial = path_with_suffix(&dest_bz2, "partial");
        let _ = tokio::fs::remove_file(&bz2_partial).await;
        let etag_orphan = path_with_suffix(&dest_bz2, "etag");
        let _ = tokio::fs::remove_file(&etag_orphan).await;
        return WofResult {
            dest: dest_db,
            outcome: Ok(FetchOutcome::Cached),
        };
    }

    let target = FetchTarget {
        url,
        dest: dest_bz2.clone(),
        md5_url: None,
    };

    let outcome = match fetch(client, &target, opts).await {
        Ok(FetchOutcome::Cached) if dest_bz2.exists() => {
            // .bz2 hadn't been decompressed (or was deleted after
            // decompression). Decompress now.
            decompress_in_place(&dest_bz2, &dest_db).await
        }
        Ok(other_outcome) => match decompress_in_place(&dest_bz2, &dest_db).await {
            Ok(_) => Ok(other_outcome),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };

    WofResult {
        dest: dest_db,
        outcome,
    }
}

/// Run `<bzip2-decoder> -d <input.bz2>` so the .bz2 is consumed and
/// the output lands at the same path with the .bz2 suffix stripped.
/// Then atomically rename the result to `dest_db` (the canonical
/// path the build pipeline reads).
async fn decompress_in_place(input_bz2: &Path, dest_db: &Path) -> Result<FetchOutcome> {
    let decoder = pick_bzip2_decoder().await?;
    tracing::info!(
        decoder,
        input = %input_bz2.display(),
        "decompressing WoF .bz2"
    );
    let status = Command::new(&decoder)
        .arg("-d")
        .arg(input_bz2)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("running {decoder} -d {}", input_bz2.display()))?;
    if !status.success() {
        return Err(anyhow!("{decoder} -d failed with exit {status}"));
    }
    // The decoder strips `.bz2` and writes the result alongside.
    let decompressed = strip_bz2_suffix(input_bz2)
        .ok_or_else(|| anyhow!("input is not a .bz2 file: {}", input_bz2.display()))?;
    tokio::fs::rename(&decompressed, dest_db).await.with_context(|| {
        format!(
            "rename {} -> {}",
            decompressed.display(),
            dest_db.display()
        )
    })?;
    let bytes = tokio::fs::metadata(dest_db).await.map(|m| m.len()).unwrap_or(0);
    Ok(FetchOutcome::Downloaded { bytes })
}

fn strip_bz2_suffix(p: &Path) -> Option<PathBuf> {
    let s = p.to_string_lossy();
    s.strip_suffix(".bz2").map(PathBuf::from)
}

fn path_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

async fn pick_bzip2_decoder() -> Result<&'static str> {
    // lbzip2 wins because it parallelises across cores on any
    // single-stream .bz2 input (which the WoF distribution is);
    // pbzip2 only parallelises on pbzip2-encoded multi-stream files
    // but still wins ~10–20 % over plain bzip2 via I/O overlap.
    for candidate in ["lbzip2", "pbzip2", "bzip2"] {
        let cmd = candidate.to_string();
        let found = tokio::task::spawn_blocking(move || which::which(&cmd).is_ok())
            .await
            .unwrap_or(false);
        if found {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "no bzip2 decoder found on PATH (tried lbzip2, pbzip2, bzip2)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_planet_yields_admin_only() {
        assert_eq!(parse_scope("planet"), vec!["admin"]);
    }

    #[test]
    fn scope_none_or_empty_yields_nothing() {
        assert!(parse_scope("none").is_empty());
        assert!(parse_scope("").is_empty());
        assert!(parse_scope("   ").is_empty());
    }

    #[test]
    fn scope_country_list_lowercases_and_prefixes() {
        assert_eq!(parse_scope("AU NZ"), vec!["admin-au", "admin-nz"]);
        assert_eq!(parse_scope("au,nz, fj"), vec!["admin-au", "admin-nz", "admin-fj"]);
    }

    #[test]
    fn strip_bz2_basic() {
        assert_eq!(
            strip_bz2_suffix(Path::new("/tmp/foo.bz2")),
            Some(PathBuf::from("/tmp/foo"))
        );
        assert_eq!(strip_bz2_suffix(Path::new("/tmp/foo")), None);
    }
}
