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
//! Output: a corpus file the regression runner understands. Comparable
//! cases retain Pelias's endpoint, supported input filters, and top-N
//! expectations. Unsupported requests are skipped; unsupported response
//! fields produce separately counted partial diagnostics.
//!
//! Usage:
//!   pelias-to-ours <input.json>
//!     [--country au]                 filter to one country (default: no filter)
//!     [--name <corpus-name>]
//!
//! Writes the converted corpus to stdout.
//!
//! Country filtering groups cases by expected country; it never adds a
//! country constraint to the request that Pelias did not send.

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
    corpus_name: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        input: PathBuf::new(),
        filter_country: None,
        corpus_name: None,
    };
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--country" => {
                args.filter_country =
                    Some(raw.get(i + 1).ok_or("--country needs a value")?.clone());
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
    println!("pelias-to-ours <input.json> [--country au] [--name NAME]");
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
    let name = args
        .corpus_name
        .clone()
        .unwrap_or_else(|| format!("pelias-{}", corpus_name.replace(' ', "-")));
    let description = format!(
        "Converted from pelias/acceptance-tests ({corpus_name}). Comparable cases retain supported request semantics and Pelias top-N expectations; partial and unsupported cases are counted separately."
    );

    let tests = pelias
        .get("tests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut our_cases: Vec<Value> = Vec::new();
    let mut skipped = 0usize;
    let mut unsupported = 0usize;
    let mut partial = 0usize;

    for case in &tests {
        match convert_case(case, &pelias, &args) {
            Some(converted) => {
                if converted.get("unsupported_reason").is_some() {
                    unsupported += 1;
                } else if converted.get("partial_reason").is_some() {
                    partial += 1;
                }
                our_cases.push(converted);
            }
            None => skipped += 1,
        }
    }

    let out = OurCorpus {
        name,
        description,
        cases: our_cases,
    };

    let json = serde_json::to_string_pretty(&out).expect("serialise corpus");
    println!("{json}");
    eprintln!(
        "converted {} cases ({} partial, {} unsupported), skipped {}",
        out.cases.len(),
        partial,
        unsupported,
        skipped
    );
    ExitCode::SUCCESS
}

fn convert_case(case: &Value, suite: &Value, args: &Args) -> Option<Value> {
    let expected = case.get("expected")?;
    let properties = expected.get("properties")?.as_array()?;
    let expected_cc = properties
        .first()
        .and_then(|p| p.get("country_a"))
        .and_then(Value::as_str)
        .and_then(alpha3_to_alpha2);
    if let Some(filter) = args.filter_country.as_deref() {
        if (filter == "none" && expected_cc.is_some())
            || (filter != "none" && expected_cc.as_deref() != Some(filter))
        {
            return None;
        }
    }

    let endpoint = case
        .get("endpoint")
        .and_then(Value::as_str)
        .or_else(|| suite.get("endpoint").and_then(Value::as_str))
        .unwrap_or("search");
    let mut unsupported = Vec::<String>::new();
    let mut partial = Vec::<String>::new();
    if case.get("status").and_then(Value::as_str) == Some("fail") {
        unsupported.push("upstream status fail".into());
    }
    let path = match endpoint {
        "search" => "/search".to_string(),
        "autocomplete" => "/autocomplete".to_string(),
        other => {
            unsupported.push(format!("endpoint {other}"));
            format!("/{other}")
        }
    };
    let priority = expected
        .get("priorityThresh")
        .and_then(Value::as_u64)
        .or_else(|| case.get("priorityThresh").and_then(Value::as_u64))
        .or_else(|| suite.get("priorityThresh").and_then(Value::as_u64))
        .unwrap_or(1);
    if priority == 0 || priority > 50 {
        unsupported.push(format!("priorityThresh {priority}"));
    }

    let mut query = HashMap::<String, String>::new();
    if let Some(input) = case.get("in").and_then(Value::as_object) {
        for (key, value) in input {
            match key.as_str() {
                "text" => match value.as_str() {
                    Some(text) => {
                        query.insert("q".into(), text.into());
                    }
                    None => unsupported.push("non-string text".into()),
                },
                "boundary.country" => match value.as_str().and_then(alpha3_to_alpha2) {
                    Some(cc) => {
                        query.insert("country_code".into(), cc);
                    }
                    None => unsupported.push("boundary.country value".into()),
                },
                "focus.point.lat" | "focus.point.lon" if endpoint == "search" => {
                    let target = if key.ends_with("lat") {
                        "bias_lat"
                    } else {
                        "bias_lng"
                    };
                    if value.is_number() || value.is_string() {
                        query.insert(
                            target.into(),
                            value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string()),
                        );
                    } else {
                        unsupported.push(format!("{key} value"));
                    }
                }
                "size" => match value.as_u64() {
                    Some(n @ 1..=50) => {
                        query.insert("limit".into(), n.to_string());
                    }
                    _ => unsupported.push("size value".into()),
                },
                other => unsupported.push(format!("request parameter {other}")),
            }
        }
    } else {
        unsupported.push("missing input object".into());
    }
    if query.contains_key("bias_lat") != query.contains_key("bias_lng") {
        unsupported.push("incomplete focus.point".into());
    }
    if !query.contains_key("q") {
        unsupported.push("missing text".into());
    }

    for (name, value) in [
        (
            "normalizers",
            case.get("normalizers").or_else(|| suite.get("normalizers")),
        ),
        (
            "weights",
            case.get("weights").or_else(|| suite.get("weights")),
        ),
    ] {
        if value.is_some_and(|v| !v.is_null() && v.as_object().is_none_or(|o| !o.is_empty())) {
            partial.push(name.into());
        }
    }

    let mut checks = Vec::<Value>::new();
    if properties.is_empty() {
        unsupported.push("no expected properties".into());
    }
    for property in properties {
        let Some(fields) = property.as_object() else {
            partial.push("non-object expected property".into());
            continue;
        };
        let (mapped, _) = map_fields(endpoint, fields, &mut partial);
        if !mapped.is_empty() {
            checks.push(
                json!({"kind": "result_matches_within", "limit": priority, "fields": mapped}),
            );
        }
    }
    if let Some(coordinates) = expected.get("coordinates") {
        if let Some(values) = coordinates.as_array() {
            let points: Vec<&Value> = if values.first().is_some_and(Value::is_number) {
                vec![coordinates]
            } else {
                values.iter().collect()
            };
            let max_m = expected
                .get("distanceThresh")
                .or_else(|| case.get("distanceThresh"))
                .or_else(|| suite.get("distanceThresh"))
                .and_then(Value::as_f64)
                .unwrap_or(500.0);
            for point in points {
                match point.as_array() {
                    Some(pair) if pair.len() == 2 => match (pair[0].as_f64(), pair[1].as_f64()) {
                        (Some(lng), Some(lat)) => checks.push(json!({
                            "kind": "result_coord_within", "limit": priority,
                            "expected_lat": lat, "expected_lng": lng, "max_m": max_m,
                        })),
                        _ => partial.push("coordinates value".into()),
                    },
                    _ => partial.push("coordinates value".into()),
                }
            }
        } else {
            partial.push("coordinates value".into());
        }
    }
    if let Some(size) = case.get("size").or_else(|| expected.get("size")) {
        if let Some(expression) = size
            .as_str()
            .map(str::to_owned)
            .or_else(|| size.as_u64().map(|n| n.to_string()))
        {
            checks.push(json!({"kind": "result_count", "expression": expression}));
        } else {
            partial.push("size expectation".into());
        }
    }
    if let Some(unexpected) = case.get("unexpected") {
        if let Some(values) = unexpected.get("properties").and_then(Value::as_array) {
            for property in values {
                if let Some(fields) = property.as_object() {
                    let (mapped, complete) = map_fields(endpoint, fields, &mut partial);
                    // Dropping half of a negative condition would reject
                    // unrelated hits, so only check fully mapped objects.
                    if complete && !mapped.is_empty() {
                        checks.push(json!({"kind": "result_absent", "fields": mapped}));
                    }
                } else {
                    partial.push("unexpected property value".into());
                }
            }
        } else {
            partial.push("unexpected value".into());
        }
    }
    if checks.is_empty() {
        unsupported.push("no comparable expected fields".into());
    }

    let case_id = case
        .get("id")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "0".into());
    let corpus_name = suite
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("pelias");
    let id = format!(
        "pelias-{}-{}",
        sanitise_for_id(corpus_name),
        case_id.trim_matches('"')
    );
    let mut converted = json!({
        "id": id,
        "tags": ["pelias", endpoint],
        "request": {"method": "GET", "path": path, "query": query},
        "expect": {"status": 200, "checks": checks},
        "notes": case.get("description"),
    });
    if !unsupported.is_empty() {
        unsupported.sort();
        unsupported.dedup();
        converted["unsupported_reason"] = json!(unsupported.join(", "));
    } else if !partial.is_empty() {
        partial.sort();
        partial.dedup();
        converted["partial_reason"] = json!(partial.join(", "));
    }
    Some(converted)
}

fn map_fields(
    endpoint: &str,
    fields: &serde_json::Map<String, Value>,
    partial: &mut Vec<String>,
) -> (HashMap<String, String>, bool) {
    let address_name = fields.contains_key("street") && fields.contains_key("housenumber");
    let mut mapped = HashMap::<String, String>::new();
    let mut complete = true;
    for (name, value) in fields {
        let Some(value) = value.as_str() else {
            partial.push(format!("expected field {name} value"));
            complete = false;
            continue;
        };
        let path = match (endpoint, name.as_str()) {
            (_, "name") if address_name => Some("name_or_address"),
            (_, "name") => Some("name"),
            ("search", "country_a") => Some("address.country_code"),
            ("search", "country") => Some("address.country"),
            ("search", "region") => Some("address.state"),
            ("search", "locality") => Some("address.city"),
            ("search", "county") => Some("address.county"),
            ("search", "street") => Some("address.road"),
            ("search", "housenumber") => Some("address.house_number"),
            ("search", "postalcode") => Some("address.postcode"),
            _ => None,
        };
        match path {
            Some(path) if name == "country_a" => match alpha3_to_alpha2(value) {
                Some(cc) => {
                    mapped.insert(path.into(), cc.to_ascii_uppercase());
                }
                None => {
                    partial.push(format!("country_a {value}"));
                    complete = false;
                }
            },
            Some(path) => {
                mapped.insert(path.into(), value.into());
            }
            None => {
                partial.push(format!("expected field {name}"));
                complete = false;
            }
        }
    }
    (mapped, complete)
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
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_request_and_rank_or_marks_unsupported() {
        let args = Args {
            input: PathBuf::new(),
            filter_country: Some("us".into()),
            corpus_name: None,
        };
        let suite = json!({"name": "sample", "endpoint": "search", "priorityThresh": 5});
        let case = json!({
            "id": "address-1",
            "in": {"text": "30 West 26th Street", "focus.point.lat": 40.74, "focus.point.lon": -73.99},
            "expected": {"priorityThresh": 2, "properties": [
                {"name": "30 West 26th Street", "street": "West 26th Street", "housenumber": "30", "country_a": "USA"}
            ]}
        });
        let converted = convert_case(&case, &suite, &args).unwrap();
        assert_eq!(converted["request"]["path"], "/search");
        assert_eq!(converted["request"]["query"]["bias_lat"], "40.74");
        assert_eq!(converted["request"]["query"]["bias_lng"], "-73.99");
        assert!(converted["request"]["query"].get("country_code").is_none());
        assert_eq!(converted["expect"]["checks"][0]["limit"], 2);
        assert_eq!(
            converted["expect"]["checks"][0]["fields"]["name_or_address"],
            "30 West 26th Street"
        );
        assert!(converted.get("unsupported_reason").is_none());

        let mut unsupported = case;
        unsupported["in"]["layers"] = json!("address");
        let converted = convert_case(&unsupported, &suite, &args).unwrap();
        assert!(converted["unsupported_reason"]
            .as_str()
            .unwrap()
            .contains("request parameter layers"));

        let coordinates = json!({
            "id": "place-2",
            "in": {"text": "Philadelphia"},
            "size": ">= 1",
            "unexpected": {"properties": [{"name": "Chicago"}]},
            "expected": {"coordinates": [-75.16, 39.95], "properties": [{"name": "Philadelphia", "country_a": "USA"}]}
        });
        let converted = convert_case(&coordinates, &suite, &args).unwrap();
        let kinds: Vec<_> = converted["expect"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|check| check["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "result_matches_within",
                "result_coord_within",
                "result_count",
                "result_absent"
            ]
        );
        assert!(converted.get("unsupported_reason").is_none());
        assert!(converted.get("partial_reason").is_none());
    }

    #[test]
    fn alpha3_to_alpha2_covers_common() {
        assert_eq!(alpha3_to_alpha2("AUS"), Some("au".into()));
        assert_eq!(alpha3_to_alpha2("usa"), Some("us".into()));
        assert_eq!(alpha3_to_alpha2("XYZ"), None);
    }
}
