//! WhosOnFirst admin SQLite (download + bz2 decompress).
//!
//! Stub. Implementation lands in task #54.

use std::path::{Path, PathBuf};

use anyhow::Result;
use reqwest::Client;

use crate::fetcher::http::FetchOpts;
use crate::fetcher::FetchOutcome;

#[derive(Debug)]
pub struct WofResult {
    pub dest: PathBuf,
    pub outcome: Result<FetchOutcome>,
}

pub async fn fetch_wof(
    _client: &Client,
    _data_dir: &Path,
    _scope: &str,
    _opts: &FetchOpts,
) -> Result<Vec<WofResult>> {
    anyhow::bail!("fetcher::sources::wof::fetch_wof not yet implemented")
}
