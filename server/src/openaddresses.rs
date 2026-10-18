//! Per-country address-point indexes derived from OpenAddresses.io data.
//!
//! OpenAddresses is a community aggregator that normalises ~60 national
//! address datasets (G-NAF for AU, BAN for FR, TIGER for US, BAG for NL,
//! and more) into a single CSV schema. Our `build-openaddresses-index`
//! tool reads those CSVs and emits per-country binary files using the
//! shared `address_points` format:
//!
//! ```text
//! oa_au_points.bin   oa_au_cells.bin   oa_au_entries.bin   oa_au_strings.bin
//! oa_fr_points.bin   oa_fr_cells.bin   ...
//! oa_us_points.bin   ...
//! ```
//!
//! At startup, `OpenAddresses::open(dir)` scans for `oa_??_*` shard files
//! in the index directory and opens each country's set. Deployments
//! that only serve AU + NZ mount just those two countries' bin files and
//! ignore the rest — no virtual memory or disk paid for unused geographies.
//!
//! At query time, callers dispatch by country code (from the reverse-geocode
//! context or an explicit `country_code` filter on forward queries). Queries
//! with no country hint fan out across all loaded countries; latency scales
//! with the number of loaded countries, so deployments with many should
//! prefer to always provide the country filter.

use crate::address_points::{AddressMatch, AddressPointIndex};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

pub struct OpenAddresses {
    per_country: HashMap<[u8; 2], AddressPointIndex>,
}

impl OpenAddresses {
    /// Open every country-specific index found in `dir`. A partial shard
    /// must fail rather than silently disappear from service.
    pub fn open(dir: &Path) -> Result<Option<Self>, String> {
        let entries = match std::fs::read_dir(dir) {
            Ok(it) => it,
            Err(_) => return Ok(None),
        };

        let mut country_codes: Vec<[u8; 2]> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("read_dir: {e}"))?;
            let Some(name) = entry.file_name().to_str().map(str::to_ascii_lowercase) else {
                continue;
            };
            if let Some(cc) = parse_oa_country_prefix(&name) {
                if !country_codes.contains(&cc) {
                    country_codes.push(cc);
                }
            }
        }

        if country_codes.is_empty() {
            return Ok(None);
        }

        let mut per_country: HashMap<[u8; 2], AddressPointIndex> = HashMap::new();
        for cc in country_codes {
            let prefix = oa_prefix(cc);
            // Use a static label (same for every country shard) so log
            // pipelines can group by `index=open_addresses` without
            // exploding into one stream per country. The path itself
            // already carries the per-country identifier.
            if let Some(index) =
                AddressPointIndex::open_with_prefix_labeled(dir, &prefix, "open_addresses")?
            {
                per_country.insert(cc, index);
            }
        }

        if per_country.is_empty() {
            return Ok(None);
        }
        Ok(Some(OpenAddresses { per_country }))
    }

    /// Country codes (ISO 3166-1 alpha-2, lowercase) for which we have a
    /// loaded index. Stable reference, borrowed from the internal map.
    pub fn countries(&self) -> impl Iterator<Item = &[u8; 2]> {
        self.per_country.keys()
    }

    pub fn shards(&self) -> impl Iterator<Item = (&[u8; 2], &AddressPointIndex)> {
        self.per_country.iter()
    }

    pub fn has_country(&self, country_code: &[u8; 2]) -> bool {
        self.per_country.contains_key(&normalise_code(country_code))
    }

    /// Housenumber-exact lookup for a specific country.
    // lookup keys are independent query fields
    #[allow(clippy::too_many_arguments)]
    pub fn find_by_housenumber(
        &self,
        country_code: &[u8; 2],
        housenumber: &str,
        street_hint: Option<&str>,
        unit_hint: Option<&str>,
        near_lat: f64,
        near_lng: f64,
        street_level: u64,
    ) -> Option<AddressMatch<'_>> {
        let idx = self.per_country.get(&normalise_code(country_code))?;
        idx.find_by_housenumber(
            housenumber,
            street_hint,
            unit_hint,
            near_lat,
            near_lng,
            street_level,
        )
    }

    /// Nearest-point lookup for a specific country.
    pub fn find_nearest(
        &self,
        country_code: &[u8; 2],
        lat: f64,
        lng: f64,
        street_level: u64,
    ) -> Option<AddressMatch<'_>> {
        let idx = self.per_country.get(&normalise_code(country_code))?;
        idx.find_nearest(lat, lng, street_level)
    }

    /// Nearest-point lookup across every loaded country. Used when the
    /// caller has no country hint — typically a rare path, since Traccar
    /// dispatches usually know which country they're in. Scans every
    /// country's index; cost scales linearly with the number loaded.
    pub fn find_nearest_any(
        &self,
        lat: f64,
        lng: f64,
        street_level: u64,
    ) -> Option<AddressMatch<'_>> {
        let mut best: Option<(f64, AddressMatch<'_>)> = None;
        for idx in self.per_country.values() {
            let Some(m) = idx.find_nearest(lat, lng, street_level) else {
                continue;
            };
            let dlat = (m.lat - lat).to_radians();
            let dlng = (m.lng - lng).to_radians();
            let cos_lat = lat.to_radians().cos();
            let dist = dlat * dlat + dlng * dlng * cos_lat * cos_lat;
            let take = match &best {
                None => true,
                Some((best_dist, _)) => dist < *best_dist,
            };
            if take {
                best = Some((dist, m));
            }
        }
        best.map(|(_, m)| m)
    }
}

/// Build-tool side: compute the filename prefix for a country code.
/// `[b'A', b'U']` → `"oa_au"`.
pub fn oa_prefix(cc: [u8; 2]) -> String {
    let cc = normalise_code(&cc);
    format!(
        "oa_{}{}",
        (cc[0] as char).to_ascii_lowercase(),
        (cc[1] as char).to_ascii_lowercase(),
    )
}

/// Parse the country code out of any address-point shard filename.
/// Returns `None` when the filename doesn't match a known component.
pub fn parse_oa_country_prefix(filename: &str) -> Option<[u8; 2]> {
    let rest = filename.strip_prefix("oa_")?;
    let cc_str = [
        "_points.bin",
        "_cells.bin",
        "_entries.bin",
        "_strings.bin",
        "_points.schema.json",
    ]
    .iter()
    .find_map(|suffix| rest.strip_suffix(suffix))?;
    let bytes = cc_str.as_bytes();
    if bytes.len() != 2 || !bytes[0].is_ascii_alphabetic() || !bytes[1].is_ascii_alphabetic() {
        return None;
    }
    Some([bytes[0].to_ascii_lowercase(), bytes[1].to_ascii_lowercase()])
}

/// US and CA OpenAddresses sources often publish ALL-CAPS text with USPS
/// abbreviations ("3122 16TH ST"). Rewrite it to read like OSM
/// ("3122 16th Street"), in the spirit of Pelias's openaddresses importer.
/// A value with any lower-case letter comes from a source that already
/// cases its text, so it passes through unchanged.
pub fn normalise_street(s: &str) -> Cow<'_, str> {
    if !is_all_caps(s) {
        return Cow::Borrowed(s);
    }
    let raw: Vec<&str> = s.split_whitespace().collect();
    let mut words: Vec<String> = raw.iter().map(|w| title_case_word(w)).collect();
    let n = raw.len();
    let suffix_at = if n >= 2 && is_directional(raw[n - 1]) {
        n - 2
    } else {
        n - 1
    };
    let mut suffix_expanded = false;
    // A leading "ST" is Saint ("ST JAMES PL").
    if suffix_at > 0 {
        if let Some(full) = street_suffix(raw[suffix_at].trim_end_matches('.')) {
            words[suffix_at] = full.to_owned();
            suffix_expanded = true;
        }
    }
    // "168 PL" is 168th Place, as OSM spells it.
    if suffix_expanded && raw[suffix_at - 1].bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(number) = raw[suffix_at - 1].parse() {
            words[suffix_at - 1] = ordinal(number);
        }
    }
    // "N ST NE" is the letter street N, not North Street.
    if suffix_at >= 1 && !(suffix_at == 1 && suffix_expanded) {
        if let Some(full) = leading_directional(raw[0]) {
            words[0] = full.to_owned();
        }
    }
    Cow::Owned(words.join(" "))
}

/// "1st", "22nd", "113th".
pub fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Title-case an ALL-CAPS city or unit; see [`normalise_street`].
pub fn normalise_name(s: &str) -> Cow<'_, str> {
    if !is_all_caps(s) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.split_whitespace()
            .map(title_case_word)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn is_all_caps(s: &str) -> bool {
    s.chars().any(char::is_alphabetic) && !s.chars().any(char::is_lowercase)
}

fn is_directional(w: &str) -> bool {
    matches!(w, "N" | "S" | "E" | "W" | "NE" | "NW" | "SE" | "SW")
}

fn leading_directional(w: &str) -> Option<&'static str> {
    Some(match w {
        "N" => "North",
        "S" => "South",
        "E" => "East",
        "W" => "West",
        _ => return None,
    })
}

fn street_suffix(w: &str) -> Option<&'static str> {
    Some(match w {
        "ST" => "Street",
        "AVE" | "AV" => "Avenue",
        "BLVD" => "Boulevard",
        "DR" => "Drive",
        "RD" => "Road",
        "LN" => "Lane",
        "CT" => "Court",
        "PL" => "Place",
        "PKWY" => "Parkway",
        "HWY" => "Highway",
        "TER" => "Terrace",
        "CIR" => "Circle",
        "TRL" | "TR" => "Trail",
        "SQ" => "Square",
        "CRES" => "Crescent",
        "EXPY" => "Expressway",
        "FWY" => "Freeway",
        "ALY" => "Alley",
        "PLZ" => "Plaza",
        "XING" => "Crossing",
        _ => return None,
    })
}

fn title_case_word(w: &str) -> String {
    w.split('-')
        .map(title_case_part)
        .collect::<Vec<_>>()
        .join("-")
}

fn title_case_part(p: &str) -> String {
    if p.starts_with(|c: char| c.is_ascii_digit()) {
        let letters = p.trim_start_matches(|c: char| c.is_ascii_digit());
        return if matches!(letters, "ST" | "ND" | "RD" | "TH") {
            p.to_lowercase()
        } else {
            p.to_owned()
        };
    }
    if matches!(p, "NE" | "NW" | "SE" | "SW" | "US" | "II" | "III" | "IV") {
        return p.to_owned();
    }
    if let Some(rest) = p.strip_prefix("MC").filter(|r| !r.is_empty()) {
        return format!("Mc{}", capitalise(rest));
    }
    if let Some((head, tail)) = p.split_once('\'') {
        if head.chars().count() == 1 && !tail.is_empty() {
            return format!("{head}'{}", capitalise(tail));
        }
    }
    capitalise(p)
}

fn capitalise(p: &str) -> String {
    let mut chars = p.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

fn normalise_code(cc: &[u8; 2]) -> [u8; 2] {
    [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_oa_filenames() {
        assert_eq!(parse_oa_country_prefix("oa_au_points.bin"), Some(*b"au"));
        assert_eq!(parse_oa_country_prefix("oa_us_points.bin"), Some(*b"us"));
        assert_eq!(parse_oa_country_prefix("oa_us_cells.bin"), Some(*b"us"));
        assert_eq!(
            parse_oa_country_prefix("oa_us_points.schema.json"),
            Some(*b"us")
        );
        // Upper-case input → normalised to lower.
        assert_eq!(parse_oa_country_prefix("oa_FR_points.bin"), Some(*b"fr"));
    }

    #[test]
    fn rejects_non_oa_filenames() {
        assert_eq!(parse_oa_country_prefix("gnaf_points.bin"), None);
        assert_eq!(parse_oa_country_prefix("oa_usa_points.bin"), None);
        assert_eq!(parse_oa_country_prefix("oa_u1_points.bin"), None);
        assert_eq!(parse_oa_country_prefix("oa__points.bin"), None);
    }

    #[test]
    fn oa_prefix_roundtrips() {
        assert_eq!(oa_prefix(*b"AU"), "oa_au");
        assert_eq!(oa_prefix(*b"us"), "oa_us");
    }

    #[test]
    fn code_normalisation_is_case_insensitive() {
        assert_eq!(normalise_code(b"AU"), *b"au");
        assert_eq!(normalise_code(b"aU"), *b"au");
    }

    #[test]
    fn caps_streets_read_like_osm() {
        for (raw, want) in [
            ("3122 16TH ST", "3122 16th Street"),
            ("16TH ST", "16th Street"),
            ("1ST AVE", "1st Avenue"),
            ("2ND AV", "2nd Avenue"),
            ("JACKSON ST", "Jackson Street"),
            ("W 37 ST", "West 37th Street"),
            ("168 PL", "168th Place"),
            ("54 ST", "54th Street"),
            ("ROUTE 9", "Route 9"),
            ("E MAIN", "East Main"),
            ("N BROADWAY", "North Broadway"),
            ("N ST NE", "N Street NE"),
            ("E ST", "E Street"),
            ("JACKSON ST SE", "Jackson Street SE"),
            ("MACLEOD TR SW", "Macleod Trail SW"),
            ("AVENUE N", "Avenue N"),
            ("ST JAMES PL", "St James Place"),
            ("ST", "St"),
            ("ST PAUL ST", "St Paul Street"),
            ("MCDONALD AVE", "McDonald Avenue"),
            ("O'BRIEN CT", "O'Brien Court"),
            ("WILKES-BARRE BLVD", "Wilkes-Barre Boulevard"),
            ("PACIFIC COAST HWY", "Pacific Coast Highway"),
            ("SUNSET PKWY", "Sunset Parkway"),
            ("ROSE TER", "Rose Terrace"),
            ("OAK CIR", "Oak Circle"),
            ("PINE TRL", "Pine Trail"),
            ("UNION SQ", "Union Square"),
            ("MAPLE CRES", "Maple Crescent"),
            ("CROSS BRONX EXPY", "Cross Bronx Expressway"),
            ("HARBOR FWY", "Harbor Freeway"),
            ("BACK ALY", "Back Alley"),
            ("CIVIC PLZ", "Civic Plaza"),
            ("MILL XING", "Mill Crossing"),
            ("ELM DR", "Elm Drive"),
            ("CEDAR RD", "Cedar Road"),
            ("BIRCH LN", "Birch Lane"),
            ("CHERRY LOOP", "Cherry Loop"),
            ("ASPEN WAY", "Aspen Way"),
            ("US HWY 1", "US Hwy 1"),
            ("PEÑA BLVD", "Peña Boulevard"),
        ] {
            assert_eq!(normalise_street(raw), want, "{raw}");
        }
    }

    #[test]
    fn caps_names_are_title_cased_only() {
        assert_eq!(normalise_name("WOODBRIDGE"), "Woodbridge");
        assert_eq!(normalise_name("NEW YORK"), "New York");
        assert_eq!(normalise_name("ST LOUIS"), "St Louis");
        assert_eq!(normalise_name("APT 3B"), "Apt 3B");
    }

    #[test]
    fn mixed_case_and_letterless_values_pass_through() {
        for raw in [
            "3010 20th Street",
            "Example St",
            "McDonald Ave",
            "7",
            "12-14",
            "",
        ] {
            assert!(matches!(normalise_street(raw), Cow::Borrowed(_)), "{raw}");
            assert!(matches!(normalise_name(raw), Cow::Borrowed(_)), "{raw}");
        }
    }
}
