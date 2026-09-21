//! Telemetry attribution tests.
//!
//! Verifies that the tracing instrumentation we wired into the query
//! server actually emits the per-query attribution fields (forward
//! tantivy_dir, address_source, autocomplete fst_variant) and the
//! startup file manifest. Operators rely on these to identify corrupt
//! files; if a refactor accidentally drops a `span.record(...)` call
//! the symptom is silent ("the field just isn't there in the logs"),
//! so we test the field presence directly.
//!
//! The tests in this file capture JSON output via a scoped tracing
//! subscriber and parse it back. Tests that need a built index skip
//! gracefully when `GEOCODER_INDEX_DIR` isn't set, matching the rest
//! of the integration-test suite.

use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing::dispatcher::DefaultGuard;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

/// Shared in-memory sink the JSON layer writes to. We hand a clone to
/// the layer (which writes via &mut), and read the inner Vec<u8> back
/// out after the scope closes.
#[derive(Clone, Default)]
struct CaptureBuf(Arc<Mutex<Vec<u8>>>);

impl CaptureBuf {
    fn lines(&self) -> Vec<serde_json::Value> {
        let bytes = self.0.lock().expect("capture buffer poisoned").clone();
        std::str::from_utf8(&bytes)
            .expect("non-utf8 in tracing output")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("non-JSON tracing line {l:?}: {e}"))
            })
            .collect()
    }
}

impl Write for CaptureBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capture buffer poisoned")
            .extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureBuf {
    type Writer = CaptureBuf;
    fn make_writer(&'a self) -> CaptureBuf {
        self.clone()
    }
}

/// Install a JSON tracing layer on the current thread for the lifetime
/// of the returned guard. Spans + events emitted while the guard is in
/// scope land in `buf`; nothing from other threads leaks in (that's
/// the whole point of `set_default` over `init`).
fn capture_json() -> (CaptureBuf, DefaultGuard) {
    let buf = CaptureBuf::default();
    // FmtSpan::CLOSE is what causes span-level attributes (recorded via
    // span.record(...)) to actually appear in the stream — without it,
    // only explicit tracing::info!() events show up. The query-server
    // production setup uses the same flag, so this captures what an
    // operator would see.
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_writer(buf.clone())
        .with_filter(tracing_subscriber::EnvFilter::new("info"));
    let subscriber = tracing_subscriber::registry().with(layer);
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

/// Walk the captured JSON looking for an event whose own span fields
/// (the `span` object emitted by the JSON formatter) contain `key`.
/// Returns the first matching event's span object so callers can
/// assert on multiple fields together.
fn find_span_with_field<'a>(
    events: &'a [serde_json::Value],
    key: &str,
) -> Option<&'a serde_json::Value> {
    for ev in events {
        if let Some(span) = ev.get("span") {
            if span.get(key).is_some() {
                return Some(span);
            }
        }
        // tracing-subscriber JSON also emits a `spans` array (the
        // current-span stack); search there too in case the field was
        // recorded on a parent span.
        if let Some(spans) = ev.get("spans").and_then(|s| s.as_array()) {
            for s in spans {
                if s.get(key).is_some() {
                    return Some(s);
                }
            }
        }
    }
    None
}

#[test]
fn log_loaded_file_emits_manifest_line() {
    let dir = tempdir();
    let path = dir.path().join("synthetic.bin");
    std::fs::write(&path, b"hello world").expect("write temp file");

    let (buf, _guard) = capture_json();
    query_server::log_loaded_file("test_index", path.to_str().expect("utf-8 path"), 11);
    drop(_guard);

    let events = buf.lines();
    let manifest_line = events
        .iter()
        .find(|e| e.get("target").and_then(|t| t.as_str()) == Some("query_server::manifest"))
        .expect("expected at least one manifest line on query_server::manifest target");
    let fields = manifest_line
        .get("fields")
        .expect("event has fields object");
    assert_eq!(
        fields.get("index").and_then(|v| v.as_str()),
        Some("test_index")
    );
    assert_eq!(fields.get("size_bytes").and_then(|v| v.as_u64()), Some(11));
    assert!(
        fields
            .get("path")
            .and_then(|v| v.as_str())
            .expect("path field")
            .ends_with("synthetic.bin"),
        "path field should reference the temp file"
    );
    // mtime_unix is "now" — we just check it's a positive integer rather
    // than pinning a specific timestamp.
    assert!(
        fields
            .get("mtime_unix")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            > 0,
        "mtime_unix should be populated for an existing file"
    );
}

#[cfg(feature = "forward")]
#[test]
fn forward_search_records_tantivy_dir_and_variant() {
    let Some(forward) = load_forward_or_skip() else {
        return;
    };

    let (buf, _guard) = capture_json();
    let _ = forward
        .search_structured(query_server::forward::StructuredQuery {
            q: Some("sydney"),
            kind: Some(query_server::forward::KIND_PLACE),
            limit: 5,
            ..Default::default()
        })
        .expect("forward search should not error on a healthy index");
    drop(_guard);

    let events = buf.lines();
    let span = find_span_with_field(&events, "geocoder.forward.tantivy_dir")
        .expect("expected at least one span carrying geocoder.forward.tantivy_dir");

    let tantivy_dir = span
        .get("geocoder.forward.tantivy_dir")
        .and_then(|v| v.as_str())
        .expect("tantivy_dir should be a string");
    // Has to be either the monolithic name or a per-country variant; an
    // empty value would mean we plumbed the field but never recorded it.
    assert!(
        tantivy_dir == "tantivy" || tantivy_dir.starts_with("tantivy_"),
        "unexpected tantivy_dir value {tantivy_dir:?}"
    );

    let variant = find_span_with_field(&events, "geocoder.forward.index_variant")
        .and_then(|s| s.get("geocoder.forward.index_variant"))
        .and_then(|v| v.as_str())
        .expect("expected index_variant to be recorded alongside tantivy_dir");
    assert!(
        variant == "default" || variant == "per_country",
        "unexpected index_variant value {variant:?}"
    );
}

#[cfg(feature = "forward")]
#[test]
fn forward_search_records_stage_progression() {
    // Strict-rung happy path: a query that resolves at the top of the
    // ladder should record geocoder.stage="strict" on the parent span.
    // If we accidentally regress to "no record" the smoke test would
    // fail silently — this catches that.
    let Some(forward) = load_forward_or_skip() else {
        return;
    };

    let (buf, _guard) = capture_json();
    let _ = forward
        .search_structured(query_server::forward::StructuredQuery {
            q: Some("sydney"),
            kind: Some(query_server::forward::KIND_PLACE),
            limit: 5,
            ..Default::default()
        })
        .expect("forward search should not error");
    drop(_guard);

    let events = buf.lines();
    // Look for the parent search_structured span: it has both stage and
    // match_count recorded after the strict rung succeeds.
    let span = events
        .iter()
        .filter_map(|e| e.get("span"))
        .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("forward.search_structured"))
        .expect("expected a forward.search_structured span in captured output");
    let stage = span
        .get("geocoder.stage")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        [
            "strict",
            "fanout",
            "fuzzy",
            "drop_location_suffix",
            "exhausted"
        ]
        .contains(&stage),
        "unexpected geocoder.stage value {stage:?}"
    );
    let count = span
        .get("geocoder.match_count")
        .and_then(|v| v.as_u64())
        .expect("match_count should be populated");
    assert!(count > 0, "search for 'sydney' should produce hits");
}

#[test]
fn index_load_emits_manifest_line_per_mmap_file() {
    // End-to-end check: every file Index::load mmap's must produce a
    // manifest line via the mmap_file → log_loaded_file path. Catches
    // regressions where a refactor adds a new mmap call but forgets
    // the helper, leaving a file invisible to operators.
    let Some(dir) = std::env::var("GEOCODER_INDEX_DIR").ok() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR not set");
        return;
    };

    let (buf, _guard) = capture_json();
    let _ = query_server::Index::load(
        &dir,
        query_server::DEFAULT_STREET_CELL_LEVEL,
        query_server::DEFAULT_ADMIN_CELL_LEVEL,
        query_server::DEFAULT_SEARCH_DISTANCE,
    )
    .expect("index load should succeed");
    drop(_guard);

    let events = buf.lines();
    let manifest_lines: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e.get("target").and_then(|t| t.as_str()) == Some("query_server::manifest"))
        .collect();

    // The reverse index alone has 22 mandatory + a few optional files.
    // Any healthy index produces at least the mandatory set; we just
    // assert "more than a handful" rather than pinning an exact count
    // since optional files vary by deployment.
    assert!(
        manifest_lines.len() >= 10,
        "expected at least 10 manifest lines for a real index, got {}",
        manifest_lines.len()
    );

    // Every manifest line must carry the four required fields. If any
    // are missing the operator can't correlate a query to a file.
    for line in &manifest_lines {
        let fields = line.get("fields").expect("manifest event has fields");
        for required in ["index", "path", "size_bytes", "mtime_unix"] {
            assert!(
                fields.get(required).is_some(),
                "manifest line missing required field {required:?}: {line}"
            );
        }
    }

    // The reverse-index group must be present — that's the load path
    // we just exercised.
    assert!(
        manifest_lines.iter().any(|l| l
            .get("fields")
            .and_then(|f| f.get("index"))
            .and_then(|v| v.as_str())
            == Some("reverse")),
        "expected at least one manifest line tagged index=reverse"
    );
}

#[cfg(feature = "forward")]
fn load_forward_or_skip() -> Option<query_server::forward::Forward> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let path = std::path::PathBuf::from(&dir);
    let tantivy_dir = path.join("tantivy");
    if !tantivy_dir.exists() && !path.join("meta.json").exists() {
        eprintln!(
            "SKIP: no tantivy index under {} — run build-forward-index first",
            path.display()
        );
        return None;
    }
    match query_server::forward::Forward::open(&path) {
        Ok(f) if !f.is_empty() => Some(f),
        Ok(_) => {
            eprintln!("SKIP: forward index opened but contains no shards");
            None
        }
        Err(e) => {
            eprintln!("SKIP: could not open forward index: {e}");
            None
        }
    }
}

// --- tempdir helper ---
//
// Avoids pulling tempfile into dev-deps for one test. RAII-cleaned on
// drop so the test doesn't litter /tmp. Used only by the
// log_loaded_file test.

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tempdir() -> TempDir {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "geocoder-telemetry-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&p).expect("create temp dir");
    TempDir(p)
}
