//! Build `postcode_lookup.bin` + `postcode_lookup_strings.bin` from the
//! G-NAF PSV files (free CC-BY 4.0 release from data.gov.au).
//!
//! Usage:
//!   build-postcode-lookup <gnaf-psv-dir> <output-dir>
//!
//! Where:
//!   <gnaf-psv-dir> holds the extracted state PSV files (e.g.
//!   `NSW_LOCALITY_psv.psv`, `NSW_STATE_psv.psv`, `NSW_ADDRESS_DETAIL_psv.psv`).
//!
//! Pipeline:
//!   1. Parse STATE files →  state_pid → state_abbr
//!   2. Parse LOCALITY files → locality_pid → (locality_name, state_pid)
//!   3. Stream ADDRESS_DETAIL files → accumulate
//!      (locality_pid, postcode) → count
//!   4. For each locality, pick the modal postcode (the one used by the
//!      most addresses). Ties broken lexically.
//!   5. Emit the sorted hash-keyed lookup table.
//!
//! Attribution (per G-NAF CC-BY 4.0): output is derived from G-NAF data
//! © Commonwealth of Australia (Geoscape Australia) licensed under CC-BY 4.0.

// mimalloc global allocator — see build-pipeline-perf-plan stage 2.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use query_server::postcode::{self, RawEntry};

/// Per-stage timing — see build-pipeline-perf-plan stage 6.
struct Stage { name: &'static str, start: Instant }
impl Stage { fn new(name: &'static str) -> Self { Self { name, start: Instant::now() } } }
impl Drop for Stage { fn drop(&mut self) { eprintln!("[stage] {}: {:.3}s", self.name, self.start.elapsed().as_secs_f64()); } }

fn main() {
    let _total = Stage::new("total");
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: {} <gnaf-psv-dir> <output-dir>",
            args.first().map(String::as_str).unwrap_or("build-postcode-lookup")
        );
        std::process::exit(2);
    }
    let psv_dir = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);

    if let Err(e) = run(&psv_dir, &out_dir) {
        eprintln!("build failed: {e}");
        std::process::exit(1);
    }
}

fn run(psv_dir: &Path, out_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {}", out_dir.display(), e))?;

    // --- Pass 1: STATE files → state_pid → state_abbr ---
    let mut state_abbrevs: HashMap<String, String> = HashMap::new();
    for path in list_files(psv_dir, "_STATE_psv.psv")? {
        read_psv(&path, |fields| {
            // STATE_PID | DATE_CREATED | DATE_RETIRED | STATE_NAME | STATE_ABBREVIATION
            if let (Some(pid), Some(abbr)) = (fields.first(), fields.get(4)) {
                if !pid.is_empty() && !abbr.is_empty() {
                    state_abbrevs.insert(pid.to_string(), abbr.to_string());
                }
            }
        })?;
    }
    eprintln!("loaded {} state abbreviations", state_abbrevs.len());

    // --- Pass 2: LOCALITY files → locality_pid → (locality_name, state_pid) ---
    let mut localities: HashMap<String, (String, String)> = HashMap::new();
    for path in list_files(psv_dir, "_LOCALITY_psv.psv")? {
        // Skip the STREET_LOCALITY suffix variant if it slipped in.
        if path.file_name().is_some_and(|n| n.to_string_lossy().contains("STREET_LOCALITY")) {
            continue;
        }
        read_psv(&path, |fields| {
            // LOCALITY_PID | DATE_CREATED | DATE_RETIRED | LOCALITY_NAME
            //             | PRIMARY_POSTCODE | LOCALITY_CLASS_CODE | STATE_PID | ...
            if fields.len() < 7 {
                return;
            }
            let pid = fields[0];
            let name = fields[3];
            let state_pid = fields[6];
            let retired = fields[2];
            if pid.is_empty() || name.is_empty() || state_pid.is_empty() {
                return;
            }
            // Skip retired localities; they aren't real current suburbs.
            if !retired.is_empty() {
                return;
            }
            localities.insert(pid.to_string(), (name.to_string(), state_pid.to_string()));
        })?;
    }
    eprintln!("loaded {} active localities", localities.len());

    // --- Pass 3: ADDRESS_DETAIL files → (locality_pid, postcode) → count ---
    // This is the expensive step — ~14M rows across all states.
    let mut postcode_counts: HashMap<(String, String), u32> = HashMap::new();
    let mut rows_scanned: u64 = 0;
    for path in list_files(psv_dir, "_ADDRESS_DETAIL_psv.psv")? {
        eprintln!("scanning {}", path.display());
        read_psv(&path, |fields| {
            // Field positions per the file's header (1-indexed in header, 0
            // here): LOCALITY_PID = 24, POSTCODE = 26.
            if fields.len() <= 26 {
                return;
            }
            let locality_pid = fields[24];
            let postcode = fields[26];
            let retired = fields[3]; // DATE_RETIRED
            if locality_pid.is_empty() || postcode.is_empty() || !retired.is_empty() {
                return;
            }
            *postcode_counts
                .entry((locality_pid.to_string(), postcode.to_string()))
                .or_insert(0) += 1;
            rows_scanned += 1;
            if rows_scanned % 1_000_000 == 0 {
                eprintln!("  {} rows scanned, {} distinct (locality,postcode) pairs", rows_scanned, postcode_counts.len());
            }
        })?;
    }
    eprintln!(
        "processed {} address rows, {} distinct (locality,postcode) pairs",
        rows_scanned,
        postcode_counts.len(),
    );

    // --- Pass 4: modal postcode per locality ---
    // Fold counts into per-locality best postcode. Ties: lexicographic min
    // (stable across rebuilds).
    let mut best: HashMap<String, (String, u32)> = HashMap::new();
    for ((locality_pid, postcode), count) in postcode_counts {
        best.entry(locality_pid)
            .and_modify(|(existing_pc, existing_count)| {
                if count > *existing_count || (count == *existing_count && postcode < *existing_pc) {
                    *existing_pc = postcode.clone();
                    *existing_count = count;
                }
            })
            .or_insert((postcode, count));
    }
    eprintln!("resolved modal postcode for {} localities", best.len());

    // --- Pass 5: emit records ---
    // Build (hash, postcode) pairs; dedupe strings into a pool.
    let mut strings = Vec::<u8>::new();
    let mut pc_offset: HashMap<String, u32> = HashMap::new();
    let mut intern = |s: &str, strings: &mut Vec<u8>| -> u32 {
        if let Some(&o) = pc_offset.get(s) {
            return o;
        }
        let offset = strings.len() as u32;
        strings.extend_from_slice(s.as_bytes());
        strings.push(0);
        pc_offset.insert(s.to_owned(), offset);
        offset
    };

    let mut entries: Vec<RawEntry> = Vec::with_capacity(best.len());
    let mut emitted = 0u32;
    let mut skipped_no_state = 0u32;
    for (locality_pid, (postcode, _)) in best {
        let Some((locality_name, state_pid)) = localities.get(&locality_pid) else {
            continue;
        };
        let Some(state_abbr) = state_abbrevs.get(state_pid) else {
            skipped_no_state += 1;
            continue;
        };
        let key = postcode::lookup_hash(state_abbr, locality_name);
        let offset = intern(&postcode, &mut strings);
        entries.push(RawEntry {
            key_hash: key,
            postcode_offset: offset,
            _pad: 0,
        });
        emitted += 1;
    }
    eprintln!(
        "emitting {} entries ({} skipped due to missing state)",
        emitted, skipped_no_state
    );

    entries.sort_by_key(|e| e.key_hash);
    // Detect hash collisions — at this scale any collision indicates a
    // data problem (duplicate localities that survived earlier filters).
    let mut collisions = 0u32;
    for w in entries.windows(2) {
        if w[0].key_hash == w[1].key_hash && w[0].postcode_offset != w[1].postcode_offset {
            collisions += 1;
        }
    }
    if collisions > 0 {
        eprintln!("WARN: {} hash collisions with different postcodes (first wins)", collisions);
    }
    // Dedup — keep first of each key_hash.
    entries.dedup_by_key(|e| e.key_hash);

    let entries_path = out_dir.join("postcode_lookup.bin");
    let strings_path = out_dir.join("postcode_lookup_strings.bin");
    {
        let mut f = File::create(&entries_path)
            .map_err(|e| format!("create {}: {}", entries_path.display(), e))?;
        for entry in &entries {
            f.write_all(&entry.to_le_bytes())
                .map_err(|e| format!("write {}: {}", entries_path.display(), e))?;
        }
    }
    fs::write(&strings_path, &strings)
        .map_err(|e| format!("write {}: {}", strings_path.display(), e))?;

    eprintln!(
        "wrote {} ({} entries, {} bytes) and {} ({} bytes)",
        entries_path.display(),
        entries.len(),
        entries.len() * 16,
        strings_path.display(),
        strings.len(),
    );
    Ok(())
}

fn list_files(dir: &Path, suffix: &str) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {}: {}", dir.display(), e))? {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(suffix))
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Stream-read a PSV file: skip the header row, split every subsequent line
/// on `|`, invoke `on_row` with the fields as `&str` slices into a reusable
/// line buffer.
fn read_psv<F>(path: &Path, mut on_row: F) -> Result<(), String>
where
    F: FnMut(&[&str]),
{
    let file = File::open(path).map_err(|e| format!("open {}: {}", path.display(), e))?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut first = true;
    for line in reader.lines() {
        let line = line.map_err(|e| format!("read line in {}: {}", path.display(), e))?;
        if first {
            first = false;
            continue;
        }
        // Trailing empty fields are legitimate; split_terminator handles
        // CRLF by having the caller not include \n in lines().
        let fields: Vec<&str> = line.split('|').collect();
        on_row(&fields);
    }
    Ok(())
}
