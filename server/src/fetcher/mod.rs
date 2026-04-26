//! Build-data acquisition for the geocoder pipeline.
//!
//! Replaces the bash `download-region.sh` / `fetch-build-data.sh` /
//! `fetch-openaddresses.sh` trio. Drives every external source the
//! build pipeline consumes (OSM PBF, WhosOnFirst admin SQLite,
//! OpenAddresses, MaxMind GeoLite2, G-NAF) through a single
//! conditional-GET + resumable-download core.
//!
//! The binary entry point lives in `src/bin/fetch_data.rs`; this
//! module exposes the library API used by both that binary and the
//! integration test suite.
//!
//! Design highlights:
//!
//!   - `http`: single state machine for conditional GET (If-None-Match,
//!     If-Modified-Since), resumable downloads (Range, If-Range), and
//!     atomic rename. All sources route through it for one set of
//!     correctness invariants.
//!   - `region`: ports the bash region-preset case statement to a
//!     typed enum + URL set so the test of "are all 13 presets
//!     resolving correctly?" becomes a table-driven cargo test.
//!   - `state`: parse + serialize Osmosis-format `state.txt`. Sidecars
//!     persist replication metadata so `update-index.sh` and the
//!     operator both have explicit visibility into what date snapshot
//!     is on disk.
//!   - `mismatch`: typed planet/continent mismatch detection with
//!     `Display`-formatted remediation text — replaces the bash heredoc
//!     check shipped earlier in PR #9.

pub mod http;
pub mod mismatch;
pub mod region;
pub mod sources;
pub mod state;

use std::path::PathBuf;

/// Outcome of a single per-file fetch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// Server returned `304 Not Modified` — local file is current.
    Cached,
    /// Resumed a partial download with `Range`.
    Resumed { bytes: u64, total: u64 },
    /// Full body downloaded.
    Downloaded { bytes: u64 },
    /// Source isn't in scope for this run (e.g. license-gated and not configured).
    Skipped { reason: String },
}

/// Logical fetch target. The HTTP layer doesn't care which source
/// produced it.
#[derive(Debug, Clone)]
pub struct FetchTarget {
    pub url: url::Url,
    pub dest: PathBuf,
    /// MD5 sidecar URL (Geofabrik publishes; OA / MaxMind don't).
    pub md5_url: Option<url::Url>,
}
