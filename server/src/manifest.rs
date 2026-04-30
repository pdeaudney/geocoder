//! Per-builder index manifest emitter.
//!
//! Each index builder writes a tiny JSON file at the index root
//! (`manifest_<tool>.json`) that records the git SHA, dirty flag, and
//! key counts of the data it just produced. Operators inspect these
//! before / after a rebuild to confirm the new binary actually changed
//! the on-disk index, instead of burning a 15-hour planet rebuild on a
//! binary that turns out to have the same code as the previous one.
//!
//! The manifest is human-readable; future versions of `Forward::open`
//! may parse it for sanity checks but today's contract is operator-only.

use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// Git SHA of the source tree when the binary was compiled. Captured
/// by `build.rs`. `"unknown"` when the build host has no git access.
pub fn git_sha() -> &'static str {
    env!("GEOCODER_GIT_SHA")
}

/// `"true"` if the working tree had uncommitted changes at build time,
/// `"false"` if clean, `"unknown"` if git wasn't queryable.
pub fn git_dirty() -> &'static str {
    env!("GEOCODER_GIT_DIRTY")
}

/// Write `<dir>/manifest_<tool>.json`. `extra` is merged into the
/// top-level object so callers can record their tool-specific stats
/// (place counts, country breakdown, etc.) without going through this
/// module.
pub fn write(dir: &Path, tool: &str, extra: Value) -> std::io::Result<()> {
    let now = chrono::Utc::now();
    let mut obj = json!({
        "tool": tool,
        "git_sha": git_sha(),
        "git_dirty": git_dirty() == "true",
        "git_dirty_known": git_dirty() != "unknown",
        "built_at_unix": now.timestamp(),
        "built_at_iso": now.to_rfc3339(),
    });
    if let (Some(top), Value::Object(extras)) = (obj.as_object_mut(), extra) {
        for (k, v) in extras {
            top.insert(k, v);
        }
    }
    let path = dir.join(format!("manifest_{tool}.json"));
    let mut f = std::fs::File::create(&path)?;
    serde_json::to_writer_pretty(&mut f, &obj)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    f.write_all(b"\n")?;
    eprintln!("wrote {}", path.display());
    Ok(())
}
