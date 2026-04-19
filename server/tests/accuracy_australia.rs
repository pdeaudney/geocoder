//! End-to-end accuracy test against a real Australia index.
//!
//! Requires `GEOCODER_INDEX_DIR` to point at a directory containing the 14
//! .bin files produced by `build-index` from `australia-latest.osm.pbf`.
//! When the variable is unset the test is skipped (not failed) so `cargo test`
//! remains viable on machines without the index.
//!
//! Ground-truth coordinates are mid-road or mid-suburb points picked from
//! well-known Australian landmarks. Assertions are deliberately loose on
//! street names (substring, case-insensitive) because OSM naming varies, but
//! strict on admin boundaries (country / state) which are unambiguous.

use query_server::{
    Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};

/// Assertions for a fixture. Hard asserts: country, state, city for CBD
/// fixtures (after the country-aware admin mapping, AU LGAs surface as city),
/// and road-is-some for urban fixtures. Road-name substring matches are
/// informational because OSM naming around the query point can vary.
struct Fixture {
    name: &'static str,
    lat: f64,
    lng: f64,
    expect_country: Option<&'static str>,
    expect_country_code: Option<&'static str>,
    expect_state: Option<&'static str>,
    /// Substring match against the city field (case-insensitive). `None` means
    /// don't assert on city.
    expect_city_contains: Option<&'static str>,
    /// Expect some road to be returned.
    expect_some_road: bool,
    /// Informational-only road-name hint.
    hint_road_contains: Option<&'static str>,
}

const FIXTURES: &[Fixture] = &[
    // Sydney Opera House — sits on a pier, so nearby road coverage is thin.
    Fixture {
        name: "sydney_opera_house",
        lat: -33.8568,
        lng: 151.2153,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("New South Wales"),
        expect_city_contains: Some("Sydney"),
        expect_some_road: false,
        hint_road_contains: None,
    },
    Fixture {
        name: "sydney_elizabeth_st",
        lat: -33.8745,
        lng: 151.2090,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("New South Wales"),
        expect_city_contains: Some("Sydney"),
        expect_some_road: true,
        hint_road_contains: Some("elizabeth"),
    },
    Fixture {
        name: "melbourne_flinders_st",
        lat: -37.8183,
        lng: 144.9671,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("Victoria"),
        expect_city_contains: Some("Melbourne"),
        expect_some_road: true,
        hint_road_contains: Some("flinders"),
    },
    Fixture {
        name: "brisbane_queen_st",
        lat: -27.4701,
        lng: 153.0260,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("Queensland"),
        expect_city_contains: Some("Brisbane"),
        expect_some_road: true,
        hint_road_contains: Some("queen"),
    },
    Fixture {
        name: "perth_cbd",
        lat: -31.9540,
        lng: 115.8576,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("Western Australia"),
        expect_city_contains: Some("Perth"),
        expect_some_road: true,
        hint_road_contains: None,
    },
    Fixture {
        name: "adelaide_rundle_mall",
        lat: -34.9232,
        lng: 138.6004,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("South Australia"),
        expect_city_contains: Some("Adelaide"),
        expect_some_road: true,
        hint_road_contains: Some("rundle"),
    },
    // ACT is a unitary territory; no level-6 LGA. Level-10 suburb ("Capital
    // Hill" / "Parkes") may be the effective locality. Don't pin city to a
    // specific value but do require country + state.
    Fixture {
        name: "canberra_parliament",
        lat: -35.3080,
        lng: 149.1245,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("Australian Capital Territory"),
        expect_city_contains: None,
        expect_some_road: false,
        hint_road_contains: None,
    },
    Fixture {
        name: "hobart_cbd",
        lat: -42.8826,
        lng: 147.3257,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("Tasmania"),
        expect_city_contains: Some("Hobart"),
        expect_some_road: true,
        hint_road_contains: None,
    },
    Fixture {
        name: "darwin_cbd",
        lat: -12.4634,
        lng: 130.8456,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("Northern Territory"),
        expect_city_contains: Some("Darwin"),
        expect_some_road: true,
        hint_road_contains: None,
    },
    // Remote AU — should exercise the place=* fallback. These are small
    // towns where admin_level=9 polygons may be absent but `place=town`
    // nodes exist in OSM.
    Fixture {
        name: "coober_pedy_sa",
        lat: -29.0135,
        lng: 134.7546,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("South Australia"),
        expect_city_contains: Some("Coober Pedy"),
        expect_some_road: false,
        hint_road_contains: None,
    },
    Fixture {
        name: "alice_springs_nt",
        lat: -23.6980,
        lng: 133.8807,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("Northern Territory"),
        expect_city_contains: Some("Alice Springs"),
        expect_some_road: false,
        hint_road_contains: None,
    },
    // Alysse Close, Baulkham Hills — end-to-end verification of the
    // user-reported query. Postcode comes from the G-NAF lookup (2153,
    // not 2154 as commonly assumed — 2154 is neighbouring Castle Hill).
    Fixture {
        name: "alysse_close_baulkham_hills",
        lat: -33.7369,
        lng: 150.9803,
        expect_country: Some("Australia"),
        expect_country_code: Some("AU"),
        expect_state: Some("New South Wales"),
        expect_city_contains: Some("Baulkham Hills"),
        expect_some_road: true,
        hint_road_contains: Some("alysse"),
    },
    // Offshore — well out in the Tasman Sea.
    Fixture {
        name: "tasman_sea_offshore",
        lat: -36.0,
        lng: 158.0,
        expect_country: None,
        expect_country_code: None,
        expect_state: None,
        expect_city_contains: None,
        expect_some_road: false,
        hint_road_contains: None,
    },
];

fn load_index() -> Option<Index> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    match Index::load(
        &dir,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    ) {
        Ok(idx) => Some(idx),
        Err(e) => {
            eprintln!("FAIL: failed to load index at {}: {}", dir, e);
            None
        }
    }
}

fn contains_ci(haystack: Option<&str>, needle: &str) -> bool {
    haystack
        .map(|h| h.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()))
        .unwrap_or(false)
}

#[test]
fn australia_accuracy() {
    let Some(index) = load_index() else {
        eprintln!("SKIP: GEOCODER_INDEX_DIR unset — cannot run accuracy test");
        return;
    };

    let mut failures: Vec<String> = Vec::new();
    let mut hint_misses = 0usize;

    for fx in FIXTURES {
        let addr = index.query(fx.lat, fx.lng);
        let a = &addr.address;
        eprintln!(
            "{:>24}  country={:?} state={:?} city={:?} county={:?} postcode={:?} road={:?} hn={:?}",
            fx.name, a.country, a.state, a.city, a.county, a.postcode, a.road, a.house_number,
        );

        let mut local = Vec::new();

        if let Some(expected) = fx.expect_country {
            if a.country.as_deref() != Some(expected) {
                local.push(format!("country: expected {expected:?}, got {:?}", a.country));
            }
        } else if a.country.is_some() {
            local.push(format!("country: expected None, got {:?}", a.country));
        }

        if let Some(expected) = fx.expect_country_code {
            if a.country_code.as_deref() != Some(expected) {
                local.push(format!(
                    "country_code: expected {expected:?}, got {:?}",
                    a.country_code
                ));
            }
        }

        if let Some(expected) = fx.expect_state {
            if a.state.as_deref() != Some(expected) {
                local.push(format!("state: expected {expected:?}, got {:?}", a.state));
            }
        }

        if fx.expect_some_road && a.road.is_none() {
            local.push("road: expected Some(_), got None".into());
        }

        if let Some(needle) = fx.expect_city_contains {
            if !contains_ci(a.city.as_deref(), needle) {
                local.push(format!(
                    "city: expected substring {needle:?}, got {:?}",
                    a.city
                ));
            }
        }

        if let Some(needle) = fx.hint_road_contains {
            if !contains_ci(a.road.as_deref(), needle) {
                hint_misses += 1;
                eprintln!(
                    "    HINT MISS: road {:?} does not contain {:?}",
                    a.road, needle
                );
            }
        }

        if !local.is_empty() {
            failures.push(format!("[{}]\n    {}", fx.name, local.join("\n    ")));
        }
    }

    eprintln!("\nHint misses: {}", hint_misses);

    assert!(
        failures.is_empty(),
        "accuracy regressions:\n{}",
        failures.join("\n")
    );
}
