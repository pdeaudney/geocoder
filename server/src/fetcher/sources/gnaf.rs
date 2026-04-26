//! G-NAF: license-gated URL fetch + zip extract.
//!
//! G-NAF is a license-gated dataset (Geoscape Australia, CC-BY 4.0
//! release on data.gov.au). The license is per-operator, so we don't
//! hard-code a download URL — the operator pastes a license-accepted
//! URL into `GNAF_ARCHIVE_URL`. Without that env var we skip with a
//! structured remediation message; the build pipeline degrades
//! gracefully (no AU address-points).
//!
//! Output layout: `data/gnaf/psv/*.psv` (flattened from the nested
//! state-level zip layout the upstream archive ships with).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use url::Url;

use crate::fetcher::http::{fetch, FetchOpts};
use crate::fetcher::{FetchOutcome, FetchTarget};

pub async fn fetch_gnaf(
    client: &Client,
    data_dir: &Path,
    opts: &FetchOpts,
) -> Result<FetchOutcome> {
    let archive_url = match std::env::var("GNAF_ARCHIVE_URL") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            return Ok(FetchOutcome::Skipped {
                reason: "GNAF_ARCHIVE_URL unset (paste the license-accepted ZIP URL \
                         from data.gov.au)"
                    .to_string(),
            })
        }
    };
    let url = Url::parse(&archive_url).context("invalid GNAF_ARCHIVE_URL")?;

    let gnaf_dir = data_dir.join("gnaf");
    tokio::fs::create_dir_all(&gnaf_dir).await?;
    let zip_dest = gnaf_dir.join("gnaf.zip");
    let psv_dir = gnaf_dir.join("psv");

    if has_psv_files(&psv_dir).await && !opts.force {
        return Ok(FetchOutcome::Cached);
    }

    let target = FetchTarget {
        url,
        dest: zip_dest.clone(),
        md5_url: None,
    };
    let outcome = fetch(client, &target, opts).await?;

    extract_psv_files(&zip_dest, &psv_dir).await?;
    Ok(outcome)
}

async fn has_psv_files(psv_dir: &Path) -> bool {
    let psv_dir = psv_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> bool {
        let Ok(entries) = std::fs::read_dir(&psv_dir) else {
            return false;
        };
        entries
            .flatten()
            .any(|e| e.path().extension().map(|x| x == "psv").unwrap_or(false))
    })
    .await
    .unwrap_or(false)
}

/// G-NAF archives ship as a tree of state-level zips containing PSV
/// files. We unzip recursively and copy every `*.psv` we find into
/// the flat `psv/` directory the build pipeline expects.
///
/// Pure-Rust extract via the `zip` crate would add a dep (and
/// G-NAF zips are large multi-stream archives that aren't always
/// well-handled). Shelling out to `unzip` keeps us off the critical
/// path of zip-format edge cases.
async fn extract_psv_files(zip_dest: &Path, psv_dir: &Path) -> Result<()> {
    use std::process::Stdio;
    use tokio::process::Command;

    let unpack_dir = zip_dest
        .parent()
        .unwrap_or(Path::new("."))
        .join(".unpack");
    let _ = tokio::fs::remove_dir_all(&unpack_dir).await;
    tokio::fs::create_dir_all(&unpack_dir).await?;
    tokio::fs::create_dir_all(psv_dir).await?;

    let status = Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(zip_dest)
        .arg("-d")
        .arg(&unpack_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("running unzip")?;
    if !status.success() {
        return Err(anyhow!("unzip exited {status}"));
    }

    // Walk the unpack tree and copy every .psv into the flat dir.
    let unpack_dir_clone = unpack_dir.clone();
    let psv_dir_clone = psv_dir.to_path_buf();
    let copied = tokio::task::spawn_blocking(move || -> Result<usize> {
        let mut count = 0usize;
        for entry in walkdir(&unpack_dir_clone)? {
            if entry
                .path()
                .extension()
                .map(|x| x == "psv")
                .unwrap_or(false)
            {
                let dest = psv_dir_clone.join(entry.path().file_name().expect("filename"));
                if dest.exists() {
                    // Two zip entries flattening to the same basename
                    // would silently clobber via std::fs::copy. The
                    // G-NAF archive ships state-prefixed filenames
                    // (`NSW_LOCALITY_psv.psv`, `VIC_LOCALITY_psv.psv`)
                    // so a real-world collision is a sign the archive
                    // is malformed or our flatten convention has
                    // diverged from upstream's directory layout.
                    return Err(anyhow!(
                        "G-NAF flatten collision: {} (from {}) would overwrite an existing flattened PSV. \
                         Two source entries share the same basename — the archive layout has changed; \
                         re-check the upstream G-NAF release notes.",
                        dest.display(),
                        entry.path().display()
                    ));
                }
                std::fs::copy(entry.path(), &dest)
                    .with_context(|| format!("copy to {}", dest.display()))?;
                count += 1;
            }
        }
        Ok(count)
    })
    .await
    .context("walk task panicked")??;

    let _ = tokio::fs::remove_dir_all(&unpack_dir).await;
    tracing::info!(psv_files = copied, "G-NAF PSV files extracted");
    Ok(())
}

/// Minimal recursive directory walk. We avoid pulling in walkdir as a
/// separate crate because we only need this single use case.
fn walkdir(root: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(entry);
            }
        }
    }
    Ok(out)
}
