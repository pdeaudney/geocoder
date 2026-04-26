//! G-NAF: license-gated URL fetch + zip extract.
//!
//! Stub. Implementation lands in task #56.

use std::path::Path;

use anyhow::Result;
use reqwest::Client;

use crate::fetcher::http::FetchOpts;
use crate::fetcher::FetchOutcome;

pub async fn fetch_gnaf(
    _client: &Client,
    _data_dir: &Path,
    _opts: &FetchOpts,
) -> Result<FetchOutcome> {
    anyhow::bail!("fetcher::sources::gnaf::fetch_gnaf not yet implemented")
}
