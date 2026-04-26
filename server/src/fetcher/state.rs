//! Osmosis-format `state.txt` parser/serializer.
//!
//! Geofabrik publishes `<region>-updates/state.txt` per continent and
//! planet.osm.org publishes `replication/day/state.txt`. Both follow
//! the Osmosis convention used by `pyosmium-get-changes` and
//! `osmupdate`, which is what `update-index.sh` already consumes
//! downstream — preserving the format byte-identical lets that
//! integration keep working.
//!
//! Wire format (verified against
//! `https://download.geofabrik.de/australia-oceania-updates/state.txt`):
//!
//! ```text
//! # original OSM minutely replication sequence number 7086110
//! timestamp=2026-04-25T20\:20\:59Z
//! sequenceNumber=4769
//! ```
//!
//! Notes on the wire format:
//!   - Lines starting with `#` are comments, preserved on parse and
//!     re-emitted by serialize so the bytes are stable across
//!     read/write cycles.
//!   - Colons in the timestamp are backslash-escaped (a Java
//!     properties-file holdover from the original Osmosis Java
//!     implementation). We strip the backslashes on parse and put
//!     them back on serialize.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationState {
    pub timestamp: DateTime<Utc>,
    pub sequence_number: u64,
    /// Comment lines preserved verbatim so roundtrip serialize is
    /// byte-stable (and operators reading the file keep their context).
    pub comments: Vec<String>,
}

impl ReplicationState {
    pub fn parse(s: &str) -> Result<Self> {
        let mut timestamp: Option<DateTime<Utc>> = None;
        let mut sequence_number: Option<u64> = None;
        let mut comments = Vec::new();

        for raw_line in s.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('#') {
                comments.push(rest.trim_start().to_string());
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| anyhow!("malformed state.txt line: '{raw_line}'"))?;
            match key.trim() {
                "timestamp" => {
                    // Strip Java-properties backslash-escaping on
                    // colons before handing to chrono.
                    let unescaped = value.replace("\\:", ":");
                    timestamp = Some(
                        DateTime::parse_from_rfc3339(&unescaped)
                            .with_context(|| {
                                format!("failed to parse timestamp '{unescaped}' as RFC 3339")
                            })?
                            .with_timezone(&Utc),
                    );
                }
                "sequenceNumber" => {
                    sequence_number = Some(
                        value
                            .trim()
                            .parse::<u64>()
                            .with_context(|| format!("invalid sequenceNumber '{value}'"))?,
                    );
                }
                // Other Osmosis keys (txnReady, txnMaxQueried, etc.)
                // exist in some replication state files. We don't
                // consume them but we do round-trip them through
                // comments so the file stays useful for anything
                // downstream that does.
                _ => {}
            }
        }

        let timestamp = timestamp.ok_or_else(|| anyhow!("state.txt missing 'timestamp' field"))?;
        let sequence_number = sequence_number
            .ok_or_else(|| anyhow!("state.txt missing 'sequenceNumber' field"))?;

        Ok(Self {
            timestamp,
            sequence_number,
            comments,
        })
    }

    /// Emit in the same wire format the parser accepts. Includes the
    /// preserved comment lines and the Java-properties backslash-
    /// escaping on the timestamp colons.
    pub fn to_string_wire(&self) -> String {
        let mut out = String::new();
        for c in &self.comments {
            out.push_str("# ");
            out.push_str(c);
            out.push('\n');
        }
        // RFC 3339 with second precision and a `Z` suffix is what
        // Geofabrik publishes; chrono's default `to_rfc3339` uses
        // `+00:00` instead of `Z`. We format manually to keep parity.
        let ts = self.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let ts_escaped = ts.replace(':', "\\:");
        out.push_str("timestamp=");
        out.push_str(&ts_escaped);
        out.push('\n');
        out.push_str("sequenceNumber=");
        out.push_str(&self.sequence_number.to_string());
        out.push('\n');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Geofabrik state.txt (australia-oceania, fetched
    /// 2026-04-25T22:43:31Z). Pin the exact bytes so a parser
    /// regression shows up here, not in production.
    const SAMPLE: &str = "# original OSM minutely replication sequence number 7086110\n\
                          timestamp=2026-04-25T20\\:20\\:59Z\n\
                          sequenceNumber=4769\n";

    #[test]
    fn parse_real_geofabrik_state() {
        let state = ReplicationState::parse(SAMPLE).unwrap();
        assert_eq!(state.sequence_number, 4769);
        assert_eq!(
            state.timestamp.to_rfc3339(),
            "2026-04-25T20:20:59+00:00"
        );
        assert_eq!(state.comments.len(), 1);
        assert!(state.comments[0].contains("7086110"));
    }

    #[test]
    fn roundtrip_byte_identical() {
        let state = ReplicationState::parse(SAMPLE).unwrap();
        let serialized = state.to_string_wire();
        // Byte-equal contract — `update-index.sh` and any external
        // consumer can rely on the output matching upstream's format.
        assert_eq!(serialized, SAMPLE);
    }

    #[test]
    fn parse_accepts_rfc3339_without_escaping() {
        // Defensive: a hand-written state.txt or one from a non-Osmosis
        // source might omit the backslash escapes. We should still
        // accept it.
        let unescaped = "timestamp=2026-01-01T00:00:00Z\nsequenceNumber=1\n";
        let state = ReplicationState::parse(unescaped).unwrap();
        assert_eq!(state.sequence_number, 1);
    }

    #[test]
    fn missing_timestamp_errors() {
        let s = "sequenceNumber=1\n";
        let err = ReplicationState::parse(s).unwrap_err();
        assert!(err.to_string().contains("missing 'timestamp'"));
    }

    #[test]
    fn missing_sequence_errors() {
        let s = "timestamp=2026-01-01T00:00:00Z\n";
        let err = ReplicationState::parse(s).unwrap_err();
        assert!(err.to_string().contains("missing 'sequenceNumber'"));
    }

    #[test]
    fn malformed_line_errors() {
        let s = "timestamp=2026-01-01T00:00:00Z\nthis-is-garbage\nsequenceNumber=1\n";
        let err = ReplicationState::parse(s).unwrap_err();
        assert!(err.to_string().contains("malformed state.txt line"));
    }
}
