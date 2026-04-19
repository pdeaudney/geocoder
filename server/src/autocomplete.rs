//! Prefix-matching autocomplete backed by [`fst::Map`]s — small,
//! dense tries ideal for typeahead latency (sub-millisecond per query).
//!
//! Radar's blog posts note their FST fast-path handles ~80% of traffic
//! one order of magnitude faster than a tantivy query. We use FSTs for
//! autocomplete specifically rather than as a general query cache: the
//! FST stores every street / place name's canonicalised form keyed to
//! a compact u64 payload `(kind << 56 | rank << 48 | coord_hash)` —
//! enough to return a useful `/autocomplete` hit list without a
//! tantivy round-trip.
//!
//! # On-disk layout
//!
//! One or both files per country (loaded opportunistically):
//!
//! - `fst_<cc>.fst` — serialised `fst::Map<Vec<u8>>` built from
//!   `<canonical_name> -> u64_payload`.
//! - `fst_<cc>.bin` — parallel array of full `AutocompleteEntry` records
//!   (the FST payload is an index into this array). Keeps the FST small
//!   while letting us carry the display name + suburb + lat/lng per
//!   match without encoding them into the u64.

use fst::{Automaton, IntoStreamer, Map, Streamer};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

/// Kinds — matches `forward::KIND_PLACE` / `forward::KIND_STREET`
/// numerically, duplicated here so the autocomplete module compiles
/// without the forward feature.
pub const KIND_PLACE: u8 = 1;
pub const KIND_STREET: u8 = 2;

/// Fixed-size record stored in `fst_<cc>.bin`. Mirrors the tantivy
/// schema fields clients actually need for a typeahead hit.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AutocompleteEntry {
    pub lat: f32,
    pub lng: f32,
    pub name_offset: u32,
    pub suburb_offset: u32,
    pub kind: u8,
    pub rank: u8,
    /// Padding to round to 20 bytes (f32×2 + u32×2 + u8×2 + 2 = 20).
    /// Exposed so builders can construct the record by name.
    pub pad: [u8; 2],
}

pub struct AutocompleteCountry {
    map: Map<Mmap>,
    entries: Mmap,
    strings: Mmap,
}

/// Minimum query length for the FST fast-path. Below this, an exact-key
/// match would usually land on the most common short token (e.g. "st")
/// and return a low-quality hit; better to fall through to tantivy.
const FST_MIN_PREFIX_LEN: usize = 2;

impl AutocompleteCountry {
    pub fn open(
        fst_path: &Path,
        entries_path: &Path,
        strings_path: &Path,
    ) -> Result<Option<Self>, String> {
        if !fst_path.exists() || !entries_path.exists() || !strings_path.exists() {
            return Ok(None);
        }
        let fst_file = File::open(fst_path)
            .map_err(|e| format!("open {}: {}", fst_path.display(), e))?;
        let fst_mmap = unsafe { Mmap::map(&fst_file) }
            .map_err(|e| format!("mmap {}: {}", fst_path.display(), e))?;
        let map = Map::new(fst_mmap).map_err(|e| format!("parse FST: {e}"))?;

        let entries = unsafe { Mmap::map(&File::open(entries_path).map_err(io)?) }.map_err(io)?;
        let strings = unsafe { Mmap::map(&File::open(strings_path).map_err(io)?) }.map_err(io)?;

        Ok(Some(AutocompleteCountry { map, entries, strings }))
    }

    /// Exact-key lookup — returns the single highest-rank entry the FST
    /// builder chose for this normalised key, or `None`. Used as the
    /// /search fast-path: a query like `"sydney"` that tokenises to a
    /// key already in the FST resolves in ~5 µs without touching tantivy.
    pub fn get_exact(&self, q: &str) -> Option<Hit> {
        let key = normalise_prefix(q);
        if key.is_empty() {
            return None;
        }
        let id = self.map.get(key.as_bytes())?;
        let records: &[AutocompleteEntry] = crate::as_typed_slice(&self.entries);
        let entry = records.get(id as usize)?;
        Some(Hit {
            name: read_cstr(&self.strings, entry.name_offset).to_owned(),
            suburb: {
                let s = read_cstr(&self.strings, entry.suburb_offset);
                if s.is_empty() { None } else { Some(s.to_owned()) }
            },
            kind: entry.kind,
            rank: entry.rank,
            lat: entry.lat as f64,
            lng: entry.lng as f64,
        })
    }

    /// Prefix-walk the FST for `q` (pre-normalised). Returns up to `limit`
    /// entries in FST key order (alphabetical). We re-rank after.
    pub fn starts_with(&self, q: &str, limit: usize) -> Vec<Hit> {
        let automaton = fst::automaton::Str::new(q).starts_with();
        let mut stream = self.map.search(automaton).into_stream();
        let mut out: Vec<Hit> = Vec::with_capacity(limit);
        let records: &[AutocompleteEntry] =
            crate::as_typed_slice(&self.entries);
        while let Some((_, id)) = stream.next() {
            let Some(entry) = records.get(id as usize) else {
                continue;
            };
            out.push(Hit {
                name: read_cstr(&self.strings, entry.name_offset).to_owned(),
                suburb: {
                    let s = read_cstr(&self.strings, entry.suburb_offset);
                    if s.is_empty() { None } else { Some(s.to_owned()) }
                },
                kind: entry.kind,
                rank: entry.rank,
                lat: entry.lat as f64,
                lng: entry.lng as f64,
            });
            if out.len() >= limit * 3 {
                break; // over-fetch for re-ranking, then truncate
            }
        }
        // Rank-aware: prefer more prominent features first (smaller rank),
        // alphabetical within a rank.
        out.sort_by(|a, b| a.rank.cmp(&b.rank).then(a.name.cmp(&b.name)));
        out.truncate(limit);
        out
    }
}

fn io(e: std::io::Error) -> String {
    format!("io: {e}")
}

fn read_cstr(pool: &[u8], offset: u32) -> &str {
    let off = offset as usize;
    let bytes = pool.get(off..).unwrap_or(&[]);
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suburb: Option<String>,
    pub kind: u8,
    pub rank: u8,
    pub lat: f64,
    pub lng: f64,
}

// --- Multi-country container ---

pub struct Autocomplete {
    per_country: std::collections::HashMap<[u8; 2], AutocompleteCountry>,
}

impl Autocomplete {
    /// Scan `dir` for `fst_<cc>.fst` + `fst_<cc>.bin` + `fst_<cc>_strings.bin`
    /// triples and open each country's FST. Returns `Ok(None)` if none found.
    pub fn open(dir: &Path) -> Result<Option<Self>, String> {
        let mut per_country: std::collections::HashMap<[u8; 2], AutocompleteCountry> =
            std::collections::HashMap::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(None);
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_ascii_lowercase) else {
                continue;
            };
            let Some(cc) = parse_fst_prefix(&name) else {
                continue;
            };
            if per_country.contains_key(&cc) {
                continue;
            }
            let prefix = format!(
                "fst_{}{}",
                cc[0] as char, cc[1] as char
            );
            let fst_path = dir.join(format!("{prefix}.fst"));
            let entries_path = dir.join(format!("{prefix}.bin"));
            let strings_path = dir.join(format!("{prefix}_strings.bin"));
            if let Some(country) = AutocompleteCountry::open(&fst_path, &entries_path, &strings_path)? {
                per_country.insert(cc, country);
            }
        }
        if per_country.is_empty() {
            return Ok(None);
        }
        Ok(Some(Autocomplete { per_country }))
    }

    pub fn is_empty(&self) -> bool {
        self.per_country.is_empty()
    }

    pub fn has_country(&self, cc: &[u8; 2]) -> bool {
        self.per_country.contains_key(&lower(cc))
    }

    pub fn countries(&self) -> impl Iterator<Item = &[u8; 2]> {
        self.per_country.keys()
    }

    /// FST fast-path for `/search`. If the (country, query) pair matches
    /// a full FST key, return the hit without going through tantivy.
    /// Returns `None` otherwise — caller falls through.
    pub fn exact_match(&self, country_code: &[u8; 2], q: &str) -> Option<Hit> {
        if q.len() < FST_MIN_PREFIX_LEN {
            return None;
        }
        self.per_country.get(&lower(country_code))?.get_exact(q)
    }

    /// Version that tries every loaded country. Returns the first hit —
    /// order across countries is not guaranteed, so only use this when
    /// no country hint is available.
    pub fn exact_match_any(&self, q: &str) -> Option<(&[u8; 2], Hit)> {
        if q.len() < FST_MIN_PREFIX_LEN {
            return None;
        }
        for (cc, idx) in &self.per_country {
            if let Some(hit) = idx.get_exact(q) {
                return Some((cc, hit));
            }
        }
        None
    }

    /// Prefix search within a specific country.
    pub fn search(&self, country_code: &[u8; 2], q: &str, limit: usize) -> Vec<Hit> {
        let Some(country) = self.per_country.get(&lower(country_code)) else {
            return Vec::new();
        };
        country.starts_with(&normalise_prefix(q), limit)
    }

    /// Prefix search across every loaded country, merging top-N by rank.
    /// Scans every country's FST — appropriate only when no hint is given
    /// and the deployment has a handful of countries.
    pub fn search_any(&self, q: &str, limit: usize) -> Vec<Hit> {
        let prefix = normalise_prefix(q);
        let mut merged: Vec<Hit> = Vec::new();
        for country in self.per_country.values() {
            merged.extend(country.starts_with(&prefix, limit));
        }
        merged.sort_by(|a, b| a.rank.cmp(&b.rank).then(a.name.cmp(&b.name)));
        merged.truncate(limit);
        merged
    }
}

/// Normalise a user-typed prefix the same way the builder did: lowercase,
/// ASCII fold, strip non-alphanumeric. Short circuit on empty so callers
/// don't spam the FST with a zero-length prefix (which would match everything).
pub fn normalise_prefix(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for ch in q.chars() {
        for c in ascii_fold_char(ch).chars() {
            if c.is_alphanumeric() {
                out.extend(c.to_lowercase());
            } else if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
        }
    }
    let trimmed = out.trim();
    trimmed.to_owned()
}

fn ascii_fold_char(ch: char) -> String {
    match ch {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => "a".into(),
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => "e".into(),
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => "i".into(),
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' | 'Ø' => "o".into(),
        'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => "u".into(),
        'ñ' | 'Ñ' => "n".into(),
        'ç' | 'Ç' => "c".into(),
        'ß' => "ss".into(),
        c => c.to_string(),
    }
}

fn lower(cc: &[u8; 2]) -> [u8; 2] {
    [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()]
}

/// Matches any of `fst_<cc>.fst`, `fst_<cc>.bin`, `fst_<cc>_strings.bin`.
fn parse_fst_prefix(filename: &str) -> Option<[u8; 2]> {
    let rest = filename.strip_prefix("fst_")?;
    // Strip any of the known suffixes:
    let cc_str = rest
        .strip_suffix(".fst")
        .or_else(|| rest.strip_suffix("_strings.bin"))
        .or_else(|| rest.strip_suffix(".bin"))?;
    let b = cc_str.as_bytes();
    if b.len() != 2 || !b[0].is_ascii_alphabetic() || !b[1].is_ascii_alphabetic() {
        return None;
    }
    Some([b[0].to_ascii_lowercase(), b[1].to_ascii_lowercase()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_prefixes() {
        assert_eq!(normalise_prefix("Éliz"), "eliz");
        assert_eq!(normalise_prefix("  MAIN  St  "), "main st");
        assert_eq!(normalise_prefix(""), "");
    }

    #[test]
    fn parses_fst_filenames() {
        assert_eq!(parse_fst_prefix("fst_au.fst"), Some(*b"au"));
        assert_eq!(parse_fst_prefix("fst_au.bin"), Some(*b"au"));
        assert_eq!(parse_fst_prefix("fst_au_strings.bin"), Some(*b"au"));
        assert_eq!(parse_fst_prefix("fst_US.fst"), Some(*b"us"));
        assert_eq!(parse_fst_prefix("nope.fst"), None);
        assert_eq!(parse_fst_prefix("fst_us.txt"), None);
    }
}
