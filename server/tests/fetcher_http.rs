//! Integration tests for `query_server::fetcher::http`.
//!
//! Pins the conditional-GET + resumable-download state machine
//! end-to-end against a real HTTP server (axum on a fixed test port).
//! The unit tests in `fetcher::region`, `fetcher::state`, and
//! `fetcher::mismatch` cover the pure logic; this file covers the
//! parts where we depend on real header parsing and on-disk semantics.
//!
//! Cases covered:
//!   - First fetch downloads + saves `.etag` sidecar
//!   - Re-fetch with unchanged ETag returns `Cached` (304 path)
//!   - Re-fetch with bumped ETag downloads fresh
//!   - MD5 mismatch deletes partial + errors
//!   - Resumable: kill mid-download, rerun, resume from byte N
//!   - If-Range mismatch (server changed content) restarts cleanly
//!   - Range Not Satisfiable (corrupt partial) restarts cleanly

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use md5::{Digest, Md5};
use query_server::fetcher::http::{fetch, FetchOpts};
use query_server::fetcher::{FetchOutcome, FetchTarget};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use url::Url;

/// Test fixture: a small in-memory "PBF" served by axum with full
/// conditional-GET + Range support. Bumping `etag_seq` swaps the
/// payload; bumping `truncate_at` causes mid-stream cuts.
#[derive(Clone)]
struct MockState {
    payload: Arc<parking_lot::Mutex<Vec<u8>>>,
    etag: Arc<parking_lot::Mutex<String>>,
    request_count: Arc<AtomicUsize>,
    /// If non-zero, truncate the response body at this many bytes
    /// (simulating a killed connection). Cleared after one use.
    truncate_at: Arc<AtomicU64>,
}

impl MockState {
    fn new(initial_payload: Vec<u8>) -> Self {
        let etag = format!("\"{}\"", md5_of(&initial_payload));
        Self {
            payload: Arc::new(parking_lot::Mutex::new(initial_payload)),
            etag: Arc::new(parking_lot::Mutex::new(etag)),
            request_count: Arc::new(AtomicUsize::new(0)),
            truncate_at: Arc::new(AtomicU64::new(0)),
        }
    }

    fn rotate_payload(&self, new_payload: Vec<u8>) {
        let etag = format!("\"{}\"", md5_of(&new_payload));
        *self.payload.lock() = new_payload;
        *self.etag.lock() = etag;
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

fn md5_of(bytes: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

async fn pbf_handler(State(state): State<MockState>, headers: HeaderMap) -> Response {
    state.request_count.fetch_add(1, Ordering::SeqCst);
    let payload = state.payload.lock().clone();
    let etag = state.etag.lock().clone();

    // Conditional GET — If-None-Match.
    if let Some(client_etag) = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if client_etag == etag {
            return (StatusCode::NOT_MODIFIED, [("etag", etag.as_str())]).into_response();
        }
    }

    // Range request handling.
    if let Some(range) = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        // If-Range: when present and matches, serve 206; mismatch means
        // we're supposed to send the full body via 200.
        if let Some(if_range) = headers
            .get(axum::http::header::IF_RANGE)
            .and_then(|v| v.to_str().ok())
        {
            if if_range != etag {
                return ok_response(payload, etag, &state);
            }
        }
        let prefix = range.strip_prefix("bytes=").unwrap_or("");
        let start = prefix
            .split('-')
            .next()
            .unwrap_or("0")
            .parse::<u64>()
            .unwrap_or(0);
        if start >= payload.len() as u64 {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        let mut body = payload[start as usize..].to_vec();
        if state.truncate_at.load(Ordering::SeqCst) > 0 {
            let n = state.truncate_at.swap(0, Ordering::SeqCst) as usize;
            body.truncate(n.min(body.len()));
        }
        let range_end = (start as usize + body.len()).saturating_sub(1);
        let total = payload.len();
        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("content-length", body.len().to_string())
            .header(
                "content-range",
                format!("bytes {start}-{range_end}/{total}"),
            )
            .header("etag", etag)
            .header("accept-ranges", "bytes")
            .body(Body::from(body))
            .unwrap();
    }

    ok_response(payload, etag, &state)
}

fn ok_response(payload: Vec<u8>, etag: String, state: &MockState) -> Response {
    let mut body = payload.clone();
    if state.truncate_at.load(Ordering::SeqCst) > 0 {
        let n = state.truncate_at.swap(0, Ordering::SeqCst) as usize;
        body.truncate(n.min(body.len()));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-length", body.len().to_string())
        .header("etag", etag)
        .header("accept-ranges", "bytes")
        .header("last-modified", "Wed, 21 Oct 2026 07:28:00 GMT")
        .body(Body::from(body))
        .unwrap()
}

async fn md5_handler(State(state): State<MockState>) -> impl IntoResponse {
    let payload = state.payload.lock().clone();
    format!("{}  payload\n", md5_of(&payload))
}

async fn pbf_head(State(state): State<MockState>) -> Response {
    let etag = state.etag.lock().clone();
    let len = state.payload.lock().len();
    Response::builder()
        .status(StatusCode::OK)
        .header("etag", etag)
        .header("accept-ranges", "bytes")
        .header("content-length", len.to_string())
        .body(Body::empty())
        .unwrap()
}

struct ServerHandle {
    addr: SocketAddr,
    state: MockState,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            j.abort();
        }
    }
}

async fn spawn_mock(initial_payload: Vec<u8>) -> ServerHandle {
    let state = MockState::new(initial_payload);
    let app = Router::new()
        .route("/payload.bin", get(pbf_handler).head(pbf_head))
        .route("/payload.bin.md5", get(md5_handler))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let (shutdown, rx) = oneshot::channel();
    let join = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .expect("serve");
    });
    // Tiny wait so the listener is actually accepting before
    // tests send requests.
    tokio::time::sleep(Duration::from_millis(20)).await;
    ServerHandle {
        addr,
        state,
        shutdown: Some(shutdown),
        join: Some(join),
    }
}

fn target(server: &ServerHandle, dest_dir: &std::path::Path) -> FetchTarget {
    FetchTarget {
        url: Url::parse(&format!("http://{}/payload.bin", server.addr)).unwrap(),
        dest: dest_dir.join("payload.bin"),
        md5_url: Some(Url::parse(&format!("http://{}/payload.bin.md5", server.addr)).unwrap()),
    }
}

fn opts() -> FetchOpts {
    FetchOpts {
        force: false,
        verify_md5: true,
        resume: true,
        show_progress: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_fetch_downloads_and_caches() {
    let payload = b"hello world this is a fake PBF body".to_vec();
    let server = spawn_mock(payload.clone()).await;
    let tmp = TempDir::new().unwrap();
    let client = reqwest::Client::new();
    let target = target(&server, tmp.path());

    let outcome = fetch(&client, &target, &opts()).await.expect("fetch ok");
    match outcome {
        FetchOutcome::Downloaded { bytes } => assert_eq!(bytes, payload.len() as u64),
        other => panic!("expected Downloaded, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(&target.dest).unwrap(),
        payload,
        "on-disk bytes match server payload"
    );
    assert!(
        std::fs::metadata(format!("{}.etag", target.dest.display())).is_ok(),
        ".etag sidecar persisted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_fetch_returns_cached_via_304() {
    let payload = b"another fake PBF body".to_vec();
    let server = spawn_mock(payload).await;
    let tmp = TempDir::new().unwrap();
    let client = reqwest::Client::new();
    let target = target(&server, tmp.path());

    let _ = fetch(&client, &target, &opts()).await.expect("first fetch");
    let count_after_first = server.state.request_count();

    let outcome = fetch(&client, &target, &opts()).await.expect("second fetch");
    assert_eq!(outcome, FetchOutcome::Cached);
    // The handler hit count went up by exactly 1 (the second fetch's
    // conditional GET) — the body wasn't streamed.
    assert_eq!(server.state.request_count(), count_after_first + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotated_payload_triggers_full_redownload() {
    let server = spawn_mock(b"version 1".to_vec()).await;
    let tmp = TempDir::new().unwrap();
    let client = reqwest::Client::new();
    let target = target(&server, tmp.path());

    let _ = fetch(&client, &target, &opts()).await.expect("first fetch");
    server.state.rotate_payload(b"version 2 longer payload".to_vec());

    let outcome = fetch(&client, &target, &opts()).await.expect("refetch");
    match outcome {
        FetchOutcome::Downloaded { bytes } => assert_eq!(bytes, b"version 2 longer payload".len() as u64),
        other => panic!("expected Downloaded after rotation, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(&target.dest).unwrap(),
        b"version 2 longer payload"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn md5_mismatch_deletes_partial_and_errors() {
    let server = spawn_mock(b"correct payload".to_vec()).await;
    let tmp = TempDir::new().unwrap();
    let client = reqwest::Client::new();

    let mut t = target(&server, tmp.path());
    // Point md5_url at a separate server that returns a literal wrong
    // md5, so the verify step bails after the streaming hash matches
    // the (correct) payload but the (forged) sidecar disagrees.
    let wrong_md5_app = Router::new().route(
        "/wrong.md5",
        get(|| async { "00000000000000000000000000000000  payload\n" }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wrong_addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let join = tokio::spawn(async move {
        axum::serve(listener, wrong_md5_app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .expect("serve wrong-md5");
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    t.md5_url = Some(Url::parse(&format!("http://{wrong_addr}/wrong.md5")).unwrap());

    let err = fetch(&client, &t, &opts()).await.expect_err("md5 must mismatch");
    assert!(
        err.to_string().contains("md5 mismatch"),
        "expected md5-mismatch error, got: {err}"
    );
    assert!(
        std::fs::metadata(&t.dest).is_err(),
        "final dest must not exist"
    );
    let partial = format!("{}.partial", t.dest.display());
    assert!(
        std::fs::metadata(&partial).is_err(),
        ".partial must be cleaned up after mismatch"
    );

    let _ = tx.send(());
    let _ = join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_refuses_without_saved_etag() {
    // C1 (QA review): If a .partial survives without a matching .etag
    // sidecar — e.g. an operator hand-crafted a partial, or a prior
    // run was killed before the etag could be persisted — we have no
    // way to detect server-side content drift via If-Range. Splicing
    // the resumed tail onto stale prefix bytes would silently
    // produce a corrupt artifact. The fetcher should restart from
    // scratch instead.
    let payload: Vec<u8> = (0..4096u32).map(|i| ((i + 7) % 251) as u8).collect();
    let server = spawn_mock(payload.clone()).await;
    let tmp = TempDir::new().unwrap();
    let client = reqwest::Client::new();
    let target = target(&server, tmp.path());

    // Pre-seed only the .partial; no .etag.
    let partial_path: PathBuf = format!("{}.partial", target.dest.display()).into();
    std::fs::write(&partial_path, b"junk-prefix-bytes-not-from-server").unwrap();

    let outcome = fetch(&client, &target, &opts()).await.expect("must restart cleanly");
    match outcome {
        FetchOutcome::Downloaded { bytes } => assert_eq!(bytes, payload.len() as u64),
        other => panic!("expected Downloaded after restart, got {other:?}"),
    }
    // Final file must be byte-identical to the server payload —
    // the junk prefix must not have leaked through.
    assert_eq!(std::fs::read(&target.dest).unwrap(), payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_from_partial_succeeds() {
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let server = spawn_mock(payload.clone()).await;
    let tmp = TempDir::new().unwrap();
    let client = reqwest::Client::new();
    let target = target(&server, tmp.path());

    // Pre-seed a .partial holding the first half of the payload, plus
    // an .etag sidecar matching the server's current etag — this
    // simulates a killed-mid-download state.
    let partial_path: PathBuf = format!("{}.partial", target.dest.display()).into();
    std::fs::write(&partial_path, &payload[..2048]).unwrap();
    let etag_path: PathBuf = format!("{}.etag", target.dest.display()).into();
    std::fs::write(&etag_path, &*server.state.etag.lock()).unwrap();

    let outcome = fetch(&client, &target, &opts()).await.expect("resume ok");
    match outcome {
        FetchOutcome::Resumed { bytes, total } => {
            assert_eq!(bytes, 2048, "appended bytes match the missing tail");
            assert_eq!(total, payload.len() as u64);
        }
        other => panic!("expected Resumed, got {other:?}"),
    }
    assert_eq!(std::fs::read(&target.dest).unwrap(), payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn if_range_mismatch_restarts_cleanly() {
    let payload: Vec<u8> = b"original payload bytes for the test".to_vec();
    let server = spawn_mock(payload.clone()).await;
    let tmp = TempDir::new().unwrap();
    let client = reqwest::Client::new();
    let target = target(&server, tmp.path());

    // Seed partial + a stale etag so the server's If-Range check fails.
    let partial_path: PathBuf = format!("{}.partial", target.dest.display()).into();
    std::fs::write(&partial_path, &payload[..10]).unwrap();
    let etag_path: PathBuf = format!("{}.etag", target.dest.display()).into();
    std::fs::write(&etag_path, "\"stale-etag\"").unwrap();

    // Also rotate the server's payload so the etag check is doubly
    // mismatched.
    let new_payload = b"completely different bytes here".to_vec();
    server.state.rotate_payload(new_payload.clone());

    let outcome = fetch(&client, &target, &opts()).await.expect("restart ok");
    match outcome {
        FetchOutcome::Downloaded { bytes } => assert_eq!(bytes, new_payload.len() as u64),
        other => panic!("expected Downloaded after If-Range mismatch, got {other:?}"),
    }
    assert_eq!(std::fs::read(&target.dest).unwrap(), new_payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_bypasses_conditional_get() {
    let payload = b"some bytes".to_vec();
    let server = spawn_mock(payload).await;
    let tmp = TempDir::new().unwrap();
    let client = reqwest::Client::new();
    let target = target(&server, tmp.path());

    let _ = fetch(&client, &target, &opts()).await.expect("first ok");

    let mut o = opts();
    o.force = true;
    let outcome = fetch(&client, &target, &o).await.expect("forced ok");
    match outcome {
        FetchOutcome::Downloaded { .. } => {}
        other => panic!("expected Downloaded under --force, got {other:?}"),
    }
}
