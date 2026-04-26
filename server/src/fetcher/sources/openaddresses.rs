//! OpenAddresses fetch via AWS S3 Requester-Pays.
//!
//! Stub. Implementation lands in task #55.

use std::path::{Path, PathBuf};

use anyhow::Result;
use reqwest::Client;

use crate::fetcher::http::FetchOpts;
use crate::fetcher::FetchOutcome;

#[derive(Debug)]
pub struct OaResult {
    pub dest: PathBuf,
    pub outcome: Result<FetchOutcome>,
}

pub async fn fetch_openaddresses(
    _client: &Client,
    _data_dir: &Path,
    _sources: &str,
    _parallel: usize,
    _opts: &FetchOpts,
) -> Result<Vec<OaResult>> {
    anyhow::bail!("fetcher::sources::openaddresses::fetch_openaddresses not yet implemented")
}
