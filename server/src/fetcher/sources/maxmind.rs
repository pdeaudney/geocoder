//! MaxMind GeoLite2-City: license-key URL + tar.gz extract.
//!
//! Stub. Implementation lands in task #56.

use std::path::Path;

use anyhow::Result;
use reqwest::Client;

use crate::fetcher::http::FetchOpts;
use crate::fetcher::FetchOutcome;

pub async fn fetch_maxmind(
    _client: &Client,
    _data_dir: &Path,
    _opts: &FetchOpts,
) -> Result<FetchOutcome> {
    anyhow::bail!("fetcher::sources::maxmind::fetch_maxmind not yet implemented")
}
