//! Interactive probe for the WoF country fallback. Runs only when
//! `GEOCODER_INDEX_DIR` points at an index that contains
//! `wof_countries*.bin` — otherwise it no-ops cleanly so the rest of
//! the test suite still passes on machines without a built index.
//!
//! Kept as a test (rather than an example binary) so `cargo test`
//! covers it automatically — it's cheap to run and catches
//! regressions in the find_country path.

use query_server::wof_countries::WofCountries;
use std::path::Path;

fn open() -> Option<WofCountries> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    WofCountries::open(Path::new(&dir)).ok().flatten()
}

#[test]
fn wof_resolves_known_coords() {
    let Some(wof) = open() else { return };

    // (name, lat, lng, expected_cc)
    let probes: &[(&str, f64, f64, &str)] = &[
        ("Times Square NYC", 40.7580, -73.9855, "US"),
        ("London Trafalgar", 51.5074, -0.1278, "GB"),
        ("Toronto CN Tower", 43.6426, -79.3871, "CA"),
        ("Sydney CBD", -33.8688, 151.2093, "AU"),
        ("Wellington NZ", -41.2865, 174.7762, "NZ"),
    ];

    for (label, lat, lng, expected) in probes {
        let hit = wof.find_country(*lat, *lng);
        match hit {
            Some(m) => {
                let cc = std::str::from_utf8(&m.country_code).unwrap_or("??");
                assert_eq!(
                    cc, *expected,
                    "{label} ({lat}, {lng}) resolved to {cc}, expected {expected}"
                );
                eprintln!("  {label:<20} -> {cc} ({})", m.name);
            }
            None => panic!(
                "{label} ({lat}, {lng}) -> WoF returned None (expected {expected})"
            ),
        }
    }
}
