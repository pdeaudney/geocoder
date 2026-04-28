//! Forward geocoding: text → coordinates. Built on tantivy (Lucene-in-Rust).
//!
//! The tantivy index carries per-document admin context (suburb/city, state,
//! country code) gathered at build time by running a reverse-geocoding
//! lookup at each street's / place's coordinate. That lets the query side
//! match across multiple fields — a user typing "alysse close baulkham
//! hills nsw" finds the street whose registered suburb is Baulkham Hills,
//! not just any document that happens to mention one of the tokens.
//!
//! ## Scope
//!
//! Indexes two sources from the reverse-geocoding binary index:
//! - **place points** (city/town/village/suburb/hamlet) from `place_points.bin`
//! - **streets** (way names with a node centroid) from `street_ways.bin` +
//!   `street_nodes.bin`, enriched with the suburb they sit in.
//!
//! Admin polygons (states, countries) aren't indexed as hit candidates
//! themselves — their geometry isn't useful for returning a single
//! coordinate. They serve only as enrichment context for streets/places.

use crate::{
    as_typed_slice, Index, NodeCoord, PlacePoint, WayHeader, DEFAULT_ADMIN_CELL_LEVEL,
    DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, Value, FAST, INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{
    doc, Index as TIndex, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term,
};

pub const KIND_PLACE: u64 = 1;
pub const KIND_STREET: u64 = 2;

/// Schema handle — kept together so build + query code agree on field ids.
pub struct ForwardSchema {
    pub schema: Schema,
    pub name: Field,
    pub name_raw: Field,
    pub suburb: Field,
    pub state: Field,
    pub country_code: Field,
    pub kind: Field,
    pub rank: Field,
    pub lat: Field,
    pub lng: Field,
}

/// Name of the tokenizer we register on the tantivy index. Inline
/// unicode folding + lowercase so query/index tokens match even across
/// diacritic variants ("Zürich" ≡ "Zurich", "São Paulo" ≡ "Sao Paulo").
/// Must match between build time and query time — register the same
/// pipeline on any `TIndex` we open.
const TOKENIZER_NAME: &str = "geocoder";

fn register_tokenizer(idx: &TIndex) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(AsciiFoldingFilter)
        .filter(LowerCaser)
        .build();
    idx.tokenizers().register(TOKENIZER_NAME, analyzer);
}

// --- Abbreviation expansion ---
//
// Small, conservative table keyed by the abbreviation (lowercased, post-ASCII-
// fold). Only unambiguous road-type abbreviations go here — "St" stays out
// because it can mean Street or Saint. We canonicalise to the long form at
// both index time (via `normalize_street_text` pre-processing the source) and
// query time (via `canonicalise_token`). That way "Albert Hwy" and
// "Albert Highway" produce the same token set `{albert, highway}`.
//
// Based on AU/UK/US common street-type abbreviations, deliberately leaving
// out country-specific rarities. Expanding the table later is safe — it's
// additive, but requires re-running `build-forward-index` so stored docs
// pick up the new canonicalisations.
const STREET_TYPE_ABBREVIATIONS: &[(&str, &str)] = &[
    ("hwy", "highway"),
    ("pde", "parade"),
    ("tce", "terrace"),
    ("ter", "terrace"),
    ("cres", "crescent"),
    ("blvd", "boulevard"),
    ("blv", "boulevard"),
    ("bvd", "boulevard"),
    ("ln", "lane"),
    ("ave", "avenue"),
    ("av", "avenue"),
    ("rd", "road"),
    ("dr", "drive"),
    ("ct", "court"),
    ("cl", "close"),
    ("pl", "place"),
];

fn canonicalise_token(token: &str) -> &str {
    for (abbr, full) in STREET_TYPE_ABBREVIATIONS {
        if token == *abbr {
            return full;
        }
    }
    token
}

impl ForwardSchema {
    pub fn build() -> Self {
        use tantivy::schema::{TextFieldIndexing, TextOptions};
        let mut schema = Schema::builder();

        let text_indexed = TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER_NAME)
            .set_index_option(IndexRecordOption::WithFreqs);
        let text_opts = TextOptions::default()
            .set_indexing_options(text_indexed.clone())
            .set_stored();

        let name = schema.add_text_field("name", text_opts.clone());
        let name_raw = schema.add_text_field("name_raw", STRING | STORED);
        // Enrichment fields — tokenized so "baulkham hills" matches both tokens.
        let suburb = schema.add_text_field("suburb", text_opts.clone());
        let state = schema.add_text_field("state", text_opts);
        let country_code = schema.add_text_field("country_code", STRING | STORED);
        let kind = schema.add_u64_field("kind", INDEXED | FAST | STORED);
        let rank = schema.add_u64_field("rank", FAST | STORED);
        let lat = schema.add_f64_field("lat", STORED | FAST);
        let lng = schema.add_f64_field("lng", STORED | FAST);

        ForwardSchema {
            schema: schema.build(),
            name,
            name_raw,
            suburb,
            state,
            country_code,
            kind,
            rank,
            lat,
            lng,
        }
    }
}

// --- Build path ---

#[derive(Default, Debug, Clone, Copy)]
pub struct BuildStats {
    pub places: usize,
    pub streets: usize,
}

/// Build a single monolithic tantivy index at `dest`. Everything goes in
/// one bucket — queries are filtered by the indexed `country_code` field
/// rather than dispatched to a different tantivy per country.
///
/// Use this when a deployment only serves one or two countries. For
/// worldwide / many-country deployments, `build_partitioned` produces
/// per-country indexes that query faster (smaller term dicts, tighter
/// BM25) — see `tantivy_<cc>/` layout consumed by `Forward::open`.
pub fn build(source: &Path, dest: &Path) -> Result<BuildStats, String> {
    build_with_heap(source, dest, default_heap_bytes())
}

/// Default tantivy `IndexWriter` heap budget. 512 MB is sized to keep
/// AU's ~100 MB tantivy dataset entirely in one in-memory segment,
/// with substantial headroom; planet operators should bump this via
/// `--tantivy-heap-mb` until the build's [stage] timing for the
/// commit phase stops dominating wallclock. See
/// docs/performance/tantivy-heap-2026-04-25.md for the analysis.
pub fn default_heap_bytes() -> usize {
    512 * 1024 * 1024
}

pub fn build_with_heap(source: &Path, dest: &Path, heap_bytes: usize) -> Result<BuildStats, String> {
    let source_str = source
        .to_str()
        .ok_or_else(|| format!("non-utf8 source path: {}", source.display()))?;
    let idx = Index::load(
        source_str,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )?;

    let schema_handle = ForwardSchema::build();

    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| format!("clear {}: {}", dest.display(), e))?;
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {}", dest.display(), e))?;

    let t_index = TIndex::create_in_dir(dest, schema_handle.schema.clone())
        .map_err(|e| format!("create tantivy index: {e}"))?;
    register_tokenizer(&t_index);

    let mut writer: IndexWriter = t_index
        .writer(heap_bytes)
        .map_err(|e| format!("tantivy writer: {e}"))?;

    let mut stats = BuildStats::default();

    // Places
    if let Some(pp) = idx.place_points.as_ref() {
        let points: &[PlacePoint] = as_typed_slice(pp);
        for (place_id, p) in points.iter().enumerate() {
            let name = idx.get_string(p.name_id);
            if name.is_empty() {
                continue;
            }
            let lat = p.lat as f64;
            let lng = p.lng as f64;
            let admin = idx.find_admin(lat, lng);

            // Gather `name:xx` alternates so an English query for
            // `Cologne` finds the Köln entry (canonical name unchanged
            // — the alternates are appended to the indexed `name`
            // field only). entity_type=1, entity_id=index in
            // place_points (matches the C++ builder; see
            // builder/src/build_index.cpp:633).
            let alternates: Vec<&str> = idx
                .i18n_names
                .as_ref()
                .map(|i| {
                    i.alternates_for(crate::i18n::ENTITY_PLACE, place_id as u32)
                        .map(|(_, name_id)| idx.get_string(name_id))
                        .filter(|alt| !alt.is_empty() && *alt != name)
                        .collect()
                })
                .unwrap_or_default();

            writer
                .add_document(tantivy_doc(
                    &schema_handle,
                    name,
                    &alternates,
                    KIND_PLACE,
                    p.rank as u64,
                    lat,
                    lng,
                    admin.city,
                    admin.state,
                    admin.country_code,
                ))
                .map_err(|e| format!("index place: {e}"))?;
            stats.places += 1;
        }
    }

    // Streets
    let ways: &[WayHeader] = as_typed_slice(&idx.street_ways);
    let nodes: &[NodeCoord] = as_typed_slice(&idx.street_nodes);
    // Dedup by (name_id, enriched suburb) to keep distinct "Main Street"s in
    // different suburbs without inflating the index with a row per OSM way.
    let mut seen: std::collections::HashSet<(u32, String)> = std::collections::HashSet::new();
    for way in ways {
        let name = idx.get_string(way.name_id);
        if name.is_empty() {
            continue;
        }
        let offset = way.node_offset as usize;
        let count = way.node_count as usize;
        if count == 0 || offset + count > nodes.len() {
            continue;
        }
        let mid = nodes[offset + count / 2];
        let lat = mid.lat as f64;
        let lng = mid.lng as f64;
        let admin = idx.find_admin(lat, lng);
        let suburb_key = admin.city.unwrap_or("").to_string();
        if !seen.insert((way.name_id, suburb_key)) {
            continue;
        }

        // The C++ builder doesn't emit i18n_names entries for streets,
        // so there are no alternates to feed in here.
        writer
            .add_document(tantivy_doc(
                &schema_handle,
                name,
                &[],
                KIND_STREET,
                26,
                lat,
                lng,
                admin.city,
                admin.state,
                admin.country_code,
            ))
            .map_err(|e| format!("index street: {e}"))?;
        stats.streets += 1;
    }

    writer
        .commit()
        .map_err(|e| format!("tantivy commit: {e}"))?;
    Ok(stats)
}

/// Build per-country tantivy indexes at `<dest_root>/tantivy_<cc>/`. Docs
/// whose enriched `country_code` is empty (offshore, unknown) are dropped
/// — they wouldn't be reachable by any meaningful forward query anyway.
///
/// For a worldwide deployment this splits ~2 GB of tantivy into ~60
/// smaller per-country indexes (AU ~42 MB, US ~300 MB, etc.), letting a
/// deployment that only serves N countries mount only their N indexes
/// and skip the rest. Queries with a country filter dispatch directly
/// to the right index — smaller term dictionaries, accurate BM25 IDF.
///
/// Returns per-country stats keyed by the ISO alpha-2 code.
pub fn build_partitioned(
    source: &Path,
    dest_root: &Path,
) -> Result<std::collections::HashMap<[u8; 2], BuildStats>, String> {
    build_partitioned_with_heap(source, dest_root, default_heap_bytes())
}

pub fn build_partitioned_with_heap(
    source: &Path,
    dest_root: &Path,
    heap_bytes: usize,
) -> Result<std::collections::HashMap<[u8; 2], BuildStats>, String> {
    use rayon::prelude::*;
    use std::collections::HashMap as Map;
    use std::time::Instant;

    let source_str = source
        .to_str()
        .ok_or_else(|| format!("non-utf8 source path: {}", source.display()))?;
    let idx = Index::load(
        source_str,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )?;
    let schema_handle = ForwardSchema::build();

    std::fs::create_dir_all(dest_root)
        .map_err(|e| format!("mkdir {}: {}", dest_root.display(), e))?;

    // Phase 1: classify all docs into per-country buckets. The expensive
    // step here is `idx.find_admin(lat, lng)` (point-in-polygon test);
    // PendingDoc borrows `&str` from the loaded Index so the intermediate
    // is cheap — just (lat, lng, refs) × N docs.
    //
    // Streets dedup happens here so each country bucket is already
    // distinct on (name_id, suburb, cc) before we hand it to tantivy.
    // Same dedup key as the previous serial implementation; behaviour
    // is preserved.
    struct PendingDoc<'a> {
        name: &'a str,
        /// `name:xx` alternates from i18n_names.bin. Empty for streets
        /// (the C++ builder doesn't emit street entries) and for
        /// places without name:xx tags. Concatenated into the indexed
        /// `name` field at writer time so a query in any tagged
        /// language matches.
        alternates: Vec<&'a str>,
        kind: u64,
        rank: u64,
        lat: f64,
        lng: f64,
        suburb: Option<&'a str>,
        state: Option<&'a str>,
        country_code: [u8; 2],
    }

    // Helper: resolve a doc's country code from find_admin-derived bytes.
    // Drops docs without a country (offshore, unknown, or level-2 admin
    // that didn't store a country_code).
    let country_bytes = |raw: Option<[u8; 2]>| -> Option<[u8; 2]> {
        let cc = raw?;
        if cc[0] == 0 || cc[1] == 0 {
            return None;
        }
        Some(cc)
    };

    let phase1 = Instant::now();
    eprintln!(
        "[stage] forward_classify: starting (rayon threads = {}, RAYON_NUM_THREADS = {:?})",
        rayon::current_num_threads(),
        std::env::var("RAYON_NUM_THREADS").ok(),
    );
    let mut buckets: Map<[u8; 2], Vec<PendingDoc<'_>>> = Map::new();

    // Places. par_iter parallelises the per-doc find_admin (the
    // expensive point-in-polygon step) across rayon's thread pool;
    // the post-collect bucketing is cheap.
    if let Some(pp) = idx.place_points.as_ref() {
        let points: &[PlacePoint] = as_typed_slice(pp);
        let place_candidates: Vec<([u8; 2], PendingDoc<'_>)> = points
            .par_iter()
            .enumerate()
            .filter_map(|(place_id, p)| {
                let name = idx.get_string(p.name_id);
                if name.is_empty() {
                    return None;
                }
                let lat = p.lat as f64;
                let lng = p.lng as f64;
                let admin = idx.find_admin(lat, lng);
                let cc = country_bytes(admin.country_code)?;

                // entity_type=1 for place points; entity_id matches
                // the slice index into place_points.bin (the C++
                // builder uses `place_points.size()` as the id pre-
                // push; see builder/src/build_index.cpp:633).
                let alternates: Vec<&str> = idx
                    .i18n_names
                    .as_ref()
                    .map(|i| {
                        i.alternates_for(crate::i18n::ENTITY_PLACE, place_id as u32)
                            .map(|(_, name_id)| idx.get_string(name_id))
                            .filter(|alt| !alt.is_empty() && *alt != name)
                            .collect()
                    })
                    .unwrap_or_default();

                Some((
                    cc,
                    PendingDoc {
                        name,
                        alternates,
                        kind: KIND_PLACE,
                        rank: p.rank as u64,
                        lat,
                        lng,
                        suburb: admin.city,
                        state: admin.state,
                        country_code: cc,
                    },
                ))
            })
            .collect();
        for (cc, doc) in place_candidates {
            buckets.entry(cc).or_default().push(doc);
        }
    }

    // Streets — same pattern, then a sequential dedup-and-bucket pass
    // on the results. Materialising the intermediate Vec is the price
    // of doing the dedup correctly across all threads (per-thread
    // local dedup would let cross-thread duplicates survive merge).
    // For planet that's ~48M candidates × ~80 bytes ≈ ~3.8 GB peak;
    // the find_admin parallelism more than pays for it.
    let ways: &[WayHeader] = as_typed_slice(&idx.street_ways);
    let nodes: &[NodeCoord] = as_typed_slice(&idx.street_nodes);
    let way_candidates: Vec<(u32, String, [u8; 2], PendingDoc<'_>)> = ways
        .par_iter()
        .filter_map(|way| {
            let name = idx.get_string(way.name_id);
            if name.is_empty() {
                return None;
            }
            let offset = way.node_offset as usize;
            let count = way.node_count as usize;
            if count == 0 || offset + count > nodes.len() {
                return None;
            }
            let mid = nodes[offset + count / 2];
            let lat = mid.lat as f64;
            let lng = mid.lng as f64;
            let admin = idx.find_admin(lat, lng);
            let cc = country_bytes(admin.country_code)?;
            let suburb_key = admin.city.unwrap_or("").to_string();
            Some((
                way.name_id,
                suburb_key,
                cc,
                PendingDoc {
                    name,
                    // No street entries in i18n_names.bin (the C++
                    // builder only emits admin polygons and place
                    // points), so nothing to add here.
                    alternates: Vec::new(),
                    kind: KIND_STREET,
                    rank: 26,
                    lat,
                    lng,
                    suburb: admin.city,
                    state: admin.state,
                    country_code: cc,
                },
            ))
        })
        .collect();

    let mut seen: std::collections::HashSet<(u32, String, [u8; 2])> =
        std::collections::HashSet::with_capacity(way_candidates.len());
    for (name_id, suburb, cc, doc) in way_candidates {
        if seen.insert((name_id, suburb, cc)) {
            buckets.entry(cc).or_default().push(doc);
        }
    }
    drop(seen);
    eprintln!(
        "[stage] forward_classify: {:.3}s ({} countries, {} docs)",
        phase1.elapsed().as_secs_f64(),
        buckets.len(),
        buckets.values().map(|v| v.len()).sum::<usize>(),
    );

    // Phase 2: parallel per-country tantivy builds. Each writer is
    // single-threaded internally — rayon supplies the cross-country
    // parallelism, so we don't oversubscribe with N×min(8, ncpus)
    // tantivy worker threads on top of N rayon workers.
    //
    // Writers hold `heap_bytes` each. Concurrent peak is bounded by
    // rayon's thread pool size (defaults to ncpus); operators on
    // memory-tight hosts can either lower `--tantivy-heap-mb` or set
    // `RAYON_NUM_THREADS=N` to reduce concurrent writer count.
    let phase2 = Instant::now();
    let dest_root_buf = dest_root.to_path_buf();
    let stats_out: Result<Map<[u8; 2], BuildStats>, String> = buckets
        .into_par_iter()
        .map(|(cc, docs)| -> Result<([u8; 2], BuildStats), String> {
            let cc_lower = [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()];
            let dir = dest_root_buf.join(format!(
                "tantivy_{}{}",
                cc_lower[0] as char, cc_lower[1] as char
            ));
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .map_err(|e| format!("clear {}: {}", dir.display(), e))?;
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("mkdir {}: {}", dir.display(), e))?;
            let t_index = TIndex::create_in_dir(&dir, schema_handle.schema.clone())
                .map_err(|e| format!("create tantivy {}: {}", dir.display(), e))?;
            register_tokenizer(&t_index);
            let mut writer = t_index
                .writer_with_num_threads(1, heap_bytes)
                .map_err(|e| format!("tantivy writer: {e}"))?;

            let mut stats = BuildStats::default();
            for d in &docs {
                writer
                    .add_document(tantivy_doc(
                        &schema_handle,
                        d.name,
                        &d.alternates,
                        d.kind,
                        d.rank,
                        d.lat,
                        d.lng,
                        d.suburb,
                        d.state,
                        Some(d.country_code),
                    ))
                    .map_err(|e| format!("index doc: {e}"))?;
                if d.kind == KIND_PLACE {
                    stats.places += 1;
                } else {
                    stats.streets += 1;
                }
            }
            writer.commit().map_err(|e| {
                format!("commit {}{}: {}", cc[0] as char, cc[1] as char, e)
            })?;
            Ok((cc, stats))
        })
        .collect();
    let stats_out = stats_out?;
    eprintln!(
        "[stage] forward_build_parallel: {:.3}s ({} countries built)",
        phase2.elapsed().as_secs_f64(),
        stats_out.len(),
    );
    Ok(stats_out)
}

/// Append ICU-derived Latin transliterations of `source` to
/// `name_indexed`, separated by single spaces. No-op when the
/// `translit` feature is disabled OR when `source` is already
/// Latin/ASCII. Whitespace-only ICU output is skipped.
#[cfg(feature = "translit")]
fn append_translit(name_indexed: &mut String, source: &str) {
    for latin in crate::translit::transliterate_for_index(source) {
        if latin.trim().is_empty() {
            continue;
        }
        if !name_indexed.is_empty() {
            name_indexed.push(' ');
        }
        name_indexed.push_str(latin.trim());
    }
}

#[cfg(not(feature = "translit"))]
fn append_translit(_name_indexed: &mut String, _source: &str) {}

fn tantivy_doc(
    s: &ForwardSchema,
    name: &str,
    alternates: &[&str],
    kind: u64,
    rank: u64,
    lat: f64,
    lng: f64,
    suburb: Option<&str>,
    state: Option<&str>,
    country_code: Option<[u8; 2]>,
) -> TantivyDocument {
    // Enrichment fields are genuinely optional per-document: a street in
    // Antarctica may have no suburb. We store "" for "unknown" and treat it
    // as absent on read. Country code from OSM is a 2-byte ASCII pair; if
    // it's not valid UTF-8 that's a bad string in the index, not a missing
    // value — keep empty rather than panic at build time.
    let cc = match country_code.map(|c| std::str::from_utf8(&c).map(str::to_owned)) {
        Some(Ok(code)) => code,
        _ => String::new(),
    };

    // `name_raw` preserves the original OSM name for display. `name` is
    // canonicalised so abbreviations in either the index or the query
    // converge on a single token form ("Smith Tce" ≡ "Smith Terrace").
    // i18n alternates are concatenated into the same field so a query
    // for `the hague` matches the Den Haag entry via the `hague` token,
    // courtesy of tantivy's SimpleTokenizer + AsciiFoldingFilter +
    // LowerCaser pipeline.
    let mut name_indexed = canonicalise_phrase(name);
    append_translit(&mut name_indexed, name);
    for alt in alternates {
        let canonical_alt = canonicalise_phrase(alt);
        if canonical_alt.trim().is_empty() {
            continue;
        }
        if !name_indexed.is_empty() {
            name_indexed.push(' ');
        }
        name_indexed.push_str(canonical_alt.trim());
        append_translit(&mut name_indexed, alt);
    }
    let suburb_indexed = suburb.map(canonicalise_phrase).unwrap_or_default();
    let state_indexed = state.map(canonicalise_phrase).unwrap_or_default();

    doc!(
        s.name => name_indexed,
        s.name_raw => name,
        s.suburb => suburb_indexed,
        s.state => state_indexed,
        s.country_code => cc,
        s.kind => kind,
        s.rank => rank,
        s.lat => lat,
        s.lng => lng,
    )
}

// --- Query path ---

/// A single opened tantivy index with its reader + schema handle.
struct FieldedIndex {
    index: TIndex,
    reader: IndexReader,
    schema: ForwardSchema,
    /// On-disk directory name (e.g. `tantivy`, `tantivy_au`). Recorded on
    /// every span that uses this index so an operator can correlate a
    /// suspect query to a specific file set without guessing which
    /// per-country shard answered it.
    dir_name: String,
}

/// Forward-geocoding dispatcher.
///
/// Holds up to two kinds of tantivy indexes, and picks between them per
/// query:
/// - `per_country[cc]` — built with `--partition-by-country`; each
///   country's corpus in its own tantivy segment so term frequencies and
///   BM25 scores don't mix across locales. Dispatched when a query has
///   a country filter.
/// - `default` — a monolithic worldwide tantivy (or a deployment that
///   only has one country's data). Serves queries without a country hint,
///   and is the fallback when no per-country index matches.
///
/// A deployment can hold either, both, or neither:
/// - AU-only dev setup: just `default` (populated by `build-forward-index
///   data/index`); no per-country segmentation needed.
/// - Worldwide production: per-country indexes for the 10-60 countries
///   served, plus optionally `default` for cross-country lookups.
pub struct Forward {
    per_country: std::collections::HashMap<[u8; 2], FieldedIndex>,
    default: Option<FieldedIndex>,
}

/// Structured forward-geocoding query. Any field can be `None`; the query
/// composer will AND the present ones with BM25 over the name + suburb +
/// state fields. `q` is an optional freeform token string bag used in
/// addition to the structured fields.
#[derive(Default, Debug, Clone)]
pub struct StructuredQuery<'a> {
    pub q: Option<&'a str>,
    pub street: Option<&'a str>,
    pub city: Option<&'a str>,
    pub state: Option<&'a str>,
    pub country_code: Option<&'a str>,
    pub kind: Option<u64>,
    pub limit: usize,
    /// Optional proximity bias for ambiguous-name disambiguation. When
    /// supplied, hits are ranked by `bm25 - α * log(distance_km + 1)`
    /// so geographically-close matches outrank far ones at similar
    /// BM25 scores. `Sydney` from a London bias still returns Sydney
    /// AU (BM25 dominates — only one global match); `Cambridge` from
    /// a Boston bias returns Cambridge MA before Cambridge UK.
    /// See `BiasCoord::DISTANCE_ALPHA` for the tuning constant.
    pub bias: Option<BiasCoord>,
}

/// Proximity-bias hint for `/search`. Validated at the API layer so
/// the search path can rely on these being in-range.
#[derive(Debug, Clone, Copy)]
pub struct BiasCoord {
    pub lat: f64,
    pub lng: f64,
}

impl BiasCoord {
    /// Tuning constant for `bm25 - α * log(distance_km + 1)`. Picked
    /// so ~100 km of distance counterbalances roughly 1 BM25 unit
    /// (typical BM25 score spread for a same-name multi-doc query is
    /// 1–3 units), enough to flip the ordering between same-name
    /// candidates without overriding obviously-better text matches.
    /// Empirically tuned in `tests/search_bias.rs`; raise the value
    /// to bias more aggressively, lower to soften.
    ///
    /// History: started at 0.10 (validated against AU St Kilda + UK
    /// Cambridge cases). Bumped to 0.20 after the planet bench-
    /// accuracy run with bias hints showed Aurora US, Montgomery US,
    /// Cornwall CA, Saint-Eustache CA, Greensboro US still picking
    /// the wrong same-name member. At α=0.10 the penalty delta
    /// between a 20 km hit and a 900 km hit is ~0.38 BM25 units,
    /// which the wrong-name BM25 advantage routinely exceeded. At
    /// α=0.20 the delta doubles to ~0.76, giving the bias enough
    /// headroom to flip those cases.
    pub const DISTANCE_ALPHA: f64 = 0.20;

    /// Validate range. Returns the coord on success, the offending
    /// field name on failure so the caller can build a 400/Status.
    pub fn try_new(lat: f64, lng: f64) -> Result<Self, &'static str> {
        if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
            return Err("bias_lat");
        }
        if !lng.is_finite() || !(-180.0..=180.0).contains(&lng) {
            return Err("bias_lng");
        }
        Ok(BiasCoord { lat, lng })
    }
}

impl Forward {
    /// Open whatever tantivy indexes are present at `dir`.
    ///
    /// `dir` can be either:
    /// - A **parent directory** (typically `data/index/`) containing
    ///   `tantivy/` and/or `tantivy_<cc>/` subdirectories. This is the
    ///   intended layout for deployments; `Forward::open` picks up both
    ///   monolithic and per-country indexes without the caller needing
    ///   to know which were built.
    /// - An **individual tantivy index directory** (contains `meta.json`).
    ///   Loaded as the monolithic `default`. Kept for backward
    ///   compatibility with callers that point directly at `tantivy/`.
    ///
    /// Returns `Err` only on I/O or malformed index errors. A directory
    /// with neither layout yields `Forward { per_country: {}, default: None }`,
    /// which callers can detect via `is_empty()` and surface as 501.
    pub fn open(dir: &Path) -> Result<Self, String> {
        // Detect the "path is the tantivy index itself" case by looking
        // for tantivy's meta.json marker. Tantivy writes this into every
        // index directory it creates.
        if dir.join("meta.json").exists() {
            return Ok(Forward {
                per_country: std::collections::HashMap::new(),
                default: Some(Self::open_one(dir)?),
            });
        }

        // Default tantivy/ load is best-effort. A common failure mode
        // is an empty/stale `tantivy/` directory left over from a
        // previous build (e.g. a `build-forward-index <dir>` that ran
        // before `--partition-by-country` was used). Propagating the
        // open error from THIS dir would also lose the per-country
        // dirs, which is the actual data the operator wants. Log
        // and skip instead.
        let default_path = dir.join("tantivy");
        let default = if default_path.exists() && default_path.is_dir() {
            match Self::open_one(&default_path) {
                Ok(idx) => Some(idx),
                Err(e) => {
                    tracing::warn!(
                        target: "query_server::forward",
                        path = %default_path.display(),
                        error = %e,
                        "default tantivy/ dir present but failed to open; \
                         continuing with per-country indexes only"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Per-country loop is also best-effort: a single broken
        // `tantivy_<cc>/` shouldn't take down /search for every other
        // country. Skip the broken one with a warning, keep the rest.
        let mut per_country: std::collections::HashMap<[u8; 2], FieldedIndex> =
            std::collections::HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_ascii_lowercase) else {
                    continue;
                };
                let Some(cc) = parse_tantivy_country_prefix(&name) else {
                    continue;
                };
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                match Self::open_one(&path) {
                    Ok(idx) => {
                        per_country.insert(cc, idx);
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "query_server::forward",
                            path = %path.display(),
                            error = %e,
                            cc = %String::from_utf8_lossy(&cc),
                            "per-country tantivy_<cc>/ dir failed to open; \
                             skipping (other countries still served)"
                        );
                    }
                }
            }
        }
        Ok(Forward { per_country, default })
    }

    /// Open a single tantivy index directory (shared between the monolithic
    /// `tantivy/` directory and each per-country `tantivy_<cc>/`).
    fn open_one(dir: &Path) -> Result<FieldedIndex, String> {
        let t_index = TIndex::open_in_dir(dir).map_err(|e| format!("open tantivy: {e}"))?;
        register_tokenizer(&t_index);
        let schema_raw = t_index.schema();
        let field = |name: &str| {
            schema_raw
                .get_field(name)
                .map_err(|e| format!("forward schema {name}: {e}"))
        };
        let schema = ForwardSchema {
            name: field("name")?,
            name_raw: field("name_raw")?,
            suburb: field("suburb")?,
            state: field("state")?,
            country_code: field("country_code")?,
            kind: field("kind")?,
            rank: field("rank")?,
            lat: field("lat")?,
            lng: field("lng")?,
            schema: schema_raw,
        };
        let reader = t_index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| format!("tantivy reader: {e}"))?;
        let dir_name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        log_loaded_dir(dir, &dir_name);
        Ok(FieldedIndex {
            index: t_index,
            reader,
            schema,
            dir_name,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.per_country.is_empty() && self.default.is_none()
    }

    pub fn countries(&self) -> impl Iterator<Item = &[u8; 2]> {
        self.per_country.keys()
    }

    /// Whether a monolithic (non-partitioned) fallback forward index is
    /// loaded. When true, the service can serve any country covered by
    /// the underlying build even if `countries()` is empty. Used by
    /// `/healthz/indexes` so routing layers can distinguish
    /// "per-country split loaded" from "only the default is loaded".
    pub fn has_default(&self) -> bool {
        self.default.is_some()
    }

    /// Pick the index to query: per-country if we have a loaded index
    /// matching the caller's country hint, else the monolithic default,
    /// else `None` and the caller returns empty.
    ///
    /// The returned variant tag (`per_country` / `default`) is recorded
    /// on the search span so an operator hunting a corrupt file knows
    /// whether the query hit `tantivy_<cc>/` or the monolithic `tantivy/`.
    fn pick<'s>(&'s self, country_code: Option<&str>) -> Option<(&'s FieldedIndex, &'static str)> {
        if let Some(cc) = country_code {
            if let Some(code) = parse_country_code(cc) {
                if let Some(idx) = self.per_country.get(&code) {
                    return Some((idx, "per_country"));
                }
                // Country-specific filter but no per-country index loaded.
                // Fall through to default; the filter clause inside the
                // query still keeps results scoped to that country via
                // the indexed `country_code` field.
            }
        }
        self.default.as_ref().map(|d| (d, "default"))
    }

    /// Legacy single-text search — kept for tests and simple cases.
    pub fn search(
        &self,
        query: &str,
        kind_filter: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Hit>, String> {
        self.search_structured(StructuredQuery {
            q: Some(query),
            kind: kind_filter,
            limit,
            ..Default::default()
        })
    }

    /// Resolve the underlying tantivy index for the first pick-able index
    /// matching the query. Used internally by search_structured; public
    /// mainly so tests can poke at it.
    fn active_index<'s>(&'s self, q: &StructuredQuery<'_>) -> Option<(&'s FieldedIndex, &'static str)> {
        self.pick(q.country_code)
    }

    /// Multi-field structured search. Tokens from `street`/`city`/`state`
    /// become required field-specific matches; tokens from `q` become
    /// should-match matches against name + suburb + state with name boosted.
    ///
    /// When the primary query returns zero hits, walks a Nominatim-style
    /// fallback ladder — drop the most specific constraint, retry — until
    /// either a hit comes back or every relaxation is exhausted. The ladder
    /// is: full query → drop country_code → drop state → drop city → drop
    /// kind. Each step is a one-line simpler query. Total fallback cost
    /// for a truly-nothing query: ~5 × one search ≈ 100 µs.
    #[tracing::instrument(
        name = "forward.search_structured",
        skip_all,
        fields(
            geocoder.q = q.q.unwrap_or(""),
            geocoder.street = q.street.unwrap_or(""),
            geocoder.city = q.city.unwrap_or(""),
            geocoder.state = q.state.unwrap_or(""),
            geocoder.country_code = q.country_code.unwrap_or(""),
            geocoder.kind = q.kind.unwrap_or(0),
            geocoder.limit = q.limit,
            geocoder.stage = tracing::field::Empty,
            geocoder.match_count = tracing::field::Empty,
            geocoder.forward.index_variant = tracing::field::Empty,
            geocoder.forward.tantivy_dir = tracing::field::Empty,
        )
    )]
    pub fn search_structured(&self, q: StructuredQuery<'_>) -> Result<Vec<Hit>, String> {
        let Some((active, variant)) = self.active_index(&q) else {
            tracing::Span::current().record("geocoder.stage", "no_index");
            tracing::Span::current().record("geocoder.match_count", 0);
            tracing::debug!(
                target: "query_server::forward",
                "no active index for query — empty result"
            );
            return Ok(Vec::new());
        };
        tracing::Span::current().record("geocoder.forward.index_variant", variant);
        tracing::Span::current()
            .record("geocoder.forward.tantivy_dir", active.dir_name.as_str());

        // First try: honour every constraint. If it hits, we're done.
        let strict_span = tracing::info_span!(
            target: "query_server::forward",
            "forward.ladder.rung",
            geocoder.stage = "strict",
            geocoder.forward.tantivy_dir = %active.dir_name,
        );
        let hits = strict_span.in_scope(|| self.search_once(active, &q))?;
        if !hits.is_empty() {
            tracing::Span::current().record("geocoder.stage", "strict");
            tracing::Span::current().record("geocoder.match_count", hits.len());
            return Ok(hits);
        }

        // Progressive relaxation — each step returns as soon as any hit
        // shows up, so we stop dropping constraints the moment the query
        // can resolve. Mirrors Nominatim's "multiple interpretations" idea.
        let ladder: &[(&'static str, fn(&mut StructuredQuery<'_>))] = &[
            ("drop_country_code", |q| q.country_code = None),
            ("drop_state", |q| q.state = None),
            ("drop_city", |q| q.city = None),
            ("drop_kind", |q| q.kind = None),
        ];
        let mut relaxed = q.clone();
        for (stage, relax) in ladder {
            relax(&mut relaxed);
            // Relaxing the country_code may flip us to a different index
            // (per-country → default). Re-pick each iteration so the
            // fallback actually gets a chance.
            let (active, variant) = match self.active_index(&relaxed) {
                Some(a) => a,
                None => continue,
            };
            let rung_span = tracing::info_span!(
                target: "query_server::forward",
                "forward.ladder.rung",
                geocoder.stage = stage,
                geocoder.forward.tantivy_dir = %active.dir_name,
                geocoder.forward.index_variant = variant,
            );
            let hits = rung_span.in_scope(|| self.search_once(active, &relaxed))?;
            if !hits.is_empty() {
                tracing::Span::current().record("geocoder.stage", *stage);
                tracing::Span::current().record("geocoder.match_count", hits.len());
                tracing::Span::current().record("geocoder.forward.index_variant", variant);
                tracing::Span::current()
                    .record("geocoder.forward.tantivy_dir", active.dir_name.as_str());
                return Ok(hits);
            }
        }

        // Last-resort typo tolerance — retry the freeform `q` with fuzzy
        // name matching (Levenshtein distance 1). Only fires after the
        // ladder has failed, so we never pay fuzzy's 2-3x cost on queries
        // the strict path already resolved.
        if let Some(q_text) = q.q.filter(|s| !s.trim().is_empty()) {
            if let Some((active, variant)) = self.active_index(&q) {
                let fuzzy_span = tracing::info_span!(
                    target: "query_server::forward",
                    "forward.ladder.rung",
                    geocoder.stage = "fuzzy",
                    geocoder.forward.tantivy_dir = %active.dir_name,
                    geocoder.forward.index_variant = variant,
                );
                if let Some(hits) = fuzzy_span.in_scope(|| {
                    self.search_fuzzy(active, q_text, q.kind, q.limit, q.bias.as_ref())
                })? {
                    if !hits.is_empty() {
                        tracing::Span::current().record("geocoder.stage", "fuzzy");
                        tracing::Span::current().record("geocoder.match_count", hits.len());
                        return Ok(hits);
                    }
                }
            }
        }

        tracing::Span::current().record("geocoder.stage", "exhausted");
        tracing::Span::current().record("geocoder.match_count", 0);
        Ok(Vec::new())
    }

    /// Fuzzy fallback: construct a BooleanQuery of FuzzyTermQueries (edit
    /// distance 1) over the parsed tokens. Fires only when the strict
    /// ladder found nothing — typo tolerance isn't free (~2-3x tantivy
    /// cost) and we'd rather not blur scoring on queries that matched.
    #[tracing::instrument(
        name = "forward.search_fuzzy",
        skip_all,
        fields(
            geocoder.q = q_text,
            geocoder.kind = kind_filter.unwrap_or(0),
            geocoder.limit = limit,
            geocoder.match_count = tracing::field::Empty,
        )
    )]
    fn search_fuzzy(
        &self,
        active: &FieldedIndex,
        q_text: &str,
        kind_filter: Option<u64>,
        limit: usize,
        bias: Option<&BiasCoord>,
    ) -> Result<Option<Vec<Hit>>, String> {
        let parsed = parse_freeform_query(q_text);
        if parsed.rest.is_empty() {
            return Ok(None);
        }
        let s = &active.schema;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for tok in &parsed.rest {
            // Edit distance 1 is the sweet spot: catches most single-char
            // typos ("sidny"→"sydney") without blurring into false
            // positives (distance 2 matches anything remotely similar).
            let fuzzy = FuzzyTermQuery::new(
                Term::from_field_text(s.name, tok),
                1,
                true,
            );
            clauses.push((Occur::Must, Box::new(fuzzy)));
        }
        if let Some(k) = kind_filter {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(s.kind, k),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        let boolean = BooleanQuery::new(clauses);
        let limit = limit.max(1).min(50);
        let searcher = active.reader.searcher();
        let top = searcher
            .search(&boolean, &TopDocs::with_limit(limit * 3))
            .map_err(|e| format!("fuzzy search: {e}"))?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher
                .doc(addr)
                .map_err(|e| format!("fetch doc: {e}"))?;
            hits.push(hit_from_doc(&doc, s, score)?);
        }
        // Re-rank with the prominence boost + optional proximity bias,
        // same as the strict path.
        hits.sort_by(|a, b| {
            boosted_score(b, bias)
                .partial_cmp(&boosted_score(a, bias))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        tracing::Span::current().record("geocoder.match_count", hits.len());
        Ok(Some(hits))
    }

    #[tracing::instrument(
        name = "forward.search_once",
        skip_all,
        fields(
            geocoder.match_count = tracing::field::Empty,
            geocoder.top_score = tracing::field::Empty,
        )
    )]
    fn search_once(
        &self,
        active: &FieldedIndex,
        q: &StructuredQuery<'_>,
    ) -> Result<Vec<Hit>, String> {
        let searcher = active.reader.searcher();
        let s = &active.schema;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        // Structured fields: MUST match each requested token. Using
        // FuzzyTermQuery with distance 0 to get analyzer-aware matching
        // (lowercasing) for free; distance 1 could be added for typo
        // tolerance once we're confident in recall.
        if let Some(street) = q.street.filter(|s| !s.trim().is_empty()) {
            for tok in tokenize_user_input(street) {
                let tq = TermQuery::new(
                    Term::from_field_text(s.name, &tok),
                    IndexRecordOption::WithFreqs,
                );
                clauses.push((Occur::Must, Box::new(tq)));
            }
        }
        if let Some(city) = q.city.filter(|s| !s.trim().is_empty()) {
            for tok in tokenize_user_input(city) {
                let tq = TermQuery::new(
                    Term::from_field_text(s.suburb, &tok),
                    IndexRecordOption::WithFreqs,
                );
                clauses.push((Occur::Must, Box::new(tq)));
            }
        }
        if let Some(state) = q.state.filter(|s| !s.trim().is_empty()) {
            for tok in tokenize_user_input(state) {
                let tq = TermQuery::new(
                    Term::from_field_text(s.state, &tok),
                    IndexRecordOption::WithFreqs,
                );
                clauses.push((Occur::Must, Box::new(tq)));
            }
        }
        if let Some(cc) = q.country_code.filter(|s| !s.trim().is_empty()) {
            let tq = TermQuery::new(
                Term::from_field_text(s.country_code, &cc.to_ascii_uppercase()),
                IndexRecordOption::Basic,
            );
            clauses.push((Occur::Must, Box::new(tq)));
        }

        // Freeform `q=` bag: first pre-parse to extract known hints
        // (house_number, state abbreviation, postcode), then every remaining
        // token must match at least one of name/suburb/state. State hints
        // from the parse are additive on top of any structured `state`.
        if let Some(q_text) = q.q.filter(|s| !s.trim().is_empty()) {
            let parsed = parse_freeform_query(q_text);

            // State extracted from freeform is only applied when the caller
            // didn't provide a structured state; otherwise structured wins.
            if q.state.map(str::trim).filter(|s| !s.is_empty()).is_none() {
                if let Some(state) = parsed.state.as_deref() {
                    for tok in tokenize_user_input(state) {
                        clauses.push((
                            Occur::Must,
                            Box::new(TermQuery::new(
                                Term::from_field_text(s.state, &tok),
                                IndexRecordOption::WithFreqs,
                            )),
                        ));
                    }
                }
            }

            for tok in &parsed.rest {
                // Boost name-field matches well above suburb-field matches.
                // A doc whose `name` literally contains "elizabeth" and
                // "street" outranks a street in a suburb called "Elizabeth".
                // Without this, per-token should-clauses spread BM25 mass
                // evenly across fields and mislocate the intent. Rough
                // factor: streets named X should clearly beat streets in
                // suburb X when X appears in the query.
                const NAME_BOOST: f32 = 3.0;
                let name_q: Box<dyn Query> = Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(s.name, tok),
                        IndexRecordOption::WithFreqs,
                    )),
                    NAME_BOOST,
                ));
                let suburb_q: Box<dyn Query> = Box::new(TermQuery::new(
                    Term::from_field_text(s.suburb, tok),
                    IndexRecordOption::WithFreqs,
                ));
                let tok_clauses: Vec<(Occur, Box<dyn Query>)> =
                    vec![(Occur::Should, name_q), (Occur::Should, suburb_q)];
                let token_q = BooleanQuery::new(tok_clauses);
                clauses.push((Occur::Must, Box::new(token_q)));
            }
        }

        if let Some(k) = q.kind {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(s.kind, k),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if clauses.is_empty() {
            return Ok(Vec::new());
        }

        let boolean = BooleanQuery::new(clauses);
        let limit = q.limit.max(1).min(50);
        // Over-fetch so we can re-rank without losing interesting
        // candidates that a pure BM25 sort would miss. Two regimes:
        //
        //   No bias: re-rank only applies the prominence boost
        //     (multiplicative; rarely changes top-N membership). 3×
        //     the requested limit (cap 150) is plenty.
        //
        //   With bias: re-rank applies a distance penalty that CAN
        //     pull a far-by-BM25 doc above closer-but-different
        //     same-name members. Same-name clusters routinely have
        //     10-30+ same-token docs in a single country (`Aurora`
        //     in the US has Aurora CO/IL/IN/IA/MO/NE/NY/NC/OH/OR/TX/
        //     UT plus streets and parks; same shape for `Cornwall`,
        //     `Montgomery`, `Greensboro`, `Springfield`). If the
        //     RIGHT same-name member sits at BM25 rank 30+, a
        //     30-candidate pool excludes it before bias runs and
        //     re-ranking can't recover it. Bump the pool to 500
        //     (15× headroom) so bias has enough material to flip
        //     the obvious cases. Cost is one extra Tantivy query
        //     pass over a larger TopDocs heap — adds <1 ms at
        //     planet scale, paid only on bias-enabled queries.
        let oversample = if q.bias.is_some() {
            (limit * 30).min(500)
        } else {
            (limit * 3).min(150)
        };
        let top = searcher
            .search(&boolean, &TopDocs::with_limit(oversample))
            .map_err(|e| format!("search: {e}"))?;

        let mut candidates: Vec<Hit> = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher
                .doc(addr)
                .map_err(|e| format!("fetch doc: {e}"))?;
            candidates.push(hit_from_doc(&doc, s, score)?);
        }

        // Nominatim-style prominence boost: documents with a lower rank
        // (larger/more prominent feature) outrank equally-matching
        // narrower ones. A query for "Sydney" returns the city, not a
        // street named "Sydney Lane"; "Alysse Close" in Baulkham Hills
        // still wins against a generic place match because the BM25
        // component dominates the boost.
        let bias = q.bias.as_ref();
        candidates.sort_by(|a, b| {
            let score_a = boosted_score(a, bias);
            let score_b = boosted_score(b, bias);
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(limit);

        let span = tracing::Span::current();
        span.record("geocoder.match_count", candidates.len());
        if let Some(top) = candidates.first() {
            span.record("geocoder.top_score", top.score);
        }

        Ok(candidates)
    }

    /// Returns the default tantivy index if one is loaded, otherwise the
    /// first per-country index (iteration order is arbitrary). Primarily
    /// exists for tests that want to introspect the raw tantivy state.
    pub fn any_index(&self) -> Option<&TIndex> {
        self.default
            .as_ref()
            .or_else(|| self.per_country.values().next())
            .map(|f| &f.index)
    }
}

/// Log a structured manifest line for a tantivy directory we just opened.
/// One line per directory, fields: dir_name, path, size_bytes (sum of
/// every file under the dir), file_count, mtime. Operators compare
/// against an expected manifest to spot truncated / wrong-version files.
fn log_loaded_dir(path: &Path, dir_name: &str) {
    let (size_bytes, file_count, mtime_secs) = match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut bytes: u64 = 0;
            let mut count: u64 = 0;
            let mut newest: Option<std::time::SystemTime> = None;
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                bytes += meta.len();
                count += 1;
                if let Ok(m) = meta.modified() {
                    newest = Some(newest.map_or(m, |n| n.max(m)));
                }
            }
            let mtime = newest
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (bytes, count, mtime)
        }
        Err(_) => (0, 0, 0),
    };
    tracing::info!(
        target: "query_server::manifest",
        index = "forward",
        dir_name = dir_name,
        path = %path.display(),
        size_bytes,
        file_count,
        mtime_unix = mtime_secs,
        "loaded forward index directory"
    );
}

/// Parse a free-form country-code string (e.g. from the URL) into the
/// lower-case 2-byte form used as the per-country index key.
fn parse_country_code(cc: &str) -> Option<[u8; 2]> {
    let trimmed = cc.trim();
    let b = trimmed.as_bytes();
    if b.len() != 2 || !b[0].is_ascii_alphabetic() || !b[1].is_ascii_alphabetic() {
        return None;
    }
    Some([b[0].to_ascii_lowercase(), b[1].to_ascii_lowercase()])
}

/// Match directory names of the form `tantivy_<cc>`.
fn parse_tantivy_country_prefix(name: &str) -> Option<[u8; 2]> {
    let suffix = name.strip_prefix("tantivy_")?;
    parse_country_code(suffix)
}

/// Combined score for ranking: BM25 × prominence factor, where a
/// rank-16 place (city/town/village) gets ~1.4×, a rank-19 suburb ~1.15×,
/// a rank-26 street ~1.0×. The curve is gentle on purpose — strong text
/// matches still beat weak ones regardless of rank. Exponent chosen
/// empirically so "Sydney" ranks the city above streets named Sydney, but
/// "Alysse Close Baulkham Hills" still prefers the highly-specific street.
fn boosted_score(hit: &Hit, bias: Option<&BiasCoord>) -> f32 {
    match bias {
        None => {
            // Nominatim-style prominence boost: lower rank wins among
            // similar BM25 scores. A query for "Sydney" returns the
            // city, not "Sydney Lane".
            const BASELINE: f32 = 26.0;
            let delta = (BASELINE - hit.rank as f32) / 10.0;
            let boost = (1.0 + delta.max(0.0) * 0.4).clamp(1.0, 2.0);
            hit.score * boost
        }
        Some(b) => {
            // Proximity bias is the user's explicit intent: rank by
            // BM25 + geographic closeness, NOT global admin
            // prominence. Skipping the prominence boost here is the
            // mechanism that lets a Melbourne user searching
            // "St Kilda" see Melbourne's suburb (rank 19) above SA's
            // admin centre (rank 16). The distance penalty is
            // additive on the f32 BM25 score: ~0.5 units per ln-step
            // of km. A 100 km miss costs ~0.46; a 10000 km miss
            // ~0.92.
            let d_km = crate::geo::haversine_m(hit.lat, hit.lng, b.lat, b.lng) / 1_000.0;
            let penalty = (BiasCoord::DISTANCE_ALPHA as f32) * (d_km as f32 + 1.0).ln();
            hit.score - penalty
        }
    }
}

/// Pull a required string field off a tantivy doc. Returns an `Err` instead
/// of silently defaulting to "" — a missing field means our index is out of
/// sync with the schema, which the caller should surface loudly.
fn required_str(doc: &TantivyDocument, field: Field, label: &str) -> Result<String, String> {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("forward doc missing required field {label}"))
}

/// Pull an optional string field: present but empty is treated as "absent"
/// so enrichment fields (suburb/state) we stored as "" for unknown values
/// surface as `None`.
fn optional_str(doc: &TantivyDocument, field: Field) -> Option<String> {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn required_u64(doc: &TantivyDocument, field: Field, label: &str) -> Result<u64, String> {
    doc.get_first(field)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("forward doc missing required u64 field {label}"))
}

fn required_f64(doc: &TantivyDocument, field: Field, label: &str) -> Result<f64, String> {
    doc.get_first(field)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("forward doc missing required f64 field {label}"))
}

fn hit_from_doc(doc: &TantivyDocument, s: &ForwardSchema, score: f32) -> Result<Hit, String> {
    Ok(Hit {
        name: required_str(doc, s.name_raw, "name_raw")?,
        suburb: optional_str(doc, s.suburb),
        state: optional_str(doc, s.state),
        country_code: optional_str(doc, s.country_code),
        kind: required_u64(doc, s.kind, "kind")?,
        rank: required_u64(doc, s.rank, "rank")?,
        lat: required_f64(doc, s.lat, "lat")?,
        lng: required_f64(doc, s.lng, "lng")?,
        score,
    })
}

/// Split user input into lowercase tokens, stripping punctuation and
/// canonicalising known street-type abbreviations (Tce → terrace, Hwy →
/// highway, …) plus place-name abbreviations (Saint→st, Mount→mt,
/// Fort→ft) when leading. Mirrors the tokenisation applied at index
/// time so query tokens match index tokens exactly.
///
/// **Translit note (build-time only).** ICU transliteration is applied
/// at index build time (see `query_server::translit`), so a Russian
/// user typing Cyrillic and an English user typing Latin both find
/// the same place via stored alternate forms. The query path
/// deliberately does NOT call ICU — keeping the runtime libicu-free.
/// If a future product requirement needs query-time transliteration
/// (e.g. user types a Latin scheme that ICU's build-time output
/// didn't produce), wire it here as a Vec<String> expansion before
/// `apply_place_abbreviation_fold`.
pub fn tokenize_user_input(s: &str) -> Vec<String> {
    let folded = ascii_fold(s);
    let raw_tokens: Vec<String> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| canonicalise_token(&t.to_ascii_lowercase()).to_owned())
        .collect();
    apply_place_abbreviation_fold(raw_tokens)
}

/// Apply the same Saint/Mount/Fort fold as
/// `crate::autocomplete::fold_place_abbreviations`, but on a token Vec
/// (already lowercase, ASCII-folded). Position rule: only the
/// leading tokens of a multi-token phrase are mapped — trailing
/// tokens like the `St` in `Main St` (= Street, not Saint) stay
/// literal. See the symmetry test in `tests/abbrev_symmetry.rs`.
fn apply_place_abbreviation_fold(mut tokens: Vec<String>) -> Vec<String> {
    if tokens.len() < 2 {
        return tokens;
    }
    let last = tokens.len() - 1;
    for (i, t) in tokens.iter_mut().enumerate() {
        if i == last {
            continue;
        }
        let mapped = match t.as_str() {
            "saint" | "sainte" | "st" | "ste" => Some("st"),
            "mount" | "mt" => Some("mt"),
            "fort" | "ft" => Some("ft"),
            _ => None,
        };
        if let Some(m) = mapped {
            *t = m.to_owned();
        }
    }
    tokens
}

/// Apply the canonicalisation step at index time to a source name string.
/// Returns a new string where any whole-word abbreviation (case-insensitive,
/// trailing `.` tolerated) is replaced with its full form. Also collapses
/// the leading Saint/Mount/Fort place abbreviations (see
/// [`apply_place_abbreviation_fold`]) so the build-time tokenisation
/// matches the runtime tokenisation byte-for-byte after tantivy's
/// lowercase + fold filters. Non-matching words are preserved verbatim.
pub fn canonicalise_phrase(s: &str) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    let last = words.len() - 1;

    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for (i, word) in words.iter().enumerate() {
        if !first {
            out.push(' ');
        }
        first = false;

        // Match against abbreviation table using a trimmed, lowercased,
        // ASCII-folded view. Keep the original word if no match.
        let trimmed = word.trim_end_matches('.');
        let key_owned = ascii_fold(trimmed).to_ascii_lowercase();

        // Street-type abbreviation table (Tce → terrace, Hwy → highway, …).
        if let Some((_, full)) = STREET_TYPE_ABBREVIATIONS
            .iter()
            .find(|(abbr, _)| *abbr == key_owned.as_str())
        {
            out.push_str(full);
            continue;
        }

        // Place-name abbreviation table (Saint/Mount/Fort), leading-only.
        // Skip the last token so `Main St` (Street) stays literal; this
        // mirrors the position rule in `apply_place_abbreviation_fold`.
        if i != last {
            let mapped = match key_owned.as_str() {
                "saint" | "sainte" | "st" | "ste" => Some("st"),
                "mount" | "mt" => Some("mt"),
                "fort" | "ft" => Some("ft"),
                _ => None,
            };
            if let Some(m) = mapped {
                out.push_str(m);
                continue;
            }
        }

        out.push_str(word);
    }
    out
}

/// Minimal ASCII folding for text that will go through the tantivy
/// analyzer: maps the common Latin Extended characters (é/ü/ñ/ß/...) to
/// their unaccented ASCII equivalents. Keeps the behaviour of our
/// query-time tokenisation aligned with tantivy's `AsciiFoldingFilter` so
/// whatever matches an index token also matches when fed through
/// `tokenize_user_input`.
fn ascii_fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => out.push('a'),
            'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => out.push('A'),
            'é' | 'è' | 'ê' | 'ë' => out.push('e'),
            'É' | 'È' | 'Ê' | 'Ë' => out.push('E'),
            'í' | 'ì' | 'î' | 'ï' => out.push('i'),
            'Í' | 'Ì' | 'Î' | 'Ï' => out.push('I'),
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => out.push('o'),
            'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' | 'Ø' => out.push('O'),
            'ú' | 'ù' | 'û' | 'ü' => out.push('u'),
            'Ú' | 'Ù' | 'Û' | 'Ü' => out.push('U'),
            'ñ' => out.push('n'),
            'Ñ' => out.push('N'),
            'ç' => out.push('c'),
            'Ç' => out.push('C'),
            'ß' => out.push_str("ss"),
            _ => out.push(ch),
        }
    }
    out
}

/// Result of parsing a freeform query like `"10 alysse close baulkham hills nsw 2154"`.
#[derive(Debug, Default, Clone)]
pub struct ParsedQuery {
    pub house_number: Option<String>,
    /// State name (expanded from an abbreviation if recognised).
    pub state: Option<String>,
    /// Postcode as a 4-digit string. Not yet wired into search because our
    /// index has no postcode field, but carried through so the /search
    /// response can echo it and a future fix can use it as a filter.
    pub postcode: Option<String>,
    /// Remaining tokens, lowercased, with hints stripped. Fed to the
    /// `q` bag search that looks in name + suburb + state fields.
    pub rest: Vec<String>,
}

/// Map an AU state abbreviation (or full name) to its canonical form.
/// Returns `None` for tokens that don't look like a state hint.
fn canonicalise_state(token: &str) -> Option<&'static str> {
    match token.to_ascii_lowercase().as_str() {
        "nsw" | "new" => Some("New South Wales"), // "new" handles "new south wales" as a single token, cheap bias
        "vic" | "victoria" => Some("Victoria"),
        "qld" | "queensland" => Some("Queensland"),
        "wa" => Some("Western Australia"),
        "sa" => Some("South Australia"),
        "tas" | "tasmania" => Some("Tasmania"),
        "nt" => Some("Northern Territory"),
        "act" => Some("Australian Capital Territory"),
        _ => None,
    }
}

/// Parse a freeform query into structured parts. Pure string work — no
/// lookups. Conservative by design: if something doesn't look like a state
/// abbreviation / postcode / house number, it stays in `rest`.
pub fn parse_freeform_query(input: &str) -> ParsedQuery {
    let tokens = tokenize_user_input(input);
    let mut parsed = ParsedQuery::default();
    let mut rest: Vec<String> = Vec::with_capacity(tokens.len());

    let last_idx = tokens.len().saturating_sub(1);
    for (i, tok) in tokens.iter().enumerate() {
        let all_digits = !tok.is_empty() && tok.chars().all(|c| c.is_ascii_digit());
        if all_digits {
            // Leading pure-digit token → house_number.
            if i == 0 {
                parsed.house_number = Some(tok.clone());
                continue;
            }
            // Trailing 4-digit token → AU-style postcode.
            if i == last_idx && tok.len() == 4 {
                parsed.postcode = Some(tok.clone());
                continue;
            }
            // Trailing short digit group (1–3 digits, or 5 digits for
            // US-style ZIP-that-looks-like-housenumber) → house_number
            // if we don't already have one. Handles `"Alysse Close 10"`
            // and other street-then-number token orders that otherwise
            // fall into the word bag and get tokenised out of existence.
            if i == last_idx
                && parsed.house_number.is_none()
                && (tok.len() <= 3 || tok.len() == 5)
            {
                parsed.house_number = Some(tok.clone());
                continue;
            }
        }

        // AU state abbreviation / full name — only demote from `rest` if we
        // haven't already captured a state (first-wins, handles doubled
        // state mentions sensibly).
        if parsed.state.is_none() {
            if let Some(full) = canonicalise_state(tok) {
                parsed.state = Some(full.to_owned());
                continue;
            }
        }

        rest.push(tok.clone());
    }

    parsed.rest = rest;
    parsed
}

/// Forward-geocoding hit. Kept Serialize so the HTTP layer can pass it
/// straight into a JSON body.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suburb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    pub kind: u64,
    pub rank: u64,
    pub lat: f64,
    pub lng: f64,
    pub score: f32,
}

// --- Fuzzy (experimental, not yet wired) ---

/// Returns a fuzzy query against the name field. Unused today; kept so we
/// can wire in typo tolerance via a tuning flag without refactoring.
pub fn fuzzy_name(schema: &ForwardSchema, token: &str, distance: u8) -> Box<dyn Query> {
    Box::new(FuzzyTermQuery::new(
        Term::from_field_text(schema.name, &token.to_ascii_lowercase()),
        distance,
        true,
    ))
}
