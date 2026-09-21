//! End-to-end sanity test for the OpenAddresses per-country pipeline:
//! synthesise a tiny CSV batch, invoke `build-openaddresses-index`, open
//! the emitted per-country index, and verify find_by_housenumber /
//! find_nearest resolve correctly.
//!
//! Uses a temporary directory, including a made-up ISO code for the
//! original roundtrip case and four production country codes for routing.

use query_server::openaddresses::{oa_prefix, OpenAddresses};
use std::fs;
use std::path::Path;
use std::process::Command;

fn build_tool() -> &'static str {
    env!("CARGO_BIN_EXE_build-openaddresses-index")
}

#[test]
fn tiny_synthetic_batch_roundtrips_through_build_and_query() {
    // Use a temp dir so parallel runs of this test don't stomp on each
    // other, and so the emitted bin files don't pollute the repo.
    let tmp = tempdir();
    let csv_root = tmp.join("csv");
    let out_dir = tmp.join("index");
    let country_dir = csv_root.join("tt"); // "tt" — not any real country's code we use
    fs::create_dir_all(&country_dir).expect("mkdir country_dir");
    fs::create_dir_all(&out_dir).expect("mkdir out_dir");

    // Four synthetic addresses on a single street so we can test
    // housenumber matching and nearest-neighbour scoring.
    let csv = "LON,LAT,NUMBER,STREET,UNIT,CITY,DISTRICT,REGION,POSTCODE,ID,HASH\n\
150.9796,-33.7370,10,Alysse Close,,Baulkham Hills,,NSW,2153,t1,h1\n\
150.9800,-33.7371,11,Alysse Close,,Baulkham Hills,,NSW,2153,t2,h2\n\
150.9804,-33.7372,12,Alysse Close,,Baulkham Hills,,NSW,2153,t3,h3\n\
150.9808,-33.7373,14,Alysse Close,,Baulkham Hills,,NSW,2153,t4,h4\n";
    fs::write(country_dir.join("test.csv"), csv).expect("write test.csv");

    // These four countries use the same on-disk format but distinct shards.
    // A successful lookup must use the requested country's file set.
    for (cc, lon, lat, postcode) in [
        ("nz", 174.7633, -36.8485, "1010"),
        ("us", -74.0060, 40.7128, "10007"),
        ("ca", -79.3832, 43.6532, "M5H2N2"),
        ("gb", -0.1276, 51.5072, "SW1A1AA"),
    ] {
        let dir = csv_root.join(cc);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("test.csv"),
            format!("LON,LAT,NUMBER,STREET,UNIT,CITY,DISTRICT,REGION,POSTCODE,ID,HASH\n{lon},{lat},11,Example Street,,Example City,,,{postcode},id,hash\n"),
        ).unwrap();
    }

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
        .expect("OpenAddresses::open succeeds")
        .expect("per-country index should load");
    assert!(oa.has_country(b"tt"));
    assert!(!oa.has_country(b"zz"));
    for (cc, lon, lat, postcode) in [
        (b"nz", 174.7633, -36.8485, "1010"),
        (b"us", -74.0060, 40.7128, "10007"),
        (b"ca", -79.3832, 43.6532, "M5H2N2"),
        (b"gb", -0.1276, 51.5072, "SW1A1AA"),
    ] {
        assert!(oa.has_country(cc));
        let hit = oa
            .find_by_housenumber(cc, "11", Some("Example"), None, lat, lon, 17)
            .unwrap();
        assert_eq!(hit.postcode, postcode);
    }

    // Exact housenumber lookup should return the address's registered coord.
    let m = oa
        .find_by_housenumber(b"tt", "11", Some("Alysse"), None, -33.7371, 150.9800, 17)
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
    assert!(
        matches!(n.housenumber, "11" | "12"),
        "unexpected {}",
        n.housenumber
    );

    // Wrong country code returns None.
    assert!(oa
        .find_by_housenumber(b"zz", "11", None, None, -33.7371, 150.9800, 17)
        .is_none());

    cleanup(&tmp);
}

#[test]
fn partial_country_shard_fails_to_load() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("oa_gb_cells.bin"), [0u8; 12]).unwrap();
    let err = OpenAddresses::open(dir.path())
        .err()
        .expect("a missing points file must not silently remove GB coverage");
    assert!(err.contains("schema"), "{err}");
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("oa-roundtrip-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("mkdir tempdir");
    base
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}
