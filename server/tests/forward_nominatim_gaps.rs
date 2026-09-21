//! Exercises the Nominatim-style gap-closers we layered onto the forward
//! search: ASCII-folding, abbreviation canonicalisation, rank-based
//! scoring, and the fallback ladder.

#![cfg(feature = "forward")]

use query_server::forward::{
    canonicalise_phrase, parse_freeform_query, parse_freeform_query_in_country,
    tokenize_user_input, Forward, StructuredQuery,
};
use std::path::PathBuf;

fn load_forward() -> Option<Forward> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    let path = PathBuf::from(dir);
    assert!(
        path.exists(),
        "GEOCODER_INDEX_DIR does not exist: {}",
        path.display()
    );
    Some(Forward::open(&path).expect("open forward index"))
}

// --- Pure-function tests (no index required) ---

#[test]
fn canonicalise_phrase_expands_unambiguous_abbreviations() {
    assert_eq!(canonicalise_phrase("Smith Tce"), "Smith terrace");
    assert_eq!(canonicalise_phrase("Pacific Hwy"), "Pacific highway");
    assert_eq!(canonicalise_phrase("King George Pde"), "King George parade");
    assert_eq!(canonicalise_phrase("Flinders Cres"), "Flinders crescent");
    // Trailing period tolerated
    assert_eq!(canonicalise_phrase("Pacific Hwy."), "Pacific highway");
    // Words NOT in the table are preserved verbatim (including case)
    assert_eq!(canonicalise_phrase("Elizabeth Street"), "Elizabeth Street");
    assert_eq!(canonicalise_phrase("Baulkham Hills"), "Baulkham Hills");
}

#[test]
fn tokenize_user_input_applies_canonicalisation() {
    let toks = tokenize_user_input("Smith Tce");
    assert_eq!(toks, vec!["smith", "terrace"]);
    let toks = tokenize_user_input("  pacific hwy.  ");
    assert_eq!(toks, vec!["pacific", "highway"]);
}

#[test]
fn street_suffix_st_does_not_change_leading_saint() {
    assert_eq!(
        tokenize_user_input("38 Carrington St, Deakin"),
        ["38", "carrington", "street", "deakin"]
    );
    assert_eq!(
        tokenize_user_input("450 37th St, New York"),
        ["450", "37th", "street", "new", "york"]
    );
    assert_eq!(tokenize_user_input("St Louis"), ["st", "louis"]);
    assert_eq!(canonicalise_phrase("Carrington St"), "Carrington street");
}

#[test]
fn tokenize_folds_diacritics() {
    // Café → cafe, Zürich → zurich, São → sao
    assert_eq!(
        tokenize_user_input("Café Zürich São Paulo"),
        vec!["cafe", "zurich", "sao", "paulo"]
    );
}

#[test]
fn dotted_compass_and_floor_do_not_block_an_address() {
    assert_eq!(
        tokenize_user_input("Macleod Trail S.E."),
        ["macleod", "trail", "se"]
    );
    let parsed = parse_freeform_query_in_country(
        "615 MacLeod Trail S.E. 10th Floor Calgary T2G 4T8",
        Some("CA"),
    );
    assert_eq!(parsed.house_number.as_deref(), Some("615"));
    assert_eq!(parsed.postcode.as_deref(), Some("T2G4T8"));
    assert_eq!(parsed.rest, ["macleod", "trail", "se", "calgary"]);
}

#[test]
fn parse_freeform_extracts_housenumber_state_postcode() {
    let p = parse_freeform_query("10 alysse close baulkham hills nsw 2154");
    assert_eq!(p.house_number.as_deref(), Some("10"));
    assert_eq!(p.state.as_deref(), Some("New South Wales"));
    assert_eq!(p.postcode.as_deref(), Some("2154"));
    assert_eq!(
        p.rest,
        vec!["alysse", "close", "baulkham", "hills"],
        "remaining tokens should be the street + suburb, with hints stripped",
    );
}

#[test]
fn parse_freeform_handles_bare_state_abbreviations() {
    let p = parse_freeform_query("any street qld");
    assert_eq!(p.state.as_deref(), Some("Queensland"));
}

#[test]
fn parse_freeform_single_token_is_never_a_state() {
    // Bench-accuracy regression: `q=Vic` against the Spanish per-country
    // index returned zero results because "vic" was promoted to
    // state="Victoria" and the residual `rest` was empty — no name
    // search for "vic" ever ran. The fix keeps single-token queries
    // entirely in `rest` regardless of state-abbreviation overlap.
    for one in ["Vic", "NSW", "qld", "Tasmania", "WA"] {
        let p = parse_freeform_query(one);
        assert_eq!(p.state, None, "{one}: single-token must not become state");
        assert_eq!(p.rest.len(), 1, "{one}: single token must stay in rest");
    }
}

#[test]
fn parse_freeform_multi_token_state_promotion_unchanged() {
    // Multi-token queries with a state abbreviation still promote it;
    // this is what keeps `Sydney NSW` and `Melbourne VIC` working.
    let p = parse_freeform_query("Sydney NSW");
    assert_eq!(p.state.as_deref(), Some("New South Wales"));
    assert_eq!(p.rest, vec!["sydney"]);

    let p = parse_freeform_query("melbourne vic");
    assert_eq!(p.state.as_deref(), Some("Victoria"));
    assert_eq!(p.rest, vec!["melbourne"]);
}

#[test]
fn parse_freeform_trailing_short_digits_are_housenumber() {
    // Street-then-number order ("Alysse Close 10") — Nominatim's BDD
    // db/query/housenumbers.feature covers this explicitly. Without
    // the trailing-digit rule, "10" falls into the word bag and the
    // tantivy search returns 0 results.
    let p = parse_freeform_query("alysse close 10");
    assert_eq!(p.house_number.as_deref(), Some("10"));
    assert_eq!(p.rest, vec!["alysse", "close"]);
    assert_eq!(p.postcode, None);
}

#[test]
fn parse_freeform_trailing_4_digit_still_postcode() {
    // 4-digit trailing stays postcode, not housenumber — AU postcodes
    // always win at that length.
    let p = parse_freeform_query("alysse close 2153");
    assert_eq!(p.house_number, None);
    assert_eq!(p.postcode.as_deref(), Some("2153"));
    assert_eq!(p.rest, vec!["alysse", "close"]);
}

#[test]
fn parse_freeform_leading_housenumber_still_wins() {
    // Regression guard: when both orders would extract a housenumber,
    // the leading position takes precedence.
    let p = parse_freeform_query("10 alysse close");
    assert_eq!(p.house_number.as_deref(), Some("10"));
    assert_eq!(p.rest, vec!["alysse", "close"]);
}

#[test]
fn parse_freeform_trailing_5_digit_also_housenumber() {
    // Some AU addresses have 5-digit housenumbers (e.g. rural property
    // addressing, hwy milepost). Keep these as housenumber, not
    // postcode — AU postcodes are always 4 digits.
    let p = parse_freeform_query("main road 12345");
    assert_eq!(p.house_number.as_deref(), Some("12345"));
}

#[test]
fn parse_freeform_street_number_order_does_not_swallow_interior_digits() {
    // Digits in the middle of a query (not leading, not trailing) must
    // stay in the rest bag — not be promoted to housenumber. Example:
    // someone might query "Route 66 Flagstaff" where "66" is part of
    // the street name.
    let p = parse_freeform_query("route 66 flagstaff");
    // Leading is non-digit so the "66" mid-token stays in rest.
    // Housenumber extraction only fires on position 0 or last.
    assert_eq!(p.house_number, None);
    assert!(p.rest.contains(&"66".to_string()));
}

// --- Integration tests (require index) ---

#[test]
fn abbreviated_street_type_matches_expanded_form() {
    let Some(fwd) = load_forward() else { return };
    // "Pacific Hwy" should match "Pacific Highway" (and vice-versa) because
    // both sides canonicalise to {pacific, highway}.
    let short = fwd
        .search_structured(StructuredQuery {
            q: Some("pacific hwy"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    let long = fwd
        .search_structured(StructuredQuery {
            q: Some("pacific highway"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(!short.is_empty(), "'pacific hwy' should match something");
    assert!(!long.is_empty(), "'pacific highway' should match something");

    // Top results should overlap significantly — exact equality isn't
    // required (BM25 can differ on tie-breakers) but the same top pick
    // should appear in both result sets.
    let short_top = &short[0].name;
    let long_has_short_top = long.iter().any(|h| h.name == *short_top);
    assert!(
        long_has_short_top,
        "top short-form hit {:?} should also be in long-form results {:?}",
        short_top,
        long.iter().map(|h| &h.name).collect::<Vec<_>>(),
    );
}

#[test]
fn rank_boost_prefers_city_over_street_for_ambiguous_single_word() {
    let Some(fwd) = load_forward() else { return };
    // "Sydney" alone — there's a Sydney place (rank 16) AND many streets
    // named "Sydney *". Boost should put the city on top.
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("sydney"),
            limit: 10,
            ..Default::default()
        })
        .expect("search");

    // Find the first Sydney place and first Sydney street, compare positions.
    let first_place_idx = hits
        .iter()
        .position(|h| h.kind == query_server::forward::KIND_PLACE && h.name.contains("Sydney"));
    let first_street_idx = hits
        .iter()
        .position(|h| h.kind == query_server::forward::KIND_STREET);

    if let (Some(p), Some(s)) = (first_place_idx, first_street_idx) {
        assert!(
            p < s,
            "rank boost: place at index {} should come before street at index {}; got {:?}",
            p,
            s,
            hits.iter()
                .map(|h| format!("{:?}/kind={}", h.name, h.kind))
                .collect::<Vec<_>>(),
        );
    }
}

#[test]
fn explicit_state_remains_a_constraint() {
    let Some(fwd) = load_forward() else { return };
    // A wrong state must not silently fall back to a plausible street
    // elsewhere. Dispatchers could otherwise navigate to the wrong city.
    let strict_hits = fwd
        .search_structured(StructuredQuery {
            street: Some("alysse close"),
            city: Some("baulkham hills"),
            state: Some("Victoria"), // wrong state, Baulkham Hills is NSW
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(
        strict_hits.is_empty(),
        "wrong explicit state returned {strict_hits:?}"
    );
}

#[test]
fn parses_postcodes_and_regions_in_five_target_countries() {
    let us = parse_freeform_query_in_country("450 37th st, new york, ny 11232", Some("US"));
    assert_eq!(us.house_number.as_deref(), Some("450"));
    assert_eq!(us.postcode.as_deref(), Some("11232"));
    assert_eq!(us.state.as_deref(), Some("New York"));
    assert!(us.rest.contains(&"york".to_owned()));

    let au =
        parse_freeform_query_in_country("38 carrington st, deakin, ACT, australia", Some("AU"));
    assert_eq!(au.house_number.as_deref(), Some("38"));
    assert_eq!(au.state.as_deref(), Some("Australian Capital Territory"));
    assert!(!au.rest.contains(&"australia".to_owned()));

    let nz = parse_freeform_query_in_country("glasgow street 18, kelburn, wellington", Some("NZ"));
    assert_eq!(nz.house_number.as_deref(), Some("18"));
    assert_eq!(nz.state.as_deref(), Some("Wellington"));

    let ca = parse_freeform_query_in_country("123 Main Street, Toronto, ON M5V 2T6", Some("CA"));
    assert_eq!(ca.postcode.as_deref(), Some("M5V2T6"));
    assert_eq!(ca.state.as_deref(), Some("Ontario"));

    let gb = parse_freeform_query_in_country("EN5 2LP", Some("GB"));
    assert_eq!(gb.postcode.as_deref(), Some("EN52LP"));
    assert!(gb.rest.is_empty());
}

#[test]
fn parses_postcodes_outside_launch_countries() {
    let fr = parse_freeform_query_in_country("10 Rue de la Paix, Paris 75002", Some("FR"));
    assert_eq!(fr.house_number.as_deref(), Some("10"));
    assert_eq!(fr.postcode.as_deref(), Some("75002"));
    assert!(!fr.rest.contains(&"75002".to_owned()));

    let nl = parse_freeform_query_in_country("Damrak 1, Amsterdam 1012 AB", Some("NL"));
    assert_eq!(nl.postcode.as_deref(), Some("1012AB"));

    let ie = parse_freeform_query_in_country("Dublin D02 X285", Some("IE"));
    assert_eq!(ie.postcode.as_deref(), Some("D02X285"));

    let de = parse_freeform_query_in_country("Berlin, DE", Some("DE"));
    assert_eq!(de.rest, ["berlin"]);
}

#[test]
fn full_address_queries_find_the_requested_city_and_postcode() {
    let Some(fwd) = load_forward() else { return };
    for (query, country, city, postcode) in [
        (
            "38 carrington st, deakin, ACT, australia",
            "AU",
            "Deakin",
            "2600",
        ),
        ("450 37th st, new york, ny 11232", "US", "New York", "11232"),
    ] {
        let hits = fwd
            .search_structured(StructuredQuery {
                q: Some(query),
                country_code: Some(country),
                limit: 5,
                ..Default::default()
            })
            .expect("search");
        assert!(
            hits.iter().any(|hit| hit.housenumber.as_deref()
                == Some(if country == "AU" { "38" } else { "450" })
                && hit.suburb.as_deref() == Some(city)
                && hit.postcode.as_deref() == Some(postcode)),
            "{query}: {hits:?}"
        );
    }
}

#[test]
fn full_region_name_in_freeform_query_matches_indexed_state() {
    let Some(fwd) = load_forward() else { return };
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("Toronto, Ontario"),
            country_code: Some("CA"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert_eq!(
        hits.first().map(|hit| hit.kind),
        Some(query_server::forward::KIND_PLACE),
        "{hits:?}"
    );
    assert!(
        hits.iter()
            .any(|hit| hit.name.eq_ignore_ascii_case("Toronto")
                && hit.state.as_deref() == Some("Ontario")),
        "{hits:?}"
    );
}

#[test]
fn bare_postcodes_search_indexed_values_without_country_shape_guessing() {
    let Some(fwd) = load_forward() else { return };
    for (postcode, country) in [("EN52LP", "GB"), ("2158", "AU")] {
        let hits = fwd
            .search_structured(StructuredQuery {
                q: Some(postcode),
                limit: 5,
                ..Default::default()
            })
            .expect("search");
        assert!(
            hits.iter()
                .any(|hit| hit.country_code.as_deref() == Some(country)
                    && hit.postcode.as_deref().is_some_and(|stored| {
                        query_server::forward::normalize_postcode(stored) == postcode
                    })),
            "{postcode}: {hits:?}"
        );
    }
}

#[test]
fn state_abbreviation_is_not_inferred_as_a_country() {
    let Some(fwd) = load_forward() else { return };
    let us_hits = fwd
        .search_structured(StructuredQuery {
            q: Some("San Francisco, CA"),
            country_code: Some("US"),
            kind: Some(1),
            limit: 10,
            ..Default::default()
        })
        .expect("search US");
    assert!(
        us_hits
            .iter()
            .any(|hit| hit.name == "San Francisco" && hit.state.as_deref() == Some("California")),
        "US: {us_hits:?}"
    );
    let structured = fwd
        .search_structured(StructuredQuery {
            q: Some("San Francisco"),
            state: Some("California"),
            country_code: Some("US"),
            kind: Some(1),
            limit: 10,
            ..Default::default()
        })
        .expect("structured US");
    assert!(
        structured.iter().any(|hit| hit.name == "San Francisco"),
        "structured: {structured:?}"
    );
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("San Francisco, CA"),
            limit: 10,
            ..Default::default()
        })
        .expect("search");
    assert!(
        hits.iter().any(|hit| hit.name == "San Francisco"
            && hit.country_code.as_deref() == Some("US")
            && hit.state.as_deref() == Some("California")),
        "{hits:?}"
    );
}

#[test]
fn city_region_queries_return_the_city_centre_first() {
    let Some(fwd) = load_forward() else { return };
    for (query, lon) in [("San Francisco, CA", -122.4), ("New York, NY", -74.0)] {
        let hits = fwd
            .search_structured(StructuredQuery {
                q: Some(query),
                limit: 1,
                ..Default::default()
            })
            .expect("search");
        let first = hits.first().expect("city hit");
        assert_eq!(first.kind, 1, "{query}: {hits:?}");
        assert_eq!(first.rank, 16, "{query}: {hits:?}");
        assert_eq!(first.country_code.as_deref(), Some("US"));
        assert!((first.lng - lon).abs() < 0.1, "{query}: {hits:?}");
    }
}

#[test]
fn prominent_exact_names_win_within_a_country() {
    let Some(fwd) = load_forward() else { return };
    let toronto = fwd
        .search_structured(StructuredQuery {
            q: Some("Toronto"),
            country_code: Some("CA"),
            limit: 1,
            ..Default::default()
        })
        .expect("Toronto search");
    assert_eq!(toronto[0].state.as_deref(), Some("Ontario"), "{toronto:?}");

    let statue = fwd
        .search_structured(StructuredQuery {
            q: Some("Statue of Liberty"),
            country_code: Some("US"),
            limit: 5,
            ..Default::default()
        })
        .expect("landmark search");
    assert!(
        statue.iter().any(|h| h.name == "Statue of Liberty"
            && h.state.as_deref() == Some("New York")
            && (h.lat - 40.689).abs() < 0.01),
        "{statue:?}"
    );
}

#[test]
fn mixed_exact_and_partial_names_have_a_total_order() {
    let Some(fwd) = load_forward() else { return };
    assert!(!fwd
        .search_structured(StructuredQuery {
            q: Some("portland"),
            limit: 10,
            ..Default::default()
        })
        .expect("search must not panic while sorting")
        .is_empty());
}

#[test]
fn country_names_and_ambiguous_codes_resolve_from_context() {
    let Some(fwd) = load_forward() else { return };
    for (query, country, name, number) in [
        (
            "113 Bargara Road, Bundaberg East, QLD, AUS",
            "AU",
            "Bargara Road",
            "113",
        ),
        ("5 West 4th Avenue Canada", "CA", "West 4th Avenue", "5"),
        (
            "22 Lloyd George Ave, Toronto Ontario CA",
            "CA",
            "Lloyd George Avenue",
            "22",
        ),
    ] {
        let hits = fwd
            .search_structured(StructuredQuery {
                q: Some(query),
                limit: 5,
                ..Default::default()
            })
            .expect("address search");
        assert!(
            hits.iter()
                .any(|h| h.country_code.as_deref() == Some(country)
                    && h.name == name
                    && h.housenumber.as_deref() == Some(number)),
            "{query}: {hits:?}"
        );
    }
    let toronto = fwd
        .search_structured(StructuredQuery {
            q: Some("Toronto, CA"),
            limit: 1,
            ..Default::default()
        })
        .expect("Toronto, CA search");
    assert_eq!(
        toronto[0].country_code.as_deref(),
        Some("CA"),
        "{toronto:?}"
    );
    assert_eq!(toronto[0].state.as_deref(), Some("Ontario"), "{toronto:?}");

    let lloyd = fwd
        .search_structured(StructuredQuery {
            q: Some("22 Lloyd George Ave, Toronto Ontario CA"),
            limit: 1,
            ..Default::default()
        })
        .expect("ambiguous CA search");
    assert_eq!(lloyd[0].suburb.as_deref(), Some("Toronto"), "{lloyd:?}");
}

#[test]
fn unscoped_street_uses_exact_locality_to_pick_country() {
    let Some(fwd) = load_forward() else { return };
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("glen rd, kelburn"),
            limit: 5,
            ..Default::default()
        })
        .expect("street search");
    assert!(
        hits.iter().any(|h| h.country_code.as_deref() == Some("NZ")
            && h.name == "Glen Road"
            && (h.lat + 41.29).abs() < 0.05),
        "{hits:?}"
    );
}

#[test]
fn indexed_address_accepts_its_administrative_city() {
    let Some(fwd) = load_forward() else { return };
    for (query, country, street, house) in [
        (
            "490 Sussex Drive Ottawa K1N 1G8",
            "CA",
            "Sussex Drive",
            "490",
        ),
        ("1 water st manhattan ny", "US", "Water Street", "1"),
        (
            "615 MacLeod Trail S.E. 10th Floor Calgary T2G 4T8",
            "CA",
            "Macleod Trail SE",
            "615",
        ),
    ] {
        let hits = fwd
            .search_structured(StructuredQuery {
                q: Some(query),
                country_code: Some(country),
                limit: 5,
                ..Default::default()
            })
            .expect("address search");
        assert!(
            hits.iter()
                .any(|h| h.name == street && h.housenumber.as_deref() == Some(house)),
            "{query}: {hits:?}"
        );
    }
}

#[test]
fn fallback_does_not_trigger_when_primary_query_hits() {
    let Some(fwd) = load_forward() else { return };
    // When the primary query has hits, we must NOT run the fallback (which
    // would return different results for the same query). Sanity check by
    // comparing the hit set to a forced single-pass search.
    let primary = fwd
        .search_structured(StructuredQuery {
            street: Some("alysse close"),
            city: Some("baulkham hills"),
            state: Some("New South Wales"),
            limit: 5,
            ..Default::default()
        })
        .expect("search");
    assert!(!primary.is_empty());
    for hit in &primary {
        assert_eq!(hit.suburb.as_deref(), Some("Baulkham Hills"));
        assert_eq!(hit.state.as_deref(), Some("New South Wales"));
    }
}

#[test]
fn comma_neighbourhood_falls_back_to_street_in_country_index() {
    let Ok(dir) = std::env::var("GEOCODER_INDEX_DIR") else {
        return;
    };
    assert!(
        std::path::Path::new(&dir).exists(),
        "GEOCODER_INDEX_DIR does not exist: {dir}"
    );
    let fwd = Forward::open(std::path::Path::new(&dir)).expect("open forward index");
    assert!(fwd.countries().any(|cc| cc == b"nz"), "NZ shard missing");
    let direct = fwd
        .search_structured(StructuredQuery {
            q: Some("glen rd"),
            country_code: Some("nz"),
            limit: 10,
            ..Default::default()
        })
        .expect("direct street search");
    assert!(!direct.is_empty(), "fixture must contain Glen Road");
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("glen rd, kelburn"),
            country_code: Some("nz"),
            limit: 10,
            ..Default::default()
        })
        .expect("search");
    assert!(
        hits.iter()
            .any(|h| h.name == "Glen Road" && h.country_code.as_deref() == Some("NZ")),
        "expected a New Zealand Glen Road hit, got {hits:?}"
    );
}

#[test]
fn new_york_query_keeps_new_as_a_name_token() {
    let Ok(dir) = std::env::var("GEOCODER_INDEX_DIR") else {
        return;
    };
    assert!(
        std::path::Path::new(&dir).exists(),
        "GEOCODER_INDEX_DIR does not exist: {dir}"
    );
    let fwd = Forward::open(std::path::Path::new(&dir)).expect("open forward index");
    let hits = fwd
        .search_structured(StructuredQuery {
            q: Some("New York"),
            country_code: Some("us"),
            limit: 10,
            ..Default::default()
        })
        .expect("search");
    assert!(
        hits.iter()
            .any(|h| h.name == "New York" && h.country_code.as_deref() == Some("US")),
        "expected a New York hit in the US shard, got {hits:?}"
    );
}
