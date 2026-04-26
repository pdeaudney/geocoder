//! Build-data acquisition for the geocoder pipeline.
//!
//! Drives every external source the build pipeline consumes (OSM PBF,
//! WhosOnFirst admin SQLite, OpenAddresses, MaxMind GeoLite2, G-NAF)
//! through a single conditional-GET + resumable-download core.
//!
//! The binary entry point lives in `src/bin/fetch_data.rs`; this
//! module exposes the library API used by both that binary and the
//! integration test suite.
//!
//! Module map:
//!
//!   - `http`: single state machine for conditional GET
//!     (`If-None-Match`, `If-Modified-Since`), resumable downloads
//!     (`Range`, `If-Range`), and atomic `<dest>.partial → <dest>`
//!     rename. All sources route through it for one set of
//!     correctness invariants.
//!   - `region`: typed `Region` enum + URL preset table. Table-driven
//!     unit tests pin every preset's URL so a typo can't silently
//!     reroute downloads.
//!   - `state`: parse + serialize Osmosis-format `state.txt`. Sidecars
//!     persist replication metadata so `update-index.sh` and the
//!     operator both have explicit visibility into what date snapshot
//!     is on disk.
//!   - `mismatch`: typed planet/continent directory consistency check
//!     with `Display`-formatted remediation text. Prevents
//!     `build-index` from silently double-processing an OSM
//!     directory that mixes the two patterns.

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
