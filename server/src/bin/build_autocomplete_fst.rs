//! Build FST-backed autocomplete indexes from the reverse binary index.
//!
//! Usage:
//!   build-autocomplete-fst <reverse-index-dir>
//!     [--country cc,cc]
//!     [--layout per-country|unified|both]   (default: both)
//!
//! Layouts:
//! - **per-country**: one FST triple per country:
//!   `fst_<cc>.fst`, `fst_<cc>.bin`, `fst_<cc>_strings.bin`.
//!   Keys are the normalised (lowercased, ASCII-folded, alphanumeric-only)
//!   name; the value is an index into the entries file. Lets you swap a
//!   single country's FST independently.
//! - **unified**: one FST across all countries — `fst_unified.fst`,
//!   `.bin`, `_strings.bin`. Keys are `<cc[0]><cc[1]><normalised_name>`.
//!   Runtime prefers this when present. Fewer fds, smaller metadata,
//!   slightly smaller on disk due to shared string pool.
//!
//! Tiny compared to tantivy: a country with 500 K streets produces
//! ~5–15 MB of FST + ~8 MB of entries + ~2 MB of strings. The whole
//! AU index is <30 MB.

// mimalloc global allocator — see build-pipeline-perf-plan stage 2.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use fst::MapBuilder;
use query_server::autocomplete::{
    AutocompleteEntry, KIND_PLACE, KIND_POI, KIND_POSTCODE, KIND_STREET,
};
use query_server::i18n::ENTITY_PLACE;
use query_server::{
    as_typed_slice, manifest, Index, NodeCoord, PlacePoint, WayHeader, DEFAULT_ADMIN_CELL_LEVEL,
    DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};
use rayon::prelude::*;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

/// Per-stage timing — see build-pipeline-perf-plan stage 6.
struct Stage {
    name: &'static str,
    start: Instant,
}
impl Stage {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        eprintln!(
            "[stage] {}: {:.3}s",
            self.name,
            self.start.elapsed().as_secs_f64()
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    PerCountry,
    Unified,
    Both,
}

fn main() {
    let _total = Stage::new("total");
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "Usage: {} <reverse-index-dir> [--country cc,cc] [--layout per-country|unified|both]",
            args.first()
                .map(String::as_str)
                .unwrap_or("build-autocomplete-fst")
        );
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let country_filter: Option<HashSet<[u8; 2]>> = args
        .iter()
        .position(|a| a == "--country")
        .and_then(|p| args.get(p + 1))
        .map(|v| {
            v.split(|c: char| c == ',' || c.is_whitespace())
                .filter(|t| t.len() == 2)
                .filter_map(|t| {
                    let b = t.as_bytes();
                    if b[0].is_ascii_alphabetic() && b[1].is_ascii_alphabetic() {
                        Some([b[0].to_ascii_lowercase(), b[1].to_ascii_lowercase()])
                    } else {
                        None
                    }
                })
                .collect()
        })
        .filter(|s: &HashSet<_>| !s.is_empty());

    let layout = match args
        .iter()
        .position(|a| a == "--layout")
        .and_then(|p| args.get(p + 1))
        .map(String::as_str)
    {
        None | Some("both") => Layout::Both,
        Some("per-country") => Layout::PerCountry,
        Some("unified") => Layout::Unified,
        Some(other) => {
            eprintln!("unknown --layout value {other:?}; expected per-country|unified|both");
            std::process::exit(2);
        }
    };

    let t0 = Instant::now();
    match run(&dir, country_filter.as_ref(), layout) {
        Err(e) => {
            eprintln!("build failed: {e}");
            std::process::exit(1);
        }
        Ok(stats) => {
            let extra = json!({
                "layout": match layout {
                    Layout::PerCountry => "per-country",
                    Layout::Unified => "unified",
                    Layout::Both => "both",
                },
                "country_count": stats.country_count,
                "total_entries": stats.total_entries,
                "total_keys": stats.total_keys,
                "wof_postcodes_added": stats.wof_postcodes_added,
                "build_seconds": t0.elapsed().as_secs_f64(),
            });
            if let Err(e) = manifest::write(&dir, "autocomplete", extra) {
                eprintln!("warning: failed to write manifest_autocomplete.json: {e}");
            }
        }
    }
}

#[derive(Default)]
struct RunStats {
    country_count: usize,
    total_entries: u64,
    total_keys: u64,
    wof_postcodes_added: usize,
}

/// Per-country staging state. Lives at module scope so `emit_unified`
/// can iterate it after `run` collects it.
#[derive(Default)]
struct PerCountry {
    entries: Vec<AutocompleteEntry>,
    strings: Vec<u8>,
    /// interned string offset → previously-seen offset (for dedup)
    intern_index: HashMap<String, u32>,
    /// normalised key → entry_id, sorted for FST emit
    keys: BTreeMap<String, u64>,
}

/// One classified row, post `find_admin` and per-country filter,
/// pre intern + entry/key build. Borrows `&str` from the loaded
/// Index to keep the intermediate cheap on planet-scale data.
struct Candidate<'a> {
    cc: [u8; 2],
    name: &'a str,
    /// Alternate-language names from `i18n_names.bin`. Each becomes an
    /// additional FST key pointing at the same `AutocompleteEntry`,
    /// so a query like `"cologne"` lands on the entry whose canonical
    /// `name` is `"Köln"`.
    aliases: Vec<&'a str>,
    kind: u8,
    rank: u8,
    lat: f32,
    lng: f32,
    suburb: Option<&'a str>,
    /// Used by the sequential dedup pass after par_iter for streets
    /// (places don't dedup); kept on places too for symmetry.
    name_id: u32,
}

struct PostalCandidate<'a> {
    cc: [u8; 2],
    name: String,
    compact: String,
    lat: f32,
    lng: f32,
    suburb: Option<&'a str>,
}

/// Romance/Germanic place names commonly carry a leading definite
/// article that real users typing in autocomplete won't include.
/// Returns the name with that article stripped, or `None` if the name
/// does not start with one. Two forms are recognised:
///   - Space-separated: "A Fonsagrada" → "Fonsagrada", "The Hague" →
///     "Hague", "Le Havre" → "Havre".
///   - Apostrophe-elision: "L'Aquila" → "Aquila", "L'Aigle" → "Aigle"
///     (Italian / French / Catalan; both ASCII `'` and Unicode `’`).
/// The article match is case-insensitive; the returned remainder is
/// untouched so the existing FST normaliser handles the rest.
///
/// Conservative on purpose: only definite-article forms (the/le/la/el/...);
/// no demonstratives, possessives, or prepositions. False positives
/// here would index unrelated entries under the stripped key —
/// e.g. stripping "I" from English "I-95 corridor" would be wrong
/// (handled by the language-aware list staying short).
fn strip_leading_article(name: &str) -> Option<String> {
    let trimmed = name.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    // Apostrophe-elision: L', D', N', etc. (no space after the article).
    // Inspect the first two CHARS (not bytes) so the Unicode right single
    // quotation mark `’` (3 bytes in UTF-8) doesn't get sliced mid-codepoint.
    let mut chars = trimmed.chars();
    if let (Some(c0), Some(c1)) = (chars.next(), chars.next()) {
        let head_lower: String = [c0, c1].iter().flat_map(|c| c.to_lowercase()).collect();
        let is_elision = matches!(
            head_lower.as_str(),
            "l'" | "l\u{2019}" | "d'" | "d\u{2019}" | "n'" | "n\u{2019}"
        );
        if is_elision {
            let after = c0.len_utf8() + c1.len_utf8();
            let rest = trimmed[after..].trim_start();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }

    // Space-separated articles.
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next()?;
    let rest = parts.next().map(str::trim_start).unwrap_or("");
    if rest.is_empty() {
        return None;
    }
    let first_lower: String = first.chars().flat_map(char::to_lowercase).collect();

    // Definite articles across English / French / Spanish / Italian /
    // Portuguese / Galician / Catalan / German / Dutch / Welsh. Single-
    // character entries are dangerous (Galician "A"/"O", Welsh "Y") but
    // necessary for real OSM names like "A Coruña" / "O Barco" /
    // "Y Fenni" — the FST emits BOTH the full and stripped variants,
    // so a false-strip just adds an extra (harmless) index entry.
    const ARTICLES: &[&str] = &[
        "the", // English
        "le", "la", "les", // French
        "el", "los", "las", // Spanish
        "il", "lo", "i", "gli", // Italian
        "o", "a", "os", "as",  // Portuguese / Galician
        "els", // Catalan (overlaps les / el)
        "der", "die", "das", // German
        "de", "het", // Dutch
        "y", "yr", // Welsh
    ];
    if ARTICLES.contains(&first_lower.as_str()) {
        Some(rest.to_string())
    } else {
        None
    }
}

fn run(
    dir: &PathBuf,
    country_filter: Option<&HashSet<[u8; 2]>>,
    layout: Layout,
) -> Result<RunStats, String> {
    let dir_str = dir
        .to_str()
        .ok_or_else(|| format!("non-utf8 path: {}", dir.display()))?;
    let idx = Index::load(
        dir_str,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )?;

    fn intern(pc: &mut PerCountry, s: &str) -> u32 {
        if let Some(&off) = pc.intern_index.get(s) {
            return off;
        }
        let off = pc.strings.len() as u32;
        pc.strings.extend_from_slice(s.as_bytes());
        pc.strings.push(0);
        pc.intern_index.insert(s.to_owned(), off);
        off
    }

    /// Fresh per-country staging seeded with the empty string at offset 0
    /// so the runtime can treat 0 as the sentinel for "no suburb".
    fn fresh_per_country() -> PerCountry {
        let mut pc = PerCountry::default();
        pc.strings.push(0);
        pc.intern_index.insert(String::new(), 0);
        pc
    }

    fn ingest(pc: &mut PerCountry, cand: &Candidate<'_>) {
        let key = normalise_fst_key(cand.name);
        if key.is_empty() {
            return;
        }
        let name_offset = intern(pc, cand.name);
        let suburb_offset = match cand.suburb.filter(|s| !s.is_empty()) {
            Some(s) => intern(pc, s),
            None => 0,
        };
        let entry = AutocompleteEntry {
            lat: cand.lat,
            lng: cand.lng,
            name_offset,
            suburb_offset,
            kind: cand.kind,
            rank: cand.rank,
            pad: [0; 2],
        };
        let entry_idx = pc.entries.len() as u64;
        pc.entries.push(entry);

        // Multilingual aliases (i18n alternates) reuse the SAME entry
        // — only an extra FST key is added per alias, no extra entry.
        // Dedup against canonical happens naturally because the keys
        // BTreeMap is keyed on the normalised string; an alias whose
        // normalisation collides with the canonical is a no-op.
        // Compare against the cached `key` (already computed above)
        // so a place with N alternates does N normalisations, not 2N.
        for alias in &cand.aliases {
            let alias_key = normalise_fst_key(alias);
            if alias_key.is_empty() || alias_key == key {
                continue;
            }
            insert_key(pc, alias_key, entry_idx, cand.rank);
        }

        // Article-stripped variants. "A Fonsagrada" → also indexed as
        // "Fonsagrada"; "L'Aquila" → also "Aquila". Lets users typing
        // the recognisable part of a name reach entries whose canonical
        // OSM form starts with a definite article. Cost is an extra FST
        // key per article-bearing name (~5-10% of names in IT/FR/ES/PT/
        // GL/CA/CY OSM data). Dedup against the canonical key happens
        // via the BTreeMap, so non-article names cost only the article
        // probe, no extra insert.
        if let Some(stripped) = strip_leading_article(cand.name) {
            let stripped_key = normalise_fst_key(&stripped);
            if !stripped_key.is_empty() && stripped_key != key {
                insert_key(pc, stripped_key, entry_idx, cand.rank);
            }
        }
        for alias in &cand.aliases {
            if let Some(stripped) = strip_leading_article(alias) {
                let stripped_key = normalise_fst_key(&stripped);
                if !stripped_key.is_empty() && stripped_key != key {
                    insert_key(pc, stripped_key, entry_idx, cand.rank);
                }
            }
        }

        // Latin transliterations of every non-Latin source name
        // (canonical + alternates). ICU produces forms like
        // `Moskva` / `Chelyabinsk` / `Beijing` / `Tokyo` that real
        // users type when an OSM `name:en` is missing. The
        // resulting normalised keys are de-duplicated by the same
        // BTreeMap that handles aliases. No-op when the `translit`
        // feature is disabled (the helper compiles to a no-op).
        ingest_translit_keys(pc, cand.name, entry_idx, cand.rank, &key);
        for alias in &cand.aliases {
            ingest_translit_keys(pc, alias, entry_idx, cand.rank, &key);
        }

        insert_key(pc, key, entry_idx, cand.rank);
    }

    /// Emit FST keys for every Latin transliteration of `source`.
    /// Skips empties and forms whose normalised key collides with
    /// the canonical `canonical_key`.
    #[cfg(feature = "translit")]
    fn ingest_translit_keys(
        pc: &mut PerCountry,
        source: &str,
        entry_idx: u64,
        rank: u8,
        canonical_key: &str,
    ) {
        for latin in query_server::translit::transliterate_for_index(source) {
            let k = normalise_fst_key(&latin);
            if k.is_empty() || k == canonical_key {
                continue;
            }
            insert_key(pc, k, entry_idx, rank);
        }
    }

    #[cfg(not(feature = "translit"))]
    fn ingest_translit_keys(
        _pc: &mut PerCountry,
        _source: &str,
        _entry_idx: u64,
        _rank: u8,
        _canonical_key: &str,
    ) {
    }

    /// Insert a normalised key → entry_id mapping into `pc.keys`. When
    /// the key already exists, keep whichever entry has the lower
    /// `rank` (smaller wins). Multiple rows can share a normalised key
    /// (e.g. "main street" in many suburbs) — the FST keeps one
    /// representative entry per key.
    fn insert_key(pc: &mut PerCountry, key: String, entry_idx: u64, rank: u8) {
        pc.keys
            .entry(key)
            .and_modify(|existing_idx| {
                let existing_rank = pc.entries[*existing_idx as usize].rank;
                if rank < existing_rank {
                    *existing_idx = entry_idx;
                }
            })
            .or_insert(entry_idx);
    }

    // Phase 1a: parallel candidate extraction. find_admin per doc is
    // the hot loop; par_iter scales near-linearly with rayon thread
    // count (verified against the analogous tantivy build path; see
    // docs/performance/forward-index-parallelization-2026-04-27.md).
    let phase1a = Instant::now();
    eprintln!(
        "[stage] autocomplete_classify: starting (rayon threads = {}, RAYON_NUM_THREADS = {:?})",
        rayon::current_num_threads(),
        std::env::var("RAYON_NUM_THREADS").ok(),
    );

    let in_filter = |cc: [u8; 2]| -> bool {
        country_filter
            .map(|f| f.contains(&[cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()]))
            .unwrap_or(true)
    };

    let mut place_candidates: Vec<Candidate<'_>> = Vec::new();
    if let Some(pp) = idx.place_points.as_ref() {
        let points: &[PlacePoint] = as_typed_slice(pp);
        place_candidates = points
            .par_iter()
            .enumerate()
            .filter_map(|(place_id, p)| {
                let name = idx.get_string(p.name_id);
                if name.is_empty() {
                    return None;
                }
                let admin = idx.find_admin(p.lat as f64, p.lng as f64);
                let cc = admin.country_code.filter(|c| c[0] != 0 && c[1] != 0)?;
                if !in_filter(cc) {
                    return None;
                }

                // Pull `name:xx` alternates so a query in any of the
                // tagged languages lands on this same entry. The C++
                // builder writes i18n_names.bin sorted by
                // (entity_type, entity_id, lang_code) and uses
                // entity_type=1 for place points keyed by their index
                // in place_points.bin, which matches the slice index
                // we have here. See builder/src/build_index.cpp:633.
                let aliases: Vec<&str> = idx
                    .i18n_names
                    .as_ref()
                    .map(|i| {
                        i.alternates_for(ENTITY_PLACE, place_id as u32)
                            .map(|(_, _, name_id)| idx.get_string(name_id))
                            .filter(|alt| !alt.is_empty() && *alt != name)
                            .collect()
                    })
                    .unwrap_or_default();

                Some(Candidate {
                    cc: [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()],
                    name,
                    aliases,
                    kind: KIND_PLACE,
                    rank: p.rank as u8,
                    lat: p.lat,
                    lng: p.lng,
                    suburb: admin.city,
                    name_id: p.name_id,
                })
            })
            .collect();
    }

    let ways: &[WayHeader] = as_typed_slice(&idx.street_ways);
    let nodes: &[NodeCoord] = as_typed_slice(&idx.street_nodes);
    let street_candidates: Vec<Candidate<'_>> = ways
        .par_iter()
        .filter_map(|way| {
            let name = idx.get_string(way.name_id);
            if name.is_empty() {
                return None;
            }
            let off = way.node_offset as usize;
            let count = way.node_count as usize;
            if count == 0 || off + count > nodes.len() {
                return None;
            }
            let mid = nodes[off + count / 2];
            let admin = idx.find_admin(mid.lat as f64, mid.lng as f64);
            let cc = admin.country_code.filter(|c| c[0] != 0 && c[1] != 0)?;
            if !in_filter(cc) {
                return None;
            }
            Some(Candidate {
                cc: [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()],
                name,
                // The C++ builder doesn't emit i18n_names entries for
                // streets (only admin polygons + place points), so
                // there's nothing to expand here — keep the field for
                // shape parity with places.
                aliases: Vec::new(),
                kind: KIND_STREET,
                rank: 26,
                lat: mid.lat,
                lng: mid.lng,
                suburb: admin.city,
                name_id: way.name_id,
            })
        })
        .collect();

    // POIs (commit 5). Gated on rank ≤ 10 — only wikipedia/wikidata-
    // backed POIs make it into the FST so the file size stays small
    // enough to mmap without paging. Every named cafe / fence / bench
    // would otherwise blow up the FST size with low-value entries
    // ("McDonald's" appearing thousands of times across a country).
    let poi_candidates: Vec<Candidate<'_>> = match idx.poi_points.as_ref() {
        Some(pp) => {
            let pois: &[query_server::PoiPoint] = as_typed_slice(pp);
            pois.par_iter()
                .enumerate()
                .filter_map(|(poi_id, poi)| {
                    if poi.rank > 10 {
                        return None;
                    }
                    let name = idx.get_string(poi.name_id);
                    if name.is_empty() {
                        return None;
                    }
                    let admin = idx.find_admin(poi.lat as f64, poi.lng as f64);
                    let cc = admin.country_code.filter(|c| c[0] != 0 && c[1] != 0)?;
                    if !in_filter(cc) {
                        return None;
                    }
                    let aliases: Vec<&str> = idx
                        .i18n_names
                        .as_ref()
                        .map(|i| {
                            i.alternates_for(query_server::i18n::ENTITY_POI, poi_id as u32)
                                .map(|(_, _, name_id)| idx.get_string(name_id))
                                .filter(|alt| !alt.is_empty() && *alt != name)
                                .collect()
                        })
                        .unwrap_or_default();
                    let suburb = if poi.parent_place_id != 0 {
                        Some(idx.get_string(poi.parent_place_id))
                    } else {
                        admin.city
                    };
                    Some(Candidate {
                        cc: [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()],
                        name,
                        aliases,
                        kind: KIND_POI,
                        rank: poi.rank as u8,
                        lat: poi.lat,
                        lng: poi.lng,
                        suburb,
                        name_id: poi.name_id,
                    })
                })
                .collect()
        }
        None => Vec::new(),
    };

    // A postcode is a searchable feature, not an attribute of one
    // arbitrarily chosen house. Keep one representative per country and
    // normalised code so both spaced and unspaced input find it.
    fn add_postcode<'a>(
        codes: &mut HashMap<([u8; 2], String), PostalCandidate<'a>>,
        cc: [u8; 2],
        raw: &str,
        lat: f32,
        lng: f32,
        suburb: Option<&'a str>,
    ) {
        let code = query_server::forward::normalize_postcode(raw);
        if !(3..=10).contains(&code.len()) {
            return;
        }
        let cc_upper = [cc[0].to_ascii_uppercase(), cc[1].to_ascii_uppercase()];
        codes
            .entry((cc, code.clone()))
            .and_modify(|sample| {
                if (lat, lng) < (sample.lat, sample.lng) {
                    sample.lat = lat;
                    sample.lng = lng;
                    sample.suburb = suburb;
                }
            })
            .or_insert_with(|| PostalCandidate {
                cc,
                name: query_server::forward::display_postcode(
                    &code,
                    std::str::from_utf8(&cc_upper).ok(),
                ),
                compact: code,
                lat,
                lng,
                suburb,
            });
    }

    let mut postcodes = HashMap::new();
    let osm_addresses: &[query_server::AddrPoint] = as_typed_slice(&idx.addr_points);
    // Resolve country once per code and coarse location. The location
    // bucket preserves a postcode reused in distant countries, while
    // avoiding a polygon lookup for every house in a postal area.
    // ponytail: a reused code on both sides of a border inside one
    // degree tile gets one sample; use finer tiles if observed.
    let mut osm_samples = HashMap::new();
    for p in osm_addresses {
        if p.postcode_id != 0 {
            osm_samples
                .entry((p.postcode_id, p.lat.floor() as i16, p.lng.floor() as i16))
                .or_insert(p);
        }
    }
    let osm_codes: Vec<_> = osm_samples
        .into_par_iter()
        .filter_map(|((postcode_id, _, _), p)| {
            let raw = idx.get_string(postcode_id);
            if raw.is_empty() {
                return None;
            }
            let admin = idx.find_admin(p.lat as f64, p.lng as f64);
            let cc = admin.country_code?;
            let cc = [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()];
            (in_filter(cc) && !(cc == *b"au" && idx.gnaf.is_some()))
                .then_some((cc, raw, p.lat, p.lng, admin.city))
        })
        .collect();
    for (cc, raw, lat, lng, suburb) in osm_codes {
        add_postcode(&mut postcodes, cc, raw, lat, lng, suburb);
    }
    if let Some(gnaf) = idx.gnaf.as_ref() {
        if in_filter(*b"au") {
            let mut seen = HashSet::new();
            for p in gnaf.points() {
                if !seen.insert(p.postcode_id) {
                    continue;
                }
                add_postcode(
                    &mut postcodes,
                    *b"au",
                    gnaf.string_at(p.postcode_id),
                    p.lat,
                    p.lng,
                    Some(gnaf.string_at(p.locality_id)),
                );
            }
        }
    }
    if let Some(oa) = idx.open_addresses.as_ref() {
        for (cc, shard) in oa.shards() {
            let cc = [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()];
            if !in_filter(cc) || (cc == *b"au" && idx.gnaf.is_some()) {
                continue;
            }
            let mut seen = HashSet::new();
            for p in shard.points() {
                if !seen.insert(p.postcode_id) {
                    continue;
                }
                add_postcode(
                    &mut postcodes,
                    cc,
                    shard.string_at(p.postcode_id),
                    p.lat,
                    p.lng,
                    Some(shard.string_at(p.locality_id)),
                );
            }
        }
    }
    // WoF postalcode SQLite is exported by wof-importer as a versioned
    // text file. Add only codes absent from OSM/G-NAF/OpenAddresses: those
    // sources are tied to actual addresses, while WoF supplies a postal
    // centroid. In particular, WoF NZ codes have (0,0) and are omitted by
    // the importer rather than inventing a location for them.
    let postcodes_before_wof = postcodes.len();
    let wof_path = dir.join("wof_postcodes.tsv");
    if wof_path.exists() {
        let reader = BufReader::new(File::open(&wof_path)
            .map_err(|e| format!("open {}: {e}", wof_path.display()))?);
        for (line_no, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| format!("read {} line {}: {e}", wof_path.display(), line_no + 1))?;
            if line_no == 0 {
                if line != "#wof-postcodes-v1\tcountry\tpostcode\tlatitude\tlongitude" {
                    return Err(format!("{}: unsupported WoF postcode schema", wof_path.display()));
                }
                continue;
            }
            let mut fields = line.split('\t');
            let (Some(country), Some(raw), Some(lat), Some(lng), None) =
                (fields.next(), fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(format!("{} line {}: malformed postcode row", wof_path.display(), line_no + 1));
            };
            let cc_bytes = country.as_bytes();
            if cc_bytes.len() != 2 || !cc_bytes.iter().all(u8::is_ascii_uppercase) {
                return Err(format!("{} line {}: invalid country", wof_path.display(), line_no + 1));
            }
            let cc = [cc_bytes[0].to_ascii_lowercase(), cc_bytes[1].to_ascii_lowercase()];
            if !in_filter(cc) {
                continue;
            }
            let lat: f32 = lat.parse().map_err(|e| format!("{} line {}: latitude: {e}", wof_path.display(), line_no + 1))?;
            let lng: f32 = lng.parse().map_err(|e| format!("{} line {}: longitude: {e}", wof_path.display(), line_no + 1))?;
            if !lat.is_finite() || !lng.is_finite() || !(-90.0..=90.0).contains(&lat)
                || !(-180.0..=180.0).contains(&lng) || (lat == 0.0 && lng == 0.0) {
                return Err(format!("{} line {}: invalid coordinate", wof_path.display(), line_no + 1));
            }
            let code = query_server::forward::normalize_postcode(raw);
            if !(3..=10).contains(&code.len()) {
                continue;
            }
            postcodes.entry((cc, code.clone())).or_insert_with(|| PostalCandidate {
                cc,
                name: query_server::forward::display_postcode(&code, Some(country)),
                compact: code,
                lat,
                lng,
                suburb: None,
            });
        }
    }
    let wof_postcodes_added = postcodes.len() - postcodes_before_wof;
    eprintln!("added {wof_postcodes_added} WoF postcode candidates");
    let mut postal_candidates: Vec<_> = postcodes.into_values().collect();
    postal_candidates.sort_by(|a, b| a.cc.cmp(&b.cc).then(a.name.cmp(&b.name)));

    eprintln!(
        "[stage] autocomplete_classify: {:.3}s ({} place + {} street + {} poi + {} postcode candidates)",
        phase1a.elapsed().as_secs_f64(),
        place_candidates.len(),
        street_candidates.len(),
        poi_candidates.len(),
        postal_candidates.len(),
    );

    // Phase 1b: sequential bucket-by-country with street dedup. Cheap
    // relative to find_admin — HashSet inserts on tens of millions
    // of items.
    let phase1b = Instant::now();
    let mut by_country_cands: HashMap<[u8; 2], Vec<Candidate<'_>>> = HashMap::new();
    for cand in place_candidates {
        by_country_cands.entry(cand.cc).or_default().push(cand);
    }
    for cand in poi_candidates {
        // POIs aren't deduped: identical names in the same suburb are
        // legitimately distinct (two cafes with the same name on
        // different blocks). Rank-10 gating already keeps the volume
        // bounded.
        by_country_cands.entry(cand.cc).or_default().push(cand);
    }
    for code in &postal_candidates {
        by_country_cands
            .entry(code.cc)
            .or_default()
            .push(Candidate {
                cc: code.cc,
                name: &code.name,
                aliases: vec![&code.compact],
                kind: KIND_POSTCODE,
                rank: 12,
                lat: code.lat,
                lng: code.lng,
                suburb: code.suburb,
                name_id: 0,
            });
    }
    let mut seen: HashSet<(u32, String, [u8; 2])> = HashSet::new();
    for cand in street_candidates {
        let suburb_key = cand.suburb.unwrap_or("").to_string();
        if !seen.insert((cand.name_id, suburb_key, cand.cc)) {
            continue;
        }
        by_country_cands.entry(cand.cc).or_default().push(cand);
    }
    drop(seen);
    eprintln!(
        "[stage] autocomplete_dedup: {:.3}s ({} countries)",
        phase1b.elapsed().as_secs_f64(),
        by_country_cands.len(),
    );

    // Phase 1c: per-country intern + entry/key build, parallel via
    // rayon. Each country is independent — its own intern index,
    // entries vec, and keys BTreeMap.
    let phase1c = Instant::now();
    let by_country: HashMap<[u8; 2], PerCountry> = by_country_cands
        .into_par_iter()
        .map(|(cc, cands)| {
            let mut pc = fresh_per_country();
            for cand in &cands {
                ingest(&mut pc, cand);
            }
            (cc, pc)
        })
        .collect();
    eprintln!(
        "[stage] autocomplete_intern: {:.3}s",
        phase1c.elapsed().as_secs_f64(),
    );

    // Deterministic ordering: sort country codes before emitting so
    // rebuilds from identical input produce byte-identical .bin outputs
    // (unified entry IDs depend on iteration order).
    let mut ccs: Vec<[u8; 2]> = by_country.keys().copied().collect();
    ccs.sort();

    // Phase 2: parallel emission of per-country .fst/.bin/_strings.bin
    // triples. File writes are independent across countries; rayon
    // dispatches across the pool.
    if matches!(layout, Layout::PerCountry | Layout::Both) {
        let phase2 = Instant::now();
        ccs.par_iter()
            .filter_map(|cc| by_country.get(cc).map(|pc| (cc, pc)))
            .filter(|(_, pc)| !pc.entries.is_empty())
            .try_for_each(|(cc, pc)| emit_per_country(dir, cc, pc))?;
        eprintln!(
            "[stage] autocomplete_per_country_emit: {:.3}s",
            phase2.elapsed().as_secs_f64(),
        );
    }

    // Phase 3: unified FST emit. Sequential because it builds one
    // merged structure (re-interns strings into a shared pool, builds
    // one BTreeMap of `<cc><normalised_name>` → entry id). Bottleneck
    // here is the sequential intern + BTreeMap insert; parallelising
    // would need a concurrent re-intern which is more complexity than
    // the wall-time saving justifies (the unified emit is a small
    // fraction of total when phase 1 is parallel).
    if matches!(layout, Layout::Unified | Layout::Both) {
        let phase3 = Instant::now();
        emit_unified(dir, &ccs, &by_country)?;
        eprintln!(
            "[stage] autocomplete_unified_emit: {:.3}s",
            phase3.elapsed().as_secs_f64(),
        );
    }

    let mut stats = RunStats {
        country_count: ccs.len(),
        wof_postcodes_added,
        ..Default::default()
    };
    for pc in by_country.values() {
        stats.total_entries += pc.entries.len() as u64;
        stats.total_keys += pc.keys.len() as u64;
    }
    Ok(stats)
}

fn emit_per_country(dir: &std::path::Path, cc: &[u8; 2], pc: &PerCountry) -> Result<(), String> {
    let prefix = format!("fst_{}{}", cc[0] as char, cc[1] as char);
    let entries_path = dir.join(format!("{prefix}.bin"));
    let strings_path = dir.join(format!("{prefix}_strings.bin"));
    let fst_path = dir.join(format!("{prefix}.fst"));

    let entries_tmp = with_tmp_suffix(&entries_path);
    let strings_tmp = with_tmp_suffix(&strings_path);
    let fst_tmp = with_tmp_suffix(&fst_path);

    // Write all three staging files first; if any fails, remove whatever
    // we created and leave the previous set in place untouched.
    let write_result = (|| -> Result<(), String> {
        write_entries(&entries_tmp, &pc.entries)?;
        fs::write(&strings_tmp, &pc.strings)
            .map_err(|e| format!("write {}: {}", strings_tmp.display(), e))?;
        write_fst(&fst_tmp, pc.keys.iter().map(|(k, v)| (k.as_bytes(), *v)))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&entries_tmp);
        let _ = fs::remove_file(&strings_tmp);
        let _ = fs::remove_file(&fst_tmp);
        return Err(e);
    }

    // Rename into place. Order matters: rename the .fst last so that if a
    // crash splits the operation, a reader either (a) sees the old triple
    // (pre-first-rename) or (b) sees the new triple; never a valid .fst
    // pointing at stale .bin/.strings.
    fs::rename(&strings_tmp, &strings_path)
        .map_err(|e| format!("rename {}: {}", strings_path.display(), e))?;
    fs::rename(&entries_tmp, &entries_path)
        .map_err(|e| format!("rename {}: {}", entries_path.display(), e))?;
    fs::rename(&fst_tmp, &fst_path).map_err(|e| format!("rename {}: {}", fst_path.display(), e))?;

    eprintln!(
        "  {}: {} entries, {} keys, {} KB strings",
        prefix,
        pc.entries.len(),
        pc.keys.len(),
        pc.strings.len() / 1024,
    );
    Ok(())
}

fn emit_unified(
    dir: &std::path::Path,
    ccs: &[[u8; 2]],
    by_country: &HashMap<[u8; 2], PerCountry>,
) -> Result<(), String> {
    // Re-intern strings into one shared pool and build one array of
    // entries so the FST value is an index into a single, unified table.
    let mut unified_strings: Vec<u8> = vec![0];
    let mut intern_index: HashMap<String, u32> = HashMap::new();
    intern_index.insert(String::new(), 0);
    let mut intern = |s: &str, strings: &mut Vec<u8>| -> u32 {
        if let Some(&off) = intern_index.get(s) {
            return off;
        }
        let off = strings.len() as u32;
        strings.extend_from_slice(s.as_bytes());
        strings.push(0);
        intern_index.insert(s.to_owned(), off);
        off
    };

    // Country codes arrive pre-sorted from `run`, so entry IDs and
    // string-pool offsets are deterministic across rebuilds.
    let mut all_entries: Vec<AutocompleteEntry> = Vec::new();
    let mut keys: BTreeMap<Vec<u8>, u64> = BTreeMap::new();

    for cc in ccs {
        let Some(pc) = by_country.get(cc) else {
            continue;
        };
        for (key_str, per_country_id) in &pc.keys {
            let Some(src) = pc.entries.get(*per_country_id as usize) else {
                continue;
            };
            let name = read_cstr(&pc.strings, src.name_offset);
            let suburb = read_cstr(&pc.strings, src.suburb_offset);

            let new_entry = AutocompleteEntry {
                lat: src.lat,
                lng: src.lng,
                name_offset: intern(name, &mut unified_strings),
                suburb_offset: intern(suburb, &mut unified_strings),
                kind: src.kind,
                rank: src.rank,
                pad: [0; 2],
            };
            let unified_id = all_entries.len() as u64;
            all_entries.push(new_entry);

            let mut prefixed = Vec::with_capacity(2 + key_str.len());
            prefixed.push(cc[0].to_ascii_lowercase());
            prefixed.push(cc[1].to_ascii_lowercase());
            prefixed.extend_from_slice(key_str.as_bytes());
            // Country codes differ, so prefixed keys never collide across
            // countries; `insert` is safe.
            keys.insert(prefixed, unified_id);
        }
    }

    if keys.is_empty() {
        return Ok(());
    }

    let fst_path = dir.join("fst_unified.fst");
    let entries_path = dir.join("fst_unified.bin");
    let strings_path = dir.join("fst_unified_strings.bin");
    let fst_tmp = with_tmp_suffix(&fst_path);
    let entries_tmp = with_tmp_suffix(&entries_path);
    let strings_tmp = with_tmp_suffix(&strings_path);

    let write_result = (|| -> Result<(), String> {
        write_fst(&fst_tmp, keys.iter().map(|(k, v)| (k.as_slice(), *v)))?;
        write_entries(&entries_tmp, &all_entries)?;
        fs::write(&strings_tmp, &unified_strings)
            .map_err(|e| format!("write {}: {}", strings_tmp.display(), e))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&fst_tmp);
        let _ = fs::remove_file(&entries_tmp);
        let _ = fs::remove_file(&strings_tmp);
        return Err(e);
    }

    fs::rename(&strings_tmp, &strings_path)
        .map_err(|e| format!("rename {}: {}", strings_path.display(), e))?;
    fs::rename(&entries_tmp, &entries_path)
        .map_err(|e| format!("rename {}: {}", entries_path.display(), e))?;
    fs::rename(&fst_tmp, &fst_path).map_err(|e| format!("rename {}: {}", fst_path.display(), e))?;

    eprintln!(
        "fst_unified: {} entries across {} countries, {} keys, {} KB strings",
        all_entries.len(),
        ccs.len(),
        keys.len(),
        unified_strings.len() / 1024,
    );
    Ok(())
}

/// Append `.tmp` to a path's file name. Preserves the original stem + ext
/// so operators can tell the staging file apart at a glance.
fn with_tmp_suffix(p: &std::path::Path) -> PathBuf {
    let mut tmp = p.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

fn write_entries(path: &std::path::Path, entries: &[AutocompleteEntry]) -> Result<(), String> {
    let mut f = BufWriter::new(
        File::create(path).map_err(|e| format!("create {}: {}", path.display(), e))?,
    );
    for entry in entries {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                entry as *const AutocompleteEntry as *const u8,
                std::mem::size_of::<AutocompleteEntry>(),
            )
        };
        f.write_all(bytes)
            .map_err(|e| format!("write {}: {}", path.display(), e))?;
    }
    f.flush()
        .map_err(|e| format!("flush {}: {}", path.display(), e))?;
    Ok(())
}

fn write_fst<'a, I>(path: &std::path::Path, entries: I) -> Result<(), String>
where
    I: IntoIterator<Item = (&'a [u8], u64)>,
{
    let fst_file = File::create(path).map_err(|e| format!("create {}: {}", path.display(), e))?;
    let mut builder =
        MapBuilder::new(BufWriter::new(fst_file)).map_err(|e| format!("fst builder: {e}"))?;
    for (key, id) in entries {
        builder
            .insert(key, id)
            .map_err(|e| format!("fst insert {:?}: {e}", key))?;
    }
    builder.finish().map_err(|e| format!("fst finish: {e}"))?;
    Ok(())
}

/// Read a NUL-terminated string from a string pool at the given offset.
fn read_cstr(pool: &[u8], offset: u32) -> &str {
    let off = offset as usize;
    let bytes = pool.get(off..).unwrap_or(&[]);
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

/// Build-time FST key normaliser. Identical to the runtime
/// `query_server::autocomplete::normalise_prefix` — pinned in
/// `tests/abbrev_symmetry.rs` so the two can't drift. Wraps it
/// directly so the build pipeline always picks up runtime fixes
/// (e.g., the uppercase-diacritic fold) without a second
/// implementation to keep in sync.
fn normalise_fst_key(s: &str) -> String {
    query_server::autocomplete::normalise_prefix(s)
}

#[cfg(test)]
mod article_strip_tests {
    use super::strip_leading_article;

    #[test]
    fn galician_a_fonsagrada() {
        assert_eq!(
            strip_leading_article("A Fonsagrada"),
            Some("Fonsagrada".into())
        );
        assert_eq!(strip_leading_article("A Coruña"), Some("Coruña".into()));
    }

    #[test]
    fn english_the_hague() {
        assert_eq!(strip_leading_article("The Hague"), Some("Hague".into()));
        assert_eq!(strip_leading_article("the Bronx"), Some("Bronx".into()));
    }

    #[test]
    fn french_le_havre_les_baux() {
        assert_eq!(strip_leading_article("Le Havre"), Some("Havre".into()));
        assert_eq!(
            strip_leading_article("La Rochelle"),
            Some("Rochelle".into())
        );
        assert_eq!(
            strip_leading_article("Les Baux-de-Provence"),
            Some("Baux-de-Provence".into())
        );
    }

    #[test]
    fn spanish_el_la_los_las() {
        assert_eq!(
            strip_leading_article("El Escorial"),
            Some("Escorial".into())
        );
        assert_eq!(strip_leading_article("La Coruña"), Some("Coruña".into()));
        assert_eq!(strip_leading_article("Los Angeles"), Some("Angeles".into()));
        assert_eq!(strip_leading_article("Las Vegas"), Some("Vegas".into()));
    }

    #[test]
    fn italian_apostrophe_elision() {
        assert_eq!(strip_leading_article("L'Aquila"), Some("Aquila".into()));
        assert_eq!(
            strip_leading_article("L\u{2019}Aigle"),
            Some("Aigle".into())
        ); // Unicode apostrophe
    }

    #[test]
    fn german_dutch_welsh() {
        assert_eq!(strip_leading_article("Der Spiegel"), Some("Spiegel".into()));
        assert_eq!(strip_leading_article("De Wolden"), Some("Wolden".into()));
        assert_eq!(strip_leading_article("Y Fenni"), Some("Fenni".into()));
        assert_eq!(
            strip_leading_article("Yr Wyddgrug"),
            Some("Wyddgrug".into())
        );
    }

    #[test]
    fn no_article_no_strip() {
        assert_eq!(strip_leading_article("Madrid"), None);
        assert_eq!(strip_leading_article("Berlin"), None);
        assert_eq!(strip_leading_article("Paris"), None);
        // Single-token names where the only token happens to be an
        // article are also returned as None — there's nothing to strip.
        assert_eq!(strip_leading_article("La"), None);
        assert_eq!(strip_leading_article(""), None);
    }

    #[test]
    fn case_insensitive_match() {
        assert_eq!(strip_leading_article("LA Habana"), Some("Habana".into()));
        assert_eq!(strip_leading_article("THE Bronx"), Some("Bronx".into()));
    }
}
