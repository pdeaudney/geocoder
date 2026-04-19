//! End-to-end sanity test for the OpenAddresses per-country pipeline:
//! synthesise a tiny CSV batch, invoke `build-openaddresses-index`, open
//! the emitted per-country index, and verify find_by_housenumber /
//! find_nearest resolve correctly.
//!
//! Uses a made-up ISO code so we don't collide with any real country's
//! real index when running the whole suite.

use query_server::openaddresses::{oa_prefix, OpenAddresses};
use std::fs;
use std::path::Path;
use std::process::Command;

fn build_tool() -> &'static str {
    // Invoked after `cargo build --release` has populated target/.
    // Path is relative to the workspace root; tests run from
    // server/ by default.
    concat!(env!("CARGO_MANIFEST_DIR"), "/target/release/build-openaddresses-index")
}

fn ensure_tool_built() {
    if !Path::new(build_tool()).exists() {
        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
                "--bin",
                "build-openaddresses-index",
            ])
            .status()
            .expect("cargo build");
        assert!(status.success(), "cargo build failed");
    }
}

#[test]
fn tiny_synthetic_batch_roundtrips_through_build_and_query() {
    ensure_tool_built();

    // Use a temp dir so parallel runs of this test don't stomp on each
    // other, and so the emitted bin files don't pollute the repo.
    let tmp = tempdir();
    let csv_root = tmp.join("csv");
    let out_dir = tmp.join("index");
    let country_dir = csv_root.join("tt"); // "tt" — not any real country's code we use
    fs::create_dir_all(&country_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    // Four synthetic addresses on a single street so we can test
    // housenumber matching and nearest-neighbour scoring.
    let csv = "LON,LAT,NUMBER,STREET,UNIT,CITY,DISTRICT,REGION,POSTCODE,ID,HASH\n\
150.9796,-33.7370,10,Alysse Close,,Baulkham Hills,,NSW,2153,t1,h1\n\
150.9800,-33.7371,11,Alysse Close,,Baulkham Hills,,NSW,2153,t2,h2\n\
150.9804,-33.7372,12,Alysse Close,,Baulkham Hills,,NSW,2153,t3,h3\n\
150.9808,-33.7373,14,Alysse Close,,Baulkham Hills,,NSW,2153,t4,h4\n";
    fs::write(country_dir.join("test.csv"), csv).unwrap();

    // Run the builder with --skip "" so the default AU skip doesn't
    // interfere with our "tt" country.
    let status = Command::new(build_tool())
        .arg(&csv_root)
        .arg(&out_dir)
        .arg("--skip")
        .arg("")
        .status()
        .expect("run build-openaddresses-index");
    assert!(status.success(), "builder exited non-zero");

    // The builder should have emitted the four bin files for "tt".
    let prefix = oa_prefix(*b"TT");
    for suffix in ["_points.bin", "_cells.bin", "_entries.bin", "_strings.bin"] {
        let expected = out_dir.join(format!("{prefix}{suffix}"));
        assert!(expected.exists(), "missing {}", expected.display());
    }

    // Now load via the runtime.
    let oa = OpenAddresses::open(&out_dir)
        .unwrap()
        .expect("per-country index should load");
    assert!(oa.has_country(b"tt"));
    assert!(!oa.has_country(b"zz"));

    // Exact housenumber lookup should return the address's registered coord.
    let m = oa
        .find_by_housenumber(b"tt", "11", Some("Alysse"), -33.7371, 150.9800, 17)
        .expect("should find #11");
    assert_eq!(m.housenumber, "11");
    assert_eq!(m.postcode, "2153");
    assert!(m.street.contains("Alysse"));
    // Coord should match CSV input to f32 precision.
    assert!((m.lat - -33.7371).abs() < 1e-4);
    assert!((m.lng - 150.9800).abs() < 1e-4);

    // Nearest-neighbour to a midpoint between #11 and #12 should return one of them.
    let n = oa
        .find_nearest(b"tt", -33.7371, 150.9802, 17)
        .expect("nearest should hit");
    assert!(matches!(n.housenumber, "11" | "12"), "unexpected {}", n.housenumber);

    // Wrong country code returns None.
    assert!(oa.find_by_housenumber(b"zz", "11", None, -33.7371, 150.9800, 17).is_none());

    cleanup(&tmp);
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "oa-roundtrip-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    base
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}
