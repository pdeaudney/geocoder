//! Build the G-NAF address-point index (`gnaf_*.bin`) from extracted G-NAF
//! PSV files. Joins ADDRESS_DETAIL + ADDRESS_DEFAULT_GEOCODE +
//! STREET_LOCALITY + LOCALITY + STATE, emits an mmap-friendly sorted index
//! spatially keyed at street_cell_level.
//!
//! Usage:
//!   build-gnaf-index <gnaf-psv-dir> <output-dir> [--street-level N]
//!
//! Memory footprint during build: ~1 GB peak (the in-memory geocode map
//! dominates). Wall-clock: ~2-4 minutes on a typical dev machine.

// mimalloc global allocator — see build-pipeline-perf-plan stage 2.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use query_server::gnaf::GnafPoint;
use s2::cellid::CellID;
use s2::latlng::LatLng;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const DEFAULT_STREET_CELL_LEVEL: u64 = 17;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: {} <gnaf-psv-dir> <output-dir> [--street-level N]",
            args.first().map(String::as_str).unwrap_or("build-gnaf-index")
        );
        std::process::exit(2);
    }
    let psv_dir = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);
    let street_level: u64 = arg_value(&args, "--street-level")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_STREET_CELL_LEVEL);

    match run(&psv_dir, &out_dir, street_level) {
        Ok(stats) => {
            eprintln!(
                "Wrote G-NAF index: {} points, {} cells, {} string bytes",
                stats.points, stats.cells, stats.string_bytes
            );
        }
        Err(e) => {
            eprintln!("build failed: {e}");
            std::process::exit(1);
        }
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|p| args.get(p + 1))
        .cloned()
}

#[derive(Default)]
struct Stats {
    points: u64,
    cells: u64,
    string_bytes: u64,
}

fn run(psv_dir: &Path, out_dir: &Path, street_level: u64) -> Result<Stats, String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {}", out_dir.display(), e))?;

    // --- Pass 1: STATE files → state_pid → state_abbr ---
    let mut state_abbr: HashMap<String, String> = HashMap::new();
    for path in list_files(psv_dir, "_STATE_psv.psv")? {
        read_psv(&path, |fields| {
            if fields.len() < 5 {
                return;
            }
            let (Some(pid), Some(abbr)) = (fields.first(), fields.get(4)) else {
                return;
            };
            if !pid.is_empty() && !abbr.is_empty() {
                state_abbr.insert(pid.to_string(), abbr.to_string());
            }
        })?;
    }
    eprintln!("loaded {} states", state_abbr.len());

    // --- Pass 2: LOCALITY files → locality_pid → (locality_name) ---
    // We don't carry state through here — the postcode comes from
    // ADDRESS_DETAIL directly, and state is tied to the PSV file the
    // record came from so we derive it at emit time.
    let mut locality_name: HashMap<String, String> = HashMap::new();
    for path in list_files(psv_dir, "_LOCALITY_psv.psv")? {
        if path.file_name()
            .is_some_and(|n| n.to_string_lossy().contains("STREET_LOCALITY"))
        {
            continue;
        }
        read_psv(&path, |fields| {
            if fields.len() < 4 {
                return;
            }
            let pid = fields[0];
            let name = fields[3];
            let retired = fields[2];
            if !pid.is_empty() && !name.is_empty() && retired.is_empty() {
                locality_name.insert(pid.to_string(), name.to_string());
            }
        })?;
    }
    eprintln!("loaded {} localities", locality_name.len());

    // --- Pass 3: STREET_LOCALITY → street_locality_pid → "NAME TYPE" ---
    let mut street: HashMap<String, String> = HashMap::new();
    for path in list_files(psv_dir, "_STREET_LOCALITY_psv.psv")? {
        read_psv(&path, |fields| {
            // STREET_LOCALITY_PID | DATE_CREATED | DATE_RETIRED | STREET_CLASS_CODE
            //                    | STREET_NAME | STREET_TYPE_CODE | ...
            if fields.len() < 6 {
                return;
            }
            let pid = fields[0];
            let retired = fields[2];
            let name = fields[4];
            let type_code = fields[5];
            if pid.is_empty() || !retired.is_empty() || name.is_empty() {
                return;
            }
            let display = if type_code.is_empty() {
                title_case(name)
            } else {
                format!("{} {}", title_case(name), title_case(type_code))
            };
            street.insert(pid.to_string(), display);
        })?;
    }
    eprintln!("loaded {} streets", street.len());

    // --- Pass 4: ADDRESS_DEFAULT_GEOCODE → address_detail_pid → (lat, lng) ---
    // Biggest single in-memory structure — ~14M entries × ~40 bytes/entry =
    // ~560 MB. Build machines routinely have 16+ GB, so this is fine.
    let mut geocode: HashMap<String, (f32, f32)> =
        HashMap::with_capacity(15_000_000);
    let mut geo_rows = 0u64;
    for path in list_files(psv_dir, "_ADDRESS_DEFAULT_GEOCODE_psv.psv")? {
        eprintln!("reading {}", path.display());
        read_psv(&path, |fields| {
            // ADDRESS_DEFAULT_GEOCODE_PID | DATE_CREATED | DATE_RETIRED |
            //   ADDRESS_DETAIL_PID | GEOCODE_TYPE_CODE | LONGITUDE | LATITUDE
            if fields.len() < 7 {
                return;
            }
            let address_pid = fields[3];
            let retired = fields[2];
            if address_pid.is_empty() || !retired.is_empty() {
                return;
            }
            let Ok(lng) = fields[5].parse::<f32>() else {
                return;
            };
            let Ok(lat) = fields[6].parse::<f32>() else {
                return;
            };
            geocode.insert(address_pid.to_string(), (lat, lng));
            geo_rows += 1;
            if geo_rows % 2_000_000 == 0 {
                eprintln!("  {}M geocodes loaded", geo_rows / 1_000_000);
            }
        })?;
    }
    eprintln!("loaded {} geocodes", geocode.len());

    // --- Pass 5: ADDRESS_DETAIL × geocode → build output records ---
    let mut strings = StringPool::new();
    let mut records: Vec<(u64, GnafPoint)> = Vec::with_capacity(14_000_000);
    let mut addr_rows = 0u64;
    let mut skipped_no_geocode = 0u64;

    for path in list_files(psv_dir, "_ADDRESS_DETAIL_psv.psv")? {
        let state_abbr_for_file = state_abbr_for_path(&path, &state_abbr);
        eprintln!(
            "joining {} ({})",
            path.display(),
            state_abbr_for_file.as_deref().unwrap_or("unknown"),
        );
        read_psv(&path, |fields| {
            // Field positions (0-indexed):
            //   0=ADDRESS_DETAIL_PID, 3=DATE_RETIRED, 16=NUMBER_FIRST_PREFIX,
            //   17=NUMBER_FIRST, 18=NUMBER_FIRST_SUFFIX, 22=STREET_LOCALITY_PID,
            //   24=LOCALITY_PID, 26=POSTCODE
            if fields.len() <= 26 {
                return;
            }
            let pid = fields[0];
            let retired = fields[3];
            if pid.is_empty() || !retired.is_empty() {
                return;
            }
            let Some(&(lat, lng)) = geocode.get(pid) else {
                skipped_no_geocode += 1;
                return;
            };
            let street_pid = fields[22];
            let locality_pid = fields[24];
            let postcode = fields[26];
            if postcode.is_empty() {
                return;
            }

            let housenumber = build_housenumber(fields);
            if housenumber.is_empty() {
                return;
            }
            let Some(street_name) = street.get(street_pid) else {
                return;
            };
            let Some(locality) = locality_name.get(locality_pid) else {
                return;
            };

            let point = GnafPoint {
                lat,
                lng,
                housenumber_id: strings.intern(&housenumber),
                street_id: strings.intern(street_name),
                locality_id: strings.intern(&title_case(locality)),
                postcode_id: strings.intern(postcode),
            };
            let cell = CellID::from(LatLng::from_degrees(lat as f64, lng as f64))
                .parent(street_level)
                .0;
            records.push((cell, point));
            addr_rows += 1;
            if addr_rows % 2_000_000 == 0 {
                eprintln!("  {}M addresses emitted", addr_rows / 1_000_000);
            }
        })?;
    }
    eprintln!(
        "emitted {} addresses ({} skipped: no geocode)",
        addr_rows, skipped_no_geocode
    );

    // --- Sort records by cell_id so lookups scan contiguous addresses ---
    records.sort_by_key(|(cell, _)| *cell);

    // --- Write gnaf_points.bin + build cells/entries index ---
    let points_path = out_dir.join("gnaf_points.bin");
    let cells_path = out_dir.join("gnaf_cells.bin");
    let entries_path = out_dir.join("gnaf_entries.bin");
    let strings_path = out_dir.join("gnaf_strings.bin");

    let mut points_file = File::create(&points_path)
        .map_err(|e| format!("create {}: {}", points_path.display(), e))?;
    let mut cells_file = File::create(&cells_path)
        .map_err(|e| format!("create {}: {}", cells_path.display(), e))?;
    let mut entries_file = File::create(&entries_path)
        .map_err(|e| format!("create {}: {}", entries_path.display(), e))?;

    let mut current_entry_offset: u32 = 0;
    let mut cells_written: u64 = 0;
    let mut run_start = 0usize;
    while run_start < records.len() {
        let cell_id = records[run_start].0;
        let mut run_end = run_start + 1;
        while run_end < records.len() && records[run_end].0 == cell_id {
            run_end += 1;
        }
        let run_len = (run_end - run_start).min(u16::MAX as usize);

        // cells.bin entry: cell_id (u64 LE) + entry offset (u32 LE)
        cells_file
            .write_all(&cell_id.to_le_bytes())
            .map_err(io_err(&cells_path))?;
        cells_file
            .write_all(&current_entry_offset.to_le_bytes())
            .map_err(io_err(&cells_path))?;
        cells_written += 1;

        // entries.bin entry: u16 count, u32 ids[]
        entries_file
            .write_all(&(run_len as u16).to_le_bytes())
            .map_err(io_err(&entries_path))?;
        for (i, _) in records[run_start..run_end].iter().enumerate() {
            if i >= u16::MAX as usize {
                break;
            }
            let id = (run_start + i) as u32;
            entries_file
                .write_all(&id.to_le_bytes())
                .map_err(io_err(&entries_path))?;
        }
        current_entry_offset += 2 + (run_len as u32) * 4;

        run_start = run_end;
    }

    // gnaf_points.bin is just the sorted records' GnafPoint values in order.
    for (_, point) in &records {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                point as *const GnafPoint as *const u8,
                std::mem::size_of::<GnafPoint>(),
            )
        };
        points_file.write_all(bytes).map_err(io_err(&points_path))?;
    }

    let strings_bytes = strings.into_bytes();
    fs::write(&strings_path, &strings_bytes).map_err(io_err(&strings_path))?;

    Ok(Stats {
        points: records.len() as u64,
        cells: cells_written,
        string_bytes: strings_bytes.len() as u64,
    })
}

fn io_err(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("write {}: {}", path.display(), e)
}

/// Compose the house number string from ADDRESS_DETAIL fields. Handles
/// "10", "10A", "10-12", "Lot 5" by concatenating the prefix/first/suffix
/// components that G-NAF splits across separate columns.
fn build_housenumber(fields: &[&str]) -> String {
    let prefix = fields.get(16).copied().unwrap_or("");
    let first = fields.get(17).copied().unwrap_or("");
    let suffix = fields.get(18).copied().unwrap_or("");
    let last_prefix = fields.get(19).copied().unwrap_or("");
    let last = fields.get(20).copied().unwrap_or("");
    let last_suffix = fields.get(21).copied().unwrap_or("");

    let first_combined = format!("{}{}{}", prefix, first, suffix);
    let last_combined = format!("{}{}{}", last_prefix, last, last_suffix);
    match (first_combined.is_empty(), last_combined.is_empty()) {
        (true, true) => String::new(),
        (false, true) => first_combined,
        (true, false) => last_combined,
        (false, false) => format!("{}-{}", first_combined, last_combined),
    }
}

/// Upper-case input like "ALYSSE CLOSE" / "BAULKHAM HILLS" → nicer
/// "Alysse Close" / "Baulkham Hills". G-NAF stores everything in ALL CAPS.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_alpha = false;
    for ch in s.chars() {
        if ch.is_alphabetic() {
            if prev_alpha {
                out.extend(ch.to_lowercase());
            } else {
                out.extend(ch.to_uppercase());
            }
            prev_alpha = true;
        } else {
            out.push(ch);
            prev_alpha = false;
        }
    }
    out
}

fn state_abbr_for_path(
    path: &Path,
    state_abbr: &HashMap<String, String>,
) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    // "NSW_ADDRESS_DETAIL_psv.psv" → "NSW"
    let prefix = file_name.split('_').next()?;
    // state_abbr values are the abbreviations themselves, so just sanity
    // check the filename prefix appears there.
    if state_abbr.values().any(|v| v == prefix) {
        Some(prefix.to_string())
    } else {
        None
    }
}

// --- PSV streaming reader ---

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

fn read_psv<F>(path: &Path, mut on_row: F) -> Result<(), String>
where
    F: FnMut(&[&str]),
{
    let file = File::open(path).map_err(|e| format!("open {}: {}", path.display(), e))?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut first = true;
    for line in reader.lines() {
        let line = line.map_err(|e| format!("read {}: {}", path.display(), e))?;
        if first {
            first = false;
            continue;
        }
        let fields: Vec<&str> = line.split('|').collect();
        on_row(&fields);
    }
    Ok(())
}

// --- String pool (dedup + byte-offset index) ---

struct StringPool {
    data: Vec<u8>,
    index: HashMap<String, u32>,
}

impl StringPool {
    fn new() -> Self {
        StringPool {
            // Prime with "" at offset 0 so unset ids all resolve to "".
            data: vec![0],
            index: HashMap::from([(String::new(), 0)]),
        }
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&off) = self.index.get(s) {
            return off;
        }
        let off = self.data.len() as u32;
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
        self.index.insert(s.to_owned(), off);
        off
    }

    fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}
