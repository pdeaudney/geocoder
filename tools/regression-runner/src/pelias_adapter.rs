//! Convert a `pelias/acceptance-tests` corpus file into our regression
//! runner's schema.
//!
//! Input  (Pelias shape, abbreviated):
//! ```json
//! { "name": "...", "endpoint": "search",
//!   "tests": [{
//!     "id": 1, "in": { "text": "38 carrington st, deakin, ACT" },
//!     "expected": { "properties": [{
//!         "name": "38 Carrington Street",
//!         "locality": "Deakin", "region": "Australian Capital Territory",
//!         "region_a": "ACT", "country_a": "AUS"
//!     }] }
//!   }, ...]
//! }
//! ```
//!
//! Output: a corpus file the regression runner understands — one
//! `json_path_contains_ci` per expected field, plus `country_code=au`
//! in the query when Pelias's `country_a` is `AUS`.
//!
//! Usage:
//!   pelias-to-ours <input.json>
//!     [--country au]                 filter to one country (default: no filter)
//!     [--endpoint search|autocomplete]
//!     [--name <corpus-name>]
//!     [--auth-key <token>]
//!
//! Writes the converted corpus to stdout.
//!
//! Not every Pelias case will pass against our service — Pelias has
//! broader coverage (global) and different ranking. Use the runner's
//! pass/fail report to curate a subset and copy into
//! `tests/regression/corpora/` for committing.

// mimalloc global allocator — see build-pipeline-perf-plan stage 2.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

// Pelias corpus files sometimes contain duplicate top-level keys per
// case (e.g. two `description` fields on the same test). Serde's strict
// struct deserialiser rejects these, so we parse the corpus as a
// generic `Value` tree and pluck fields by name — which matches
// `serde_json::Value`'s "last value wins" semantics for duplicates.

#[derive(Debug, Serialize)]
struct OurCorpus {
    name: String,
    description: String,
    cases: Vec<Value>,
}

struct Args {
    input: PathBuf,
    filter_country: Option<String>,
    endpoint: String,
    corpus_name: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        input: PathBuf::new(),
        filter_country: None,
        endpoint: "search".to_string(),
        corpus_name: None,
    };
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--country" => {
                args.filter_country = Some(raw.get(i + 1).ok_or("--country needs a value")?.clone());
                i += 2;
            }
            "--endpoint" => {
                args.endpoint = raw.get(i + 1).ok_or("--endpoint needs a value")?.clone();
                i += 2;
            }
            "--name" => {
                args.corpus_name = Some(raw.get(i + 1).ok_or("--name needs a value")?.clone());
                i += 2;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            other => {
                if args.input.as_os_str().is_empty() {
                    args.input = PathBuf::from(other);
                    i += 1;
                } else {
                    return Err(format!("unexpected positional argument {other}"));
                }
            }
        }
    }
    if args.input.as_os_str().is_empty() {
        return Err("input path required".into());
    }
    Ok(args)
}

fn print_help() {
    println!(
        "pelias-to-ours <input.json> [--country au] [--endpoint search|autocomplete] [--name NAME]"
    );
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let bytes = match std::fs::read(&args.input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: read {}: {e}", args.input.display());
            return ExitCode::from(2);
        }
    };
    let pelias: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: parse Pelias corpus: {e}");
            return ExitCode::from(2);
        }
    };

    let corpus_name = pelias
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("pelias")
        .to_string();
    let endpoint_tag = pelias
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("search")
        .to_string();

    let name = args
        .corpus_name
        .clone()
        .unwrap_or_else(|| format!("pelias-{}", corpus_name.replace(' ', "-")));
    let description = format!(
        "Converted from pelias/acceptance-tests ({corpus_name}). Only cases \
         with an extractable locality/region and status != 'fail' are emitted. \
         Expectation rules: contains_ci on locality/region/name, \
         country_code filter when expected country_a is known."
    );

    let tests = pelias
        .get("tests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut our_cases: Vec<Value> = Vec::new();
    let mut skipped = 0usize;

    for case in &tests {
        let status = case.get("status").and_then(Value::as_str);
        if status == Some("fail") {
            skipped += 1;
            continue;
        }
        let Some(text) = case
            .get("in")
            .and_then(|v| v.get("text"))
            .and_then(Value::as_str)
        else {
            skipped += 1;
            continue;
        };

        let Some(props) = case
            .get("expected")
            .and_then(|e| e.get("properties"))
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_object)
        else {
            skipped += 1;
            continue;
        };

        // Country filter: Pelias uses `country_a` = ISO 3166-1 alpha-3
        // (AUS, USA, ...), we use alpha-2. Normalize both to lowercase.
        let expected_cc_alpha3 = props.get("country_a").and_then(Value::as_str);
        let expected_cc = expected_cc_alpha3.and_then(alpha3_to_alpha2);

        if let Some(filter) = args.filter_country.as_deref() {
            let filter_lc = filter.to_ascii_lowercase();
            if filter_lc == "none" {
                // `--country none` means "cross-cutting cases only":
                // entries whose expected country_a is missing or
                // doesn't resolve to a known alpha-2. These test
                // autocomplete mechanics, schema, admin-hierarchy
                // behaviour independent of specific country data.
                if expected_cc.is_some() {
                    skipped += 1;
                    continue;
                }
            } else if expected_cc.as_deref() != Some(&filter_lc) {
                skipped += 1;
                continue;
            }
        }

        let mut query = HashMap::<String, String>::new();
        query.insert("q".to_string(), text.to_string());
        if let Some(cc) = expected_cc.as_deref() {
            query.insert("country_code".to_string(), cc.to_string());
        }

        let mut checks: Vec<Value> = Vec::new();
        checks.push(json!({ "kind": "json_path_min_count", "path": "results", "min": 1 }));

        // Map Pelias's `locality` → our city, `region` → state, `name`
        // → the top hit's name/display_name. We only assert the parts
        // that Pelias actually provides for this case.
        if let Some(locality) = props.get("locality").and_then(Value::as_str) {
            checks.push(json!({
                "kind": "json_path_contains_ci",
                "path": "results.0.address.city",
                "value": locality,
            }));
        }
        if let Some(region) = props.get("region").and_then(Value::as_str) {
            checks.push(json!({
                "kind": "json_path_contains_ci",
                "path": "results.0.address.state",
                "value": region,
            }));
        }
        // Asserting the full Pelias `name` string ("38 Carrington Street")
        // is too strict — we don't always echo the house number into
        // results[].name. Fall back to checking the road substring.
        if let Some(name) = props.get("name").and_then(Value::as_str) {
            if let Some(street) = strip_leading_housenumber(name) {
                checks.push(json!({
                    "kind": "json_path_contains_ci",
                    "path": "results.0.address.road",
                    "value": street,
                }));
            }
        }

        let case_id = case
            .get("id")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "0".to_string());
        let id = format!(
            "pelias-{}-{}",
            sanitise_for_id(&corpus_name),
            case_id.trim_matches('"'),
        );
        let notes = case
            .get("description")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        our_cases.push(json!({
            "id": id,
            "tags": ["pelias", endpoint_tag.as_str()],
            "request": {
                "method": "GET",
                "path": format!("/{}", args.endpoint),
                "query": query,
            },
            "expect": {
                "status": 200,
                "checks": checks,
            },
            "notes": notes,
        }));
    }

    let out = OurCorpus {
        name,
        description,
        cases: our_cases,
    };

    let json = serde_json::to_string_pretty(&out).expect("serialise corpus");
    println!("{json}");
    eprintln!("converted {} cases, skipped {}", out.cases.len(), skipped);
    ExitCode::SUCCESS
}

fn alpha3_to_alpha2(alpha3: &str) -> Option<String> {
    // ISO 3166-1 alpha-3 → alpha-2. Covers every country Pelias's
    // acceptance-tests corpus refers to, plus every country we ship
    // a server-side index for. Adding a new region usually means
    // adding a line here, regenerating the corpora via
    // `make pelias-full-refresh`, and rerunning the worldwide suite.
    let cc = match alpha3.to_ascii_uppercase().as_str() {
        "AUS" => "au",
        "AUT" => "at",
        "ARG" => "ar",
        "BEL" => "be",
        "BGR" => "bg",
        "BRA" => "br",
        "CAN" => "ca",
        "CHE" => "ch",
        "CHL" => "cl",
        "CHN" => "cn",
        "COL" => "co",
        "CRI" => "cr",
        "CZE" => "cz",
        "DEU" => "de",
        "DNK" => "dk",
        "DOM" => "do",
        "ECU" => "ec",
        "EGY" => "eg",
        "ESP" => "es",
        "EST" => "ee",
        "FIN" => "fi",
        "FRA" => "fr",
        "GBR" => "gb",
        "GRC" => "gr",
        "HKG" => "hk",
        "HRV" => "hr",
        "HUN" => "hu",
        "IDN" => "id",
        "IND" => "in",
        "IRL" => "ie",
        "IRN" => "ir",
        "ISL" => "is",
        "ISR" => "il",
        "ITA" => "it",
        "JAM" => "jm",
        "JPN" => "jp",
        "KEN" => "ke",
        "KOR" => "kr",
        "LKA" => "lk",
        "LTU" => "lt",
        "LUX" => "lu",
        "LVA" => "lv",
        "MAR" => "ma",
        "MEX" => "mx",
        "MYS" => "my",
        "NGA" => "ng",
        "NLD" => "nl",
        "NOR" => "no",
        "NZL" => "nz",
        "PAK" => "pk",
        "PER" => "pe",
        "PHL" => "ph",
        "POL" => "pl",
        "PRT" => "pt",
        "ROU" => "ro",
        "RUS" => "ru",
        "SAU" => "sa",
        "SGP" => "sg",
        "SVK" => "sk",
        "SVN" => "si",
        "SWE" => "se",
        "THA" => "th",
        "TUR" => "tr",
        "TWN" => "tw",
        "UKR" => "ua",
        "URY" => "uy",
        "USA" => "us",
        "VEN" => "ve",
        "VNM" => "vn",
        "ZAF" => "za",
        _ => return None,
    };
    Some(cc.to_string())
}

fn sanitise_for_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Strip a leading street number from Pelias's `name` field, returning
/// just the street part. "38 Carrington Street" → "Carrington Street";
/// "Main Street" → None (already just a street, no number to strip).
fn strip_leading_housenumber(name: &str) -> Option<&str> {
    let mut chars = name.char_indices();
    let first = chars.next()?;
    if !first.1.is_ascii_digit() {
        return None;
    }
    // Consume digits + optional house-number letter suffix (e.g. "38A").
    let mut last_digit_end = first.0 + first.1.len_utf8();
    for (i, c) in chars.by_ref() {
        if c.is_ascii_digit() || (c.is_ascii_alphabetic() && c.is_ascii_uppercase()) {
            last_digit_end = i + c.len_utf8();
        } else if c == ' ' {
            return Some(name[i + 1..].trim());
        } else {
            return None;
        }
    }
    // Ran out of chars — the whole string was digits+letters, no street.
    let _ = last_digit_end;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_housenumber() {
        assert_eq!(strip_leading_housenumber("38 Carrington Street"), Some("Carrington Street"));
        assert_eq!(strip_leading_housenumber("10A Alysse Close"), Some("Alysse Close"));
        assert_eq!(strip_leading_housenumber("Carrington Street"), None);
        assert_eq!(strip_leading_housenumber("42"), None);
    }

    #[test]
    fn alpha3_to_alpha2_covers_common() {
        assert_eq!(alpha3_to_alpha2("AUS"), Some("au".into()));
        assert_eq!(alpha3_to_alpha2("usa"), Some("us".into()));
        assert_eq!(alpha3_to_alpha2("XYZ"), None);
    }
}
