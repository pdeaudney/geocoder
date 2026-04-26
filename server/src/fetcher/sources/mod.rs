//! Per-source fetch logic. Each module wraps the shared `http` core
//! with source-specific URL derivation, sidecar handling, and any
//! post-processing (decompress, extract).

pub mod gnaf;
pub mod maxmind;
pub mod openaddresses;
pub mod osm;
pub mod wof;
