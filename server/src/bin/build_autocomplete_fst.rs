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

use fst::MapBuilder;
use query_server::autocomplete::{AutocompleteEntry, KIND_PLACE, KIND_STREET};
use query_server::{
    as_typed_slice, Index, NodeCoord, PlacePoint, WayHeader, DEFAULT_ADMIN_CELL_LEVEL,
    DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    PerCountry,
    Unified,
    Both,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "Usage: {} <reverse-index-dir> [--country cc,cc] [--layout per-country|unified|both]",
            args.first().map(String::as_str).unwrap_or("build-autocomplete-fst")
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
            eprintln!(
                "unknown --layout value {other:?}; expected per-country|unified|both"
            );
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&dir, country_filter.as_ref(), layout) {
        eprintln!("build failed: {e}");
        std::process::exit(1);
    }
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

fn run(
    dir: &PathBuf,
    country_filter: Option<&HashSet<[u8; 2]>>,
    layout: Layout,
) -> Result<(), String> {
    let dir_str = dir
        .to_str()
        .ok_or_else(|| format!("non-utf8 path: {}", dir.display()))?;
    let idx = Index::load(
        dir_str,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )?;

    let mut by_country: HashMap<[u8; 2], PerCountry> = HashMap::new();

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

    fn add(
        by_country: &mut HashMap<[u8; 2], PerCountry>,
        country_filter: Option<&HashSet<[u8; 2]>>,
        cc: [u8; 2],
        name: &str,
        kind: u8,
        rank: u8,
        lat: f32,
        lng: f32,
        suburb: Option<&str>,
    ) {
        if let Some(filter) = country_filter {
            if !filter.contains(&[cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()]) {
                return;
            }
        }
        let key = normalise_fst_key(name);
        if key.is_empty() {
            return;
        }
        let cc_lower = [cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()];
        let pc = by_country.entry(cc_lower).or_insert_with(fresh_per_country);

        let name_offset = intern(pc, name);
        let suburb_offset = match suburb.filter(|s| !s.is_empty()) {
            Some(s) => intern(pc, s),
            None => 0,
        };
        let entry = AutocompleteEntry {
            lat,
            lng,
            name_offset,
            suburb_offset,
            kind,
            rank,
            pad: [0; 2],
        };
        let idx = pc.entries.len() as u64;
        pc.entries.push(entry);

        // If multiple rows share a normalised key (e.g., "main street" in
        // many suburbs), the FST keeps only the last — but we still have
        // every row in the entries array. Consumers starts_with-walk the
        // FST and merge by rank anyway. Keeping one FST row per key is
        // fine for the prefix-seed step; we over-fetch by 3× for ranking.
        // To keep per-key entries accessible, tag the FST value with the
        // most prominent (lowest rank) representative.
        pc.keys
            .entry(key)
            .and_modify(|existing_idx| {
                let existing_rank = pc.entries[*existing_idx as usize].rank;
                if rank < existing_rank {
                    *existing_idx = idx;
                }
            })
            .or_insert(idx);
    }

    // Places
    if let Some(pp) = idx.place_points.as_ref() {
        let points: &[PlacePoint] = as_typed_slice(pp);
        for p in points {
            let name = idx.get_string(p.name_id);
            if name.is_empty() {
                continue;
            }
            let admin = idx.find_admin(p.lat as f64, p.lng as f64);
            let Some(cc) = admin.country_code.filter(|c| c[0] != 0 && c[1] != 0) else {
                continue;
            };
            add(
                &mut by_country,
                country_filter,
                cc,
                name,
                KIND_PLACE,
                p.rank as u8,
                p.lat,
                p.lng,
                admin.city,
            );
        }
    }

    // Streets
    let ways: &[WayHeader] = as_typed_slice(&idx.street_ways);
    let nodes: &[NodeCoord] = as_typed_slice(&idx.street_nodes);
    let mut seen: HashSet<(u32, String, [u8; 2])> = HashSet::new();
    for way in ways {
        let name = idx.get_string(way.name_id);
        if name.is_empty() {
            continue;
        }
        let off = way.node_offset as usize;
        let count = way.node_count as usize;
        if count == 0 || off + count > nodes.len() {
            continue;
        }
        let mid = nodes[off + count / 2];
        let lat = mid.lat;
        let lng = mid.lng;
        let admin = idx.find_admin(lat as f64, lng as f64);
        let Some(cc) = admin.country_code.filter(|c| c[0] != 0 && c[1] != 0) else {
            continue;
        };
        let suburb_key = admin.city.unwrap_or("").to_string();
        if !seen.insert((way.name_id, suburb_key.clone(), cc)) {
            continue;
        }
        add(
            &mut by_country,
            country_filter,
            cc,
            name,
            KIND_STREET,
            26,
            lat,
            lng,
            admin.city,
        );
    }

    // Deterministic ordering: sort country codes before emitting so
    // rebuilds from identical input produce byte-identical .bin outputs
    // (unified entry IDs depend on iteration order).
    let mut ccs: Vec<[u8; 2]> = by_country.keys().copied().collect();
    ccs.sort();

    if matches!(layout, Layout::PerCountry | Layout::Both) {
        for cc in &ccs {
            let pc = by_country.get(cc).expect("cc came from by_country");
            if pc.entries.is_empty() {
                continue;
            }
            emit_per_country(dir, cc, pc)?;
        }
    }

    if matches!(layout, Layout::Unified | Layout::Both) {
        emit_unified(dir, &ccs, &by_country)?;
    }

    Ok(())
}

fn emit_per_country(
    dir: &std::path::Path,
    cc: &[u8; 2],
    pc: &PerCountry,
) -> Result<(), String> {
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
    fs::rename(&fst_tmp, &fst_path)
        .map_err(|e| format!("rename {}: {}", fst_path.display(), e))?;

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
        let Some(pc) = by_country.get(cc) else { continue };
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
    fs::rename(&fst_tmp, &fst_path)
        .map_err(|e| format!("rename {}: {}", fst_path.display(), e))?;

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

fn write_entries(
    path: &std::path::Path,
    entries: &[AutocompleteEntry],
) -> Result<(), String> {
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
    f.flush().map_err(|e| format!("flush {}: {}", path.display(), e))?;
    Ok(())
}

fn write_fst<'a, I>(path: &std::path::Path, entries: I) -> Result<(), String>
where
    I: IntoIterator<Item = (&'a [u8], u64)>,
{
    let fst_file =
        File::create(path).map_err(|e| format!("create {}: {}", path.display(), e))?;
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

fn normalise_fst_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        let folded = fold(ch);
        for c in folded.chars() {
            if c.is_alphanumeric() {
                out.extend(c.to_lowercase());
                last_space = false;
            } else if !last_space {
                out.push(' ');
                last_space = true;
            }
        }
    }
    out.trim().to_owned()
}

fn fold(ch: char) -> String {
    match ch {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => "a".into(),
        'é' | 'è' | 'ê' | 'ë' => "e".into(),
        'í' | 'ì' | 'î' | 'ï' => "i".into(),
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => "o".into(),
        'ú' | 'ù' | 'û' | 'ü' => "u".into(),
        'ñ' => "n".into(),
        'ç' => "c".into(),
        'ß' => "ss".into(),
        c => c.to_string(),
    }
}
