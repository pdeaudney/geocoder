//! Deterministic, geographically mixed samples of stored forward address documents.
//! Usage: sample-addresses <index-dir> [addresses-per-country, default 1000]
use query_server::forward::{display_postcode, KIND_ADDRESS};
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use tantivy::schema::{Schema, Value};
use tantivy::{DocAddress, Index, TantivyDocument};

#[derive(Serialize)]
struct Address {
    country: String,
    street: String,
    number: String,
    unit: String,
    city: String,
    state: String,
    postcode: String,
    lat: f64,
    lon: f64,
    stratum: &'static str,
}

fn text(doc: &TantivyDocument, schema: &Schema, name: &str) -> String {
    schema
        .get_field(name)
        .ok()
        .and_then(|field| doc.get_first(field))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

fn stratum(number: &str, unit: &str) -> &'static str {
    if !unit.is_empty() {
        "unit"
    } else if number.split_once('-').is_some_and(|(a, b)| {
        match (a.parse::<u32>(), b.parse::<u32>()) {
            (Ok(a), Ok(b)) => a < b && b - a <= 1000,
            _ => false,
        }
    }) {
        "range"
    } else if number.chars().any(|c| c.is_ascii_digit())
        && number.chars().any(|c| c.is_ascii_alphabetic())
    {
        "suffix"
    } else {
        "plain"
    }
}

fn sample(path: &Path, country: &str, count: usize) -> Result<Vec<Address>, String> {
    let index = Index::open_in_dir(path).map_err(|e| e.to_string())?;
    let schema = index.schema();
    for name in [
        "name_raw",
        "housenumber_raw",
        "unit_raw",
        "suburb",
        "state",
        "postcode",
        "country_code",
        "lat",
        "lng",
    ] {
        schema.get_field(name).map_err(|e| e.to_string())?;
    }
    let reader = index.reader().map_err(|e| e.to_string())?;
    let searcher = reader.searcher();
    let segments = searcher.segment_readers();
    let kinds = segments
        .iter()
        .map(|s| s.fast_fields().u64("kind"))
        .collect::<tantivy::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    let total: u64 = segments.iter().map(|s| u64::from(s.max_doc())).sum();
    if total == 0 {
        return Err(format!("{country}: empty index"));
    }
    let seed = 0xADD2_E55
        + u64::from(u16::from_be_bytes([
            country.as_bytes()[0],
            country.as_bytes()[1],
        ]));
    let mut rng = StdRng::seed_from_u64(seed);
    let mut seen_docs = HashSet::new();
    let mut seen_addresses = HashSet::new();
    let mut buckets: [Vec<Address>; 4] = std::array::from_fn(|_| Vec::new());
    for _ in 0..count.saturating_mul(20).max(1000) {
        let mut position = rng.gen_range(0..total);
        let mut segment_ord = 0;
        while position >= u64::from(segments[segment_ord].max_doc()) {
            position -= u64::from(segments[segment_ord].max_doc());
            segment_ord += 1;
        }
        let id = position as u32;
        if !seen_docs.insert((segment_ord, id))
            || segments[segment_ord].is_deleted(id)
            || kinds[segment_ord].first(id) != Some(KIND_ADDRESS)
        {
            continue;
        }
        let doc: TantivyDocument = searcher
            .doc(DocAddress::new(segment_ord as u32, id))
            .map_err(|e| e.to_string())?;
        let street = text(&doc, &schema, "name_raw");
        let number = text(&doc, &schema, "housenumber_raw");
        let unit = text(&doc, &schema, "unit_raw");
        let city = text(&doc, &schema, "suburb");
        let postcode = display_postcode(&text(&doc, &schema, "postcode"), Some(country));
        if street.trim().is_empty()
            || number.trim().is_empty()
            || (city.trim().is_empty() && postcode.trim().is_empty())
            || !text(&doc, &schema, "country_code").eq_ignore_ascii_case(country)
        {
            continue;
        }
        let coordinate = |name| {
            schema
                .get_field(name)
                .ok()
                .and_then(|f| doc.get_first(f))
                .and_then(|v| v.as_f64())
        };
        let (Some(lat), Some(lon)) = (coordinate("lat"), coordinate("lng")) else {
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        let category = stratum(&number, &unit);
        let bucket = match category {
            "unit" => 1,
            "suffix" => 2,
            "range" => 3,
            _ => 0,
        };
        if buckets[bucket].len() >= count {
            continue;
        }
        if !seen_addresses.insert((
            street.to_lowercase(),
            number.to_lowercase(),
            unit.to_lowercase(),
            city.to_lowercase(),
            postcode.to_lowercase(),
        )) {
            continue;
        }
        buckets[bucket].push(Address {
            country: country.to_owned(),
            street,
            number,
            unit,
            city,
            state: text(&doc, &schema, "state"),
            postcode,
            lat,
            lon,
            stratum: category,
        });
    }
    let mut selected = Vec::new();
    for bucket in &mut buckets[1..] {
        let n = (count / 10).min(bucket.len());
        selected.extend(bucket.drain(..n));
    }
    for bucket in &mut buckets {
        let n = (count - selected.len()).min(bucket.len());
        selected.extend(bucket.drain(..n));
    }
    if selected.len() != count {
        return Err(format!(
            "{country}: found only {} eligible unique addresses, wanted {count}",
            selected.len()
        ));
    }
    Ok(selected)
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if !(2..=3).contains(&args.len()) {
        return Err("usage: sample-addresses <index-dir> [addresses-per-country]".into());
    }
    let count = args
        .get(2)
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(1000);
    if count == 0 || count > 100_000 {
        return Err("sample must be between 1 and 100000".into());
    }
    let root = Path::new(&args[1]);
    // shortcut: samples country shards; add monolithic selection when benchmarking that layout.
    let mut paths = std::fs::read_dir(root)
        .map_err(|e| e.to_string())?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    paths.sort();
    let mut addresses = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let Some(country) = name
            .strip_prefix("tantivy_")
            .filter(|cc| cc.len() == 2 && cc.bytes().all(|c| c.is_ascii_alphabetic()))
        else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        let selected = sample(&path, &country.to_ascii_uppercase(), count)?;
        eprintln!("{country}: {} sampled addresses", selected.len());
        addresses.extend(selected);
    }
    if addresses.is_empty() {
        return Err("no country forward shards found".into());
    }
    serde_json::to_writer_pretty(std::io::stdout().lock(), &addresses).map_err(|e| e.to_string())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn range_strata_do_not_treat_queens_identifiers_as_ranges() {
        assert_eq!(stratum("255-257", ""), "range");
        assert_eq!(stratum("85-23", ""), "plain");
        assert_eq!(stratum("42A", ""), "suffix");
        assert_eq!(stratum("42", "3B"), "unit");
    }
}
