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
//! Two alternative layouts supported; the runtime prefers unified when
//! both are present. Builders can emit one or both.
//!
//! **Per-country** (legacy, one set of files per country):
//! - `fst_<cc>.fst` — serialised `fst::Map<Vec<u8>>` built from
//!   `<canonical_name> -> u64_payload`.
//! - `fst_<cc>.bin` — parallel array of full `AutocompleteEntry` records
//!   (the FST payload is an index into this array).
//! - `fst_<cc>_strings.bin` — NUL-terminated string pool.
//!
//! **Unified** (Radar-style — one FST across all countries, keys
//! prefixed by the 2-byte ISO code, country scoping done at query time
//! via a custom `fst::Automaton`):
//! - `fst_unified.fst` — keys are `<cc_lower:2><normalised_name>`.
//! - `fst_unified.bin` — parallel array of `AutocompleteEntry`s, shared
//!   across all countries.
//! - `fst_unified_strings.bin` — shared string pool.
//!
//! The unified layout costs one file descriptor per process instead of
//! N (where N is the number of loaded countries), has a single FST
//! metadata header instead of N, and skips the per-country map bookkeeping.
//! It trades the ability to swap a single country's FST independently
//! (you'd have to rebuild the whole file) for fewer moving parts.

use fst::{Automaton, IntoStreamer, Map, Streamer};
use memmap2::Mmap;
use std::collections::{BinaryHeap, HashSet};
use std::fs::File;
use std::path::Path;

/// Hard cap on FST keys visited per prefix walk. Prevents pathological
/// broad prefixes (e.g. a single letter) from pinning the worker on a
/// 100k-entry walk. Chosen to comfortably exceed the rank-truncation
/// window for any reasonable `limit`.
const FST_WALK_CAP: usize = 10_000;

/// Kinds — matches `forward::KIND_PLACE` / `forward::KIND_STREET` /
/// `forward::KIND_POI` numerically, duplicated here so the
/// autocomplete module compiles without the forward feature.
pub const KIND_PLACE: u8 = 1;
pub const KIND_STREET: u8 = 2;
pub const KIND_POI: u8 = 3;
pub const KIND_POSTCODE: u8 = 5;

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
        let fst_file =
            File::open(fst_path).map_err(|e| format!("open {}: {}", fst_path.display(), e))?;
        let fst_mmap = unsafe { Mmap::map(&fst_file) }
            .map_err(|e| format!("mmap {}: {}", fst_path.display(), e))?;
        crate::log_loaded_file(
            "autocomplete",
            &fst_path.display().to_string(),
            fst_mmap.len() as u64,
        );
        let map = Map::new(fst_mmap).map_err(|e| format!("parse FST: {e}"))?;

        let entries = unsafe { Mmap::map(&File::open(entries_path).map_err(io)?) }.map_err(io)?;
        crate::log_loaded_file(
            "autocomplete",
            &entries_path.display().to_string(),
            entries.len() as u64,
        );
        let strings = unsafe { Mmap::map(&File::open(strings_path).map_err(io)?) }.map_err(io)?;
        crate::log_loaded_file(
            "autocomplete",
            &strings_path.display().to_string(),
            strings.len() as u64,
        );

        Ok(Some(AutocompleteCountry {
            map,
            entries,
            strings,
        }))
    }

    /// Exact-key lookup — returns the single highest-rank entry the FST
    /// builder chose for this normalised key, or `None`. Used as the
    /// /search fast-path: a query like `"sydney"` that tokenises to a
    /// key already in the FST resolves in ~5 µs without touching tantivy.
    pub fn get_exact(&self, q: &str) -> Option<Hit> {
        let key = normalise_prefix(q);
        if key.len() < FST_MIN_PREFIX_LEN {
            return None;
        }
        let id = self.map.get(key.as_bytes())?;
        let records: &[AutocompleteEntry] = crate::as_typed_slice(&self.entries);
        let entry = records.get(id as usize)?;
        Some(entry_to_hit(entry, &self.strings))
    }

    /// Prefix-walk the FST for `q` (pre-normalised). Returns the
    /// top-`limit` entries by rank (smallest-rank wins), alphabetical
    /// within a rank. Walks up to [`FST_WALK_CAP`] keys before bailing —
    /// enough to surface the best match for any realistic autocomplete
    /// prefix without pinning CPU on a 100k-entry country-wide walk.
    pub fn starts_with(&self, q: &str, limit: usize) -> Vec<Hit> {
        if limit == 0 {
            return Vec::new();
        }
        // Empty-prefix short-circuit. Without this, an `fst::automaton::Str`
        // built from `""` matches every key in the FST — the walk hits
        // FST_WALK_CAP (10 000) before it bails and returns rank-sorted
        // junk. Mirrors the unified-FST `starts_with` at the bottom of
        // this file. Caller is expected to have already normalised `q`,
        // so an empty string here means "the user typed nothing typeable"
        // — the right answer is `[]`, not a sample of the whole country.
        if q.is_empty() {
            return Vec::new();
        }
        let automaton = fst::automaton::Str::new(q).starts_with();
        let mut stream = self.map.search(automaton).into_stream();
        let records: &[AutocompleteEntry] = crate::as_typed_slice(&self.entries);
        // Heap key is (rank, id): we drop string names from the heap so a
        // broad prefix that visits ~10k candidates doesn't allocate ~10k
        // `String`s only for 10 to survive. Final alphabetic tie-break is
        // applied in `heap_into_sorted_hits` over the surviving `limit`
        // items. For rank-ties, heap eviction now breaks on id (smaller
        // wins) instead of alphabetic — in practice this still produces
        // the same top-N for any real prefix because items are stored in
        // rank-then-name order upstream.
        let mut heap: BinaryHeap<(u8, u64)> = BinaryHeap::with_capacity(limit + 1);
        let mut visited = 0usize;
        while let Some((_, id)) = stream.next() {
            visited += 1;
            if visited > FST_WALK_CAP {
                break;
            }
            let Some(entry) = records.get(id as usize) else {
                continue;
            };
            heap_push_bounded(&mut heap, entry.rank, id, limit);
        }
        heap_into_sorted_hits(heap, records, &self.strings)
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

fn entry_to_hit(entry: &AutocompleteEntry, strings: &[u8]) -> Hit {
    let suburb = read_cstr(strings, entry.suburb_offset);
    Hit {
        name: read_cstr(strings, entry.name_offset).to_owned(),
        suburb: if suburb.is_empty() {
            None
        } else {
            Some(suburb.to_owned())
        },
        kind: entry.kind,
        rank: entry.rank,
        lat: entry.lat as f64,
        lng: entry.lng as f64,
    }
}

/// Drain `heap` into a rank-then-name sorted `Vec<Hit>`. String names
/// are materialised here — once per surviving heap entry — rather than
/// per-candidate at push time.
fn heap_into_sorted_hits(
    heap: BinaryHeap<(u8, u64)>,
    records: &[AutocompleteEntry],
    strings: &[u8],
) -> Vec<Hit> {
    let mut out: Vec<Hit> = heap
        .into_iter()
        .filter_map(|(_, id)| records.get(id as usize).map(|e| entry_to_hit(e, strings)))
        .collect();
    out.sort_by(|a, b| a.rank.cmp(&b.rank).then(a.name.cmp(&b.name)));
    out
}

/// Push an entry into a bounded max-heap keyed on rank (smallest wins).
/// Breaks ties by id. When the heap exceeds `limit`, pops the worst —
/// always retaining the best `limit` observed so far. Zero-alloc on
/// every push; strings are resolved only in `heap_into_sorted_hits`.
#[inline]
fn heap_push_bounded(heap: &mut BinaryHeap<(u8, u64)>, rank: u8, id: u64, limit: usize) {
    heap.push((rank, id));
    if heap.len() > limit {
        heap.pop();
    }
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
    /// Radar-style unified FST: one file across all countries, country
    /// scoping done at query time via `CountryPrefixAutomaton`. Preferred
    /// when present — fewer fds, smaller metadata, same pruning behaviour.
    unified: Option<UnifiedAutocomplete>,
    /// Legacy per-country FSTs. Used for countries not present in
    /// `unified`, or as the full backing store when no unified file exists.
    per_country: std::collections::HashMap<[u8; 2], AutocompleteCountry>,
}

impl Autocomplete {
    /// Opens whichever FST layouts are present in `dir`:
    /// - `fst_unified.fst` (+ `.bin`, `_strings.bin`) → unified, preferred
    /// - `fst_<cc>.fst` (+ `.bin`, `_strings.bin`) triples → per-country
    ///
    /// Both can coexist; query routing checks unified first. Returns
    /// `Ok(None)` when neither layout is present.
    pub fn open(dir: &Path) -> Result<Option<Self>, String> {
        let unified = UnifiedAutocomplete::open(dir)?;

        let mut per_country: std::collections::HashMap<[u8; 2], AutocompleteCountry> =
            std::collections::HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
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
                let prefix = format!("fst_{}{}", cc[0] as char, cc[1] as char);
                let fst_path = dir.join(format!("{prefix}.fst"));
                let entries_path = dir.join(format!("{prefix}.bin"));
                let strings_path = dir.join(format!("{prefix}_strings.bin"));
                if let Some(country) =
                    AutocompleteCountry::open(&fst_path, &entries_path, &strings_path)?
                {
                    per_country.insert(cc, country);
                }
            }
        }

        if unified.is_none() && per_country.is_empty() {
            return Ok(None);
        }
        Ok(Some(Autocomplete {
            unified,
            per_country,
        }))
    }

    pub fn is_empty(&self) -> bool {
        self.unified.is_none() && self.per_country.is_empty()
    }

    pub fn has_country(&self, cc: &[u8; 2]) -> bool {
        let lower = lower(cc);
        let in_unified = self.unified.as_ref().is_some_and(|u| u.has_country(&lower));
        in_unified || self.per_country.contains_key(&lower)
    }

    /// Every country addressable through this autocomplete — the union
    /// of the per-country FST file set and the unified FST's covered
    /// countries. Order is unspecified. Used by diagnostics, logging,
    /// and the `/healthz/indexes` endpoint.
    pub fn countries(&self) -> Vec<[u8; 2]> {
        let mut set: HashSet<[u8; 2]> = self.per_country.keys().copied().collect();
        if let Some(u) = self.unified.as_ref() {
            for cc in u.countries_iter() {
                set.insert(*cc);
            }
        }
        set.into_iter().collect()
    }

    /// FST fast-path for `/search`. Prefers the unified FST (if loaded)
    /// so country scoping goes through [`CountryPrefixAutomaton`]; falls
    /// back to the per-country FST when unified isn't present. Min-length
    /// gate is enforced inside the concrete lookups against the
    /// normalised key.
    pub fn exact_match(&self, country_code: &[u8; 2], q: &str) -> Option<Hit> {
        let span = tracing::Span::current();
        if let Some(u) = self.unified.as_ref() {
            if let Some(hit) = u.get_exact(country_code, q) {
                span.record("geocoder.autocomplete.fst_variant", "unified");
                return Some(hit);
            }
        }
        let cc = lower(country_code);
        let result = self.per_country.get(&cc)?.get_exact(q);
        if result.is_some() {
            span.record(
                "geocoder.autocomplete.fst_variant",
                format!("per_country_{}", String::from_utf8_lossy(&cc)).as_str(),
            );
        }
        result
    }

    /// Prefix search within a specific country. Tries unified first
    /// (via `CountryPrefixAutomaton`), falls through to the per-country
    /// FST if unified isn't loaded.
    pub fn search(&self, country_code: &[u8; 2], q: &str, limit: usize) -> Vec<Hit> {
        let span = tracing::Span::current();
        if let Some(u) = self.unified.as_ref() {
            let hits = u.starts_with(country_code, q, limit);
            if !hits.is_empty() {
                span.record("geocoder.autocomplete.fst_variant", "unified");
                return hits;
            }
        }
        let Some(country) = self.per_country.get(&lower(country_code)) else {
            span.record("geocoder.autocomplete.fst_variant", "none");
            return Vec::new();
        };
        let hits = country.starts_with(&normalise_prefix(q), limit);
        span.record(
            "geocoder.autocomplete.fst_variant",
            format!(
                "per_country_{}",
                String::from_utf8_lossy(&lower(country_code))
            )
            .as_str(),
        );
        hits
    }

    /// Prefix search across every loaded country. With the unified FST
    /// this still requires running one automaton per distinct country
    /// code seen in the FST, but the runtime doesn't know that list —
    /// we approximate by iterating the per-country fallback (if present)
    /// and merging results.
    pub fn search_any(&self, q: &str, limit: usize) -> Vec<Hit> {
        let prefix = normalise_prefix(q);
        let mut merged: Vec<Hit> = Vec::new();
        for country in self.per_country.values() {
            merged.extend(country.starts_with(&prefix, limit));
        }
        merged.sort_by(|a, b| a.rank.cmp(&b.rank).then(a.name.cmp(&b.name)));
        merged.truncate(limit);
        tracing::Span::current().record("geocoder.autocomplete.fst_variant", "any");
        merged
    }
}

/// Normalise a user-typed prefix the same way the builder did: lowercase,
/// ASCII fold, strip non-alphanumeric, then collapse Saint/Mt/Ft place
/// abbreviations. Short circuit on empty so callers don't spam the FST
/// with a zero-length prefix (which would match everything).
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
    fold_place_abbreviations(out.trim())
}

/// Place-name abbreviation collapse. Inputs are expected to already be
/// lowercase, ASCII-folded, single-space-separated tokens (i.e. the
/// output of `normalise_prefix` / `normalise_fst_key` minus this step).
///
/// Whole-word, position-aware:
///   saint / sainte / st / ste  →  st
///   mount / mt                 →  mt
///   fort / ft                  →  ft
///
/// Leading `St` means Saint, while trailing `St` means Street.
/// Single-token inputs are returned unchanged.
///
/// MUST be applied identically at FST build, FST query, Tantivy build,
/// and Tantivy query — see `tests/abbrev_symmetry.rs`.
pub fn fold_place_abbreviations(s: &str) -> String {
    let parts: Vec<&str> = s.split(' ').filter(|t| !t.is_empty()).collect();
    if parts.len() < 2 {
        return s.to_owned();
    }
    let last = parts.len() - 1;
    let mut out = String::with_capacity(s.len());
    for (i, tok) in parts.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mapped = if i == last {
            if *tok == "st" {
                "street"
            } else {
                *tok
            }
        } else {
            match *tok {
                "saint" | "sainte" | "st" | "ste" => "st",
                "mount" | "mt" => "mt",
                "fort" | "ft" => "ft",
                _ => *tok,
            }
        };
        out.push_str(mapped);
    }
    out
}

/// Map one Latin-1 / Latin-Extended char to its ASCII equivalent.
/// Both upper and lower case variants are folded — uppercase first
/// (so `Ä` → `A`) is critical because callers lowercase AFTER fold;
/// without the uppercase entries `Ä` would lowercase to `ä` (still
/// non-ASCII) and pass through alphanumeric unchanged. Public so the
/// FST builder can share it — see `bin/build_autocomplete_fst.rs`.
pub fn ascii_fold_char(ch: char) -> String {
    match ch {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => {
            "a".into()
        }
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => "e".into(),
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => "i".into(),
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' | 'Ø' => {
            "o".into()
        }
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

// --- Unified FST (Radar-style country-prefix partitioning) ---

/// Custom [`fst::Automaton`] that matches keys of the form
/// `<cc[0]><cc[1]><name>` where the name starts with (or equals) the
/// given prefix.
///
/// Equivalent to running `starts_with("au" + prefix)` on the raw FST,
/// but expressed as an explicit state machine so `fst::Streamer` can
/// prune entire subtrees the moment it knows the key diverges from the
/// country code. This is the trick Radar called out: one FST across all
/// countries with country scoping at traversal time, instead of
/// maintaining 250 separate FST files.
///
/// Two modes:
/// - **Prefix** (the `starts_with_*` constructor) — matches any key
///   beginning with `<cc><prefix>...`. Used by `/autocomplete`.
/// - **Exact** (the `equals_*` constructor) — matches only the key
///   `<cc><prefix>` and no suffix. Used by the `/search` fast-path.
#[derive(Clone)]
pub struct CountryPrefixAutomaton<'a> {
    cc: [u8; 2],
    /// The bytes after the country code that drive the walk. In prefix
    /// mode these are a prefix; in exact mode these are the full
    /// post-country-code key. Named generically to avoid implying one
    /// semantic in field names and the other in method behaviour.
    tail: &'a [u8],
    exact: bool,
}

impl<'a> CountryPrefixAutomaton<'a> {
    /// Prefix-match: key must start with `<cc><prefix>`; any bytes after
    /// that are accepted.
    pub fn starts_with(cc: [u8; 2], prefix: &'a str) -> Self {
        CountryPrefixAutomaton {
            cc: lower(&cc),
            tail: prefix.as_bytes(),
            exact: false,
        }
    }

    /// Exact-match: key must be exactly `<cc><key>` with no trailing
    /// bytes. Used when the caller is looking for a single authoritative
    /// hit (the `/search` FST fast-path).
    pub fn equals(cc: [u8; 2], key: &'a str) -> Self {
        CountryPrefixAutomaton {
            cc: lower(&cc),
            tail: key.as_bytes(),
            exact: true,
        }
    }
}

/// Traversal state for [`CountryPrefixAutomaton`]. `fst::Streamer` keeps
/// one of these per visited edge; each must be `Clone` and `Default`
/// (via Dead) for the crate's API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpaState {
    /// Haven't consumed any bytes yet — expecting `cc[0]`.
    NeedCc0,
    /// Consumed `cc[0]`, expecting `cc[1]`.
    NeedCc1,
    /// Past the country code; the `usize` tracks how many bytes of the
    /// automaton's `tail` we've matched so far. Invariant:
    /// `pos <= tail.len()`.
    MatchingPrefix(usize),
    /// Consumed all of `tail` and at least one further byte — in prefix
    /// mode this is a matching absorbing state; unreachable in exact mode.
    PastPrefix,
    /// No way to reach a match from here. Once Dead, always Dead.
    Dead,
}

impl<'a> Automaton for CountryPrefixAutomaton<'a> {
    type State = CpaState;

    fn start(&self) -> CpaState {
        CpaState::NeedCc0
    }

    fn is_match(&self, state: &CpaState) -> bool {
        match state {
            CpaState::MatchingPrefix(i) if *i == self.tail.len() => true,
            CpaState::PastPrefix => !self.exact,
            _ => false,
        }
    }

    fn can_match(&self, state: &CpaState) -> bool {
        !matches!(state, CpaState::Dead)
    }

    fn will_always_match(&self, state: &CpaState) -> bool {
        if self.exact {
            return false;
        }
        // Prefix mode: every extension of a tail-complete state keeps
        // matching (MatchingPrefix(tail.len()) → PastPrefix → PastPrefix → …).
        match state {
            CpaState::PastPrefix => true,
            CpaState::MatchingPrefix(i) if *i == self.tail.len() => true,
            _ => false,
        }
    }

    fn accept(&self, state: &CpaState, byte: u8) -> CpaState {
        match state {
            CpaState::NeedCc0 => {
                if byte == self.cc[0] {
                    CpaState::NeedCc1
                } else {
                    CpaState::Dead
                }
            }
            CpaState::NeedCc1 => {
                if byte == self.cc[1] {
                    CpaState::MatchingPrefix(0)
                } else {
                    CpaState::Dead
                }
            }
            CpaState::MatchingPrefix(i) => {
                if *i < self.tail.len() {
                    if byte == self.tail[*i] {
                        CpaState::MatchingPrefix(*i + 1)
                    } else {
                        CpaState::Dead
                    }
                } else {
                    // Consumed all of tail.
                    if self.exact {
                        CpaState::Dead
                    } else {
                        CpaState::PastPrefix
                    }
                }
            }
            CpaState::PastPrefix => {
                if self.exact {
                    CpaState::Dead
                } else {
                    CpaState::PastPrefix
                }
            }
            CpaState::Dead => CpaState::Dead,
        }
    }
}

/// Unified FST covering all countries in one file. Keys are
/// `<cc[0]><cc[1]><normalised_name>`.
pub struct UnifiedAutocomplete {
    map: Map<Mmap>,
    entries: Mmap,
    strings: Mmap,
    /// Set of ISO 3166-1 alpha-2 codes (lowercase) that appear as key
    /// prefixes in the FST. Populated at load time by a single forward
    /// stream, so `has_country` is O(1). Without this, a runtime `has_country`
    /// call would return `true` for any cc whenever the unified file is
    /// present — a lie, since the builder may have been invoked with
    /// `--country au` and only covers AU.
    countries: HashSet<[u8; 2]>,
}

impl UnifiedAutocomplete {
    pub fn open(dir: &Path) -> Result<Option<Self>, String> {
        let fst_path = dir.join("fst_unified.fst");
        let entries_path = dir.join("fst_unified.bin");
        let strings_path = dir.join("fst_unified_strings.bin");
        if !fst_path.exists() || !entries_path.exists() || !strings_path.exists() {
            return Ok(None);
        }
        let fst_mmap = unsafe { Mmap::map(&File::open(&fst_path).map_err(io)?) }
            .map_err(|e| format!("mmap {}: {}", fst_path.display(), e))?;
        let map = Map::new(fst_mmap).map_err(|e| format!("parse unified FST: {e}"))?;
        let entries = unsafe { Mmap::map(&File::open(&entries_path).map_err(io)?) }.map_err(io)?;
        let strings = unsafe { Mmap::map(&File::open(&strings_path).map_err(io)?) }.map_err(io)?;
        let countries = Self::collect_countries(&map);
        Ok(Some(UnifiedAutocomplete {
            map,
            entries,
            strings,
            countries,
        }))
    }

    /// One-shot forward walk to enumerate the 2-byte key prefixes present
    /// in the FST. Keys are sorted, so the set of distinct prefixes tracks
    /// via a `last` sentinel — we touch each transition group exactly once.
    fn collect_countries(map: &Map<Mmap>) -> HashSet<[u8; 2]> {
        let mut out = HashSet::new();
        let mut stream = map.stream();
        let mut last: Option<[u8; 2]> = None;
        while let Some((k, _)) = stream.next() {
            if k.len() >= 2 {
                let cc = [k[0], k[1]];
                if Some(cc) != last {
                    out.insert(cc);
                    last = Some(cc);
                }
            }
        }
        out
    }

    fn records(&self) -> &[AutocompleteEntry] {
        crate::as_typed_slice(&self.entries)
    }

    pub fn has_country(&self, cc: &[u8; 2]) -> bool {
        self.countries.contains(&lower(cc))
    }

    /// Iterate the set of ISO 3166-1 alpha-2 codes the unified FST
    /// covers. Order is unspecified. Used by [`Autocomplete::countries`].
    pub fn countries_iter(&self) -> impl Iterator<Item = &[u8; 2]> {
        self.countries.iter()
    }

    /// Exact-key lookup for `(country, query)`. The FST stream runs the
    /// automaton which only accepts keys equal to `<cc><query>`, so the
    /// first (and only) hit is returned. Length is checked against the
    /// normalised key (not the raw query) so a user-typed 2-byte sequence
    /// like `"é"` that folds to 1 byte doesn't sneak past the floor.
    pub fn get_exact(&self, country_code: &[u8; 2], q: &str) -> Option<Hit> {
        let key = normalise_prefix(q);
        if key.len() < FST_MIN_PREFIX_LEN {
            return None;
        }
        let auto = CountryPrefixAutomaton::equals(*country_code, &key);
        let mut stream = self.map.search(auto).into_stream();
        let (_, id) = stream.next()?;
        self.records()
            .get(id as usize)
            .map(|e| entry_to_hit(e, &self.strings))
    }

    /// Prefix walk within a specific country via the country-prefix
    /// automaton. Returns the top-`limit` entries by rank, bounded by
    /// [`FST_WALK_CAP`] visited keys.
    pub fn starts_with(&self, country_code: &[u8; 2], q: &str, limit: usize) -> Vec<Hit> {
        if limit == 0 {
            return Vec::new();
        }
        let key = normalise_prefix(q);
        if key.is_empty() {
            return Vec::new();
        }
        let auto = CountryPrefixAutomaton::starts_with(*country_code, &key);
        let mut stream = self.map.search(auto).into_stream();
        let records = self.records();
        // Heap key is (rank, id): we drop string names from the heap so a
        // broad prefix that visits ~10k candidates doesn't allocate ~10k
        // `String`s only for 10 to survive. Final alphabetic tie-break is
        // applied in `heap_into_sorted_hits` over the surviving `limit`
        // items. For rank-ties, heap eviction now breaks on id (smaller
        // wins) instead of alphabetic — in practice this still produces
        // the same top-N for any real prefix because items are stored in
        // rank-then-name order upstream.
        let mut heap: BinaryHeap<(u8, u64)> = BinaryHeap::with_capacity(limit + 1);
        let mut visited = 0usize;
        while let Some((_, id)) = stream.next() {
            visited += 1;
            if visited > FST_WALK_CAP {
                break;
            }
            let Some(entry) = records.get(id as usize) else {
                continue;
            };
            heap_push_bounded(&mut heap, entry.rank, id, limit);
        }
        heap_into_sorted_hits(heap, records, &self.strings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_prefixes() {
        assert_eq!(normalise_prefix("Éliz"), "eliz");
        assert_eq!(normalise_prefix("  MAIN  St  "), "main street");
        assert_eq!(normalise_prefix(""), "");
    }

    #[test]
    fn collapses_saint_mount_fort_when_leading() {
        // Saint/St/Sainte/Ste → st when leading.
        assert_eq!(normalise_prefix("Saint Kilda"), "st kilda");
        assert_eq!(normalise_prefix("St Kilda"), "st kilda");
        assert_eq!(normalise_prefix("Sainte-Foy"), "st foy");
        assert_eq!(normalise_prefix("Ste-Foy"), "st foy");

        // Mount/Mt → mt when leading.
        assert_eq!(normalise_prefix("Mount Pleasant"), "mt pleasant");
        assert_eq!(normalise_prefix("Mt Pleasant"), "mt pleasant");

        // Fort/Ft → ft when leading.
        assert_eq!(normalise_prefix("Fort Worth"), "ft worth");
        assert_eq!(normalise_prefix("Ft Worth"), "ft worth");

        // Trailing St is Street. Mt and Ft remain as written.
        assert_eq!(normalise_prefix("Hampton St"), "hampton street");
        assert_eq!(normalise_prefix("Camp Mt"), "camp mt");

        // Single-token queries are returned unchanged (no position to apply).
        assert_eq!(normalise_prefix("Saint"), "saint");
        assert_eq!(normalise_prefix("Mt"), "mt");

        // Three+ tokens: the leading saint still collapses.
        assert_eq!(normalise_prefix("Saint James Court"), "st james court");
        assert_eq!(normalise_prefix("St James Court"), "st james court");
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
