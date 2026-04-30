//! Proto compilation hook plus build-time provenance capture.
//!
//! Captures the git SHA and dirty flag of the working tree at compile
//! time, exposes them via `cargo:rustc-env`, and the index-builder
//! binaries stamp those values into a per-build manifest at the index
//! root. Operators read the manifest before triggering a planet-scale
//! rebuild to confirm the binary they're about to run actually contains
//! the change they expect — see `server/src/manifest.rs`.

fn main() {
    println!("cargo:rerun-if-changed=proto/geocoder.proto");
    println!("cargo:rerun-if-changed=build.rs");
    // Re-stamp the SHA when the working tree changes. `.git/HEAD`
    // changes only on branch switches; `.git/index` changes on
    // staging operations; same-branch commits update the per-branch
    // ref file (or `.git/packed-refs` when refs are packed). Watch
    // all four so a new commit triggers rebuild.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    println!("cargo:rerun-if-changed=../.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string("../.git/HEAD") {
        if let Some(branch_ref) = head.strip_prefix("ref: ").map(|s| s.trim()) {
            println!("cargo:rerun-if-changed=../.git/{branch_ref}");
        }
    }

    let sha = match git(&["rev-parse", "--short=12", "HEAD"]) {
        Some(s) => s,
        None => "unknown".to_string(),
    };
    let dirty = match git(&["status", "--porcelain"]) {
        Some(s) if !s.is_empty() => "true",
        Some(_) => "false",
        None => "unknown",
    };
    println!("cargo:rustc-env=GEOCODER_GIT_SHA={sha}");
    println!("cargo:rustc-env=GEOCODER_GIT_DIRTY={dirty}");

    if std::env::var_os("CARGO_FEATURE_GRPC").is_none() {
        return;
    }
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/geocoder.proto"], &["proto"])
        .expect("compile geocoder.proto");
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
