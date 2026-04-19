//! Build per-country FST-backed autocomplete indexes from the reverse
//! binary index.
//!
//! Usage:
//!   build-autocomplete-fst <reverse-index-dir> [--country cc,cc]
//!
//! Emits `fst_<cc>.fst`, `fst_<cc>.bin`, `fst_<cc>_strings.bin` per
//! country. Each FST is keyed on the normalised (lowercased, ASCII-folded,
//! alphanumeric-only) name of a street or place, with the value being the
//! index into the per-country entries file.
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "Usage: {} <reverse-index-dir> [--country cc,cc]",
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

    if let Err(e) = run(&dir, country_filter.as_ref()) {
        eprintln!("build failed: {e}");
        std::process::exit(1);
    }
}

fn run(dir: &PathBuf, country_filter: Option<&HashSet<[u8; 2]>>) -> Result<(), String> {
    let dir_str = dir
        .to_str()
        .ok_or_else(|| format!("non-utf8 path: {}", dir.display()))?;
    let idx = Index::load(
        dir_str,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )?;

    // Group entries by country. Each group gets its own FST / bin pair.
    #[derive(Default)]
    struct PerCountry {
        entries: Vec<AutocompleteEntry>,
        strings: Vec<u8>,
        // interned string offset → previously-seen offset
        intern_index: HashMap<String, u32>,
        // normalised_key → entry_id, sorted for FST emit (BTreeMap)
        keys: BTreeMap<String, u64>,
    }

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

    // Seed the empty string at offset 0 for each new country, matching
    // how the runtime treats offset 0 as "".
    fn ensure_seed(pc: &mut PerCountry) {
        if pc.strings.is_empty() {
            pc.strings.push(0);
            pc.intern_index.insert(String::new(), 0);
        }
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
        let pc = by_country.entry(cc_lower).or_insert_with(PerCountry::default);
        ensure_seed(pc);

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

    // Emit per country.
    for (cc, pc) in &by_country {
        if pc.entries.is_empty() {
            continue;
        }
        let prefix = format!("fst_{}{}", cc[0] as char, cc[1] as char);

        let entries_path = dir.join(format!("{prefix}.bin"));
        let mut f = BufWriter::new(
            File::create(&entries_path).map_err(|e| format!("create {}: {}", entries_path.display(), e))?,
        );
        for entry in &pc.entries {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    entry as *const AutocompleteEntry as *const u8,
                    std::mem::size_of::<AutocompleteEntry>(),
                )
            };
            f.write_all(bytes)
                .map_err(|e| format!("write {}: {}", entries_path.display(), e))?;
        }
        f.flush().map_err(|e| format!("flush: {e}"))?;

        let strings_path = dir.join(format!("{prefix}_strings.bin"));
        fs::write(&strings_path, &pc.strings)
            .map_err(|e| format!("write {}: {}", strings_path.display(), e))?;

        let fst_path = dir.join(format!("{prefix}.fst"));
        let fst_file = File::create(&fst_path)
            .map_err(|e| format!("create {}: {}", fst_path.display(), e))?;
        let mut builder = MapBuilder::new(BufWriter::new(fst_file))
            .map_err(|e| format!("fst builder: {e}"))?;
        for (key, id) in &pc.keys {
            builder
                .insert(key, *id)
                .map_err(|e| format!("fst insert {key:?}: {e}"))?;
        }
        builder
            .finish()
            .map_err(|e| format!("fst finish: {e}"))?;

        eprintln!(
            "  fst_{}{}: {} entries, {} keys, {} KB strings",
            cc[0] as char,
            cc[1] as char,
            pc.entries.len(),
            pc.keys.len(),
            pc.strings.len() / 1024,
        );
    }

    Ok(())
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
