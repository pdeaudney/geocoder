//! Build per-country address-point indexes from an OpenAddresses.io batch.
//!
//! Usage:
//!   build-openaddresses-index <csv-root> <output-dir> \
//!       [--country au,us,fr] [--skip au] [--street-level N]
//!
//! `<csv-root>` is a directory whose children are 2-letter country codes
//! (from an extracted OpenAddresses batch):
//!
//! ```text
//! <csv-root>/
//!   us/
//!     ca.csv
//!     ny.csv
//!     ...
//!   fr/
//!     countrywide.csv
//!   au/
//!     countrywide.csv
//!   ...
//! ```
//!
//! Reads every `*.csv` under each country directory, joins them into a
//! single per-country binary index (`oa_<cc>_{points,cells,entries,strings}.bin`),
//! sorts by S2 cell at `street_cell_level` for spatial locality.
//!
//! # Why per-country
//!
//! Deployments serving a subset of countries only mount the ones they
//! need — no virtual memory / disk paid for unused geographies. Each
//! country is independently rebuildable on its own cadence.
//!
//! # Why skip AU by default
//!
//! The AU entries in OA come from G-NAF. If you've already run
//! `build-gnaf-index` you have fresher data directly from Geoscape.
//! `--skip au` (the default) avoids redundant ingestion. Pass
//! `--country au` to override.

// mimalloc global allocator — see build-pipeline-perf-plan stage 2.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use query_server::address_points::{AddressPoint, BuildOutputPaths};
use query_server::openaddresses::oa_prefix;
use s2::cellid::CellID;
use s2::latlng::LatLng;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const DEFAULT_STREET_CELL_LEVEL: u64 = 17;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: {} <csv-root> <output-dir> [--country cc,cc] [--skip cc,cc] [--street-level N]",
            args.first().map(String::as_str).unwrap_or("build-openaddresses-index"),
        );
        std::process::exit(2);
    }

    let csv_root = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);
    let street_level: u64 = arg_value(&args, "--street-level")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_STREET_CELL_LEVEL);
    let country_filter: Option<HashSet<[u8; 2]>> = arg_value(&args, "--country")
        .map(|v| parse_country_list(&v))
        .filter(|s| !s.is_empty());
    // Default: skip AU because G-NAF is fresher.
    let skip: HashSet<[u8; 2]> = arg_value(&args, "--skip")
        .map(|v| parse_country_list(&v))
        .unwrap_or_else(|| [*b"au"].into_iter().collect());

    match run(&csv_root, &out_dir, street_level, country_filter.as_ref(), &skip) {
        Ok(()) => eprintln!("Done."),
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

fn parse_country_list(s: &str) -> HashSet<[u8; 2]> {
    s.split(|c: char| c == ',' || c.is_whitespace())
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
}

fn run(
    csv_root: &Path,
    out_dir: &Path,
    street_level: u64,
    country_filter: Option<&HashSet<[u8; 2]>>,
    skip: &HashSet<[u8; 2]>,
) -> Result<(), String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {}", out_dir.display(), e))?;

    // Discover per-country directories.
    let entries = fs::read_dir(csv_root)
        .map_err(|e| format!("read_dir {}: {}", csv_root.display(), e))?;
    let mut country_dirs: Vec<([u8; 2], PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        let b = name.as_bytes();
        if b.len() != 2 || !b[0].is_ascii_alphabetic() || !b[1].is_ascii_alphabetic() {
            continue;
        }
        let cc = [b[0], b[1]];
        if skip.contains(&cc) {
            eprintln!("skipping {name} (in --skip)");
            continue;
        }
        if let Some(filter) = country_filter {
            if !filter.contains(&cc) {
                continue;
            }
        }
        country_dirs.push((cc, entry.path()));
    }
    country_dirs.sort_by_key(|(cc, _)| *cc);

    if country_dirs.is_empty() {
        eprintln!("no countries to build (check --country and --skip)");
        return Ok(());
    }

    // Each country is fully independent — separate output files, separate
    // string pools, separate cell maps. Embarrassingly parallel: rayon
    // shards across the worker pool. On a 16-core box this turns a
    // ~30-minute sequential OA pass into ~3 minutes. Stderr output
    // interleaves across countries, which is fine — nothing parses it.
    use rayon::prelude::*;
    country_dirs.par_iter().try_for_each(|(cc, dir)| {
        eprintln!("--- building {} ---", std::str::from_utf8(cc).unwrap_or("??"));
        build_country(cc, dir, out_dir, street_level)
    })
}

fn build_country(
    cc: &[u8; 2],
    csv_dir: &Path,
    out_dir: &Path,
    street_level: u64,
) -> Result<(), String> {
    // Collect every CSV under the country directory.
    let csv_paths = list_csvs(csv_dir)?;
    if csv_paths.is_empty() {
        eprintln!("  no CSVs found under {}", csv_dir.display());
        return Ok(());
    }

    let mut strings = StringPool::new();
    let mut records: Vec<(u64, AddressPoint)> = Vec::new();
    let mut scanned: u64 = 0;
    let mut kept: u64 = 0;
    let mut dropped_no_coord: u64 = 0;
    let mut dropped_no_number: u64 = 0;

    for csv_path in &csv_paths {
        eprintln!("  reading {}", csv_path.display());
        read_csv(csv_path, |row| {
            scanned += 1;

            let Some(lon) = row.get("LON").and_then(|v| v.parse::<f32>().ok()) else {
                dropped_no_coord += 1;
                return;
            };
            let Some(lat) = row.get("LAT").and_then(|v| v.parse::<f32>().ok()) else {
                dropped_no_coord += 1;
                return;
            };
            if !lat.is_finite() || !lon.is_finite() {
                dropped_no_coord += 1;
                return;
            }

            let number = row.get("NUMBER").unwrap_or_default();
            if number.trim().is_empty() {
                dropped_no_number += 1;
                return;
            }
            let street = row.get("STREET").unwrap_or_default();
            let city = row.get("CITY").unwrap_or_default();
            let postcode = row.get("POSTCODE").unwrap_or_default();

            let point = AddressPoint {
                lat,
                lng: lon,
                housenumber_id: strings.intern(number),
                street_id: strings.intern(street),
                locality_id: strings.intern(city),
                postcode_id: strings.intern(postcode),
            };
            let cell = CellID::from(LatLng::from_degrees(lat as f64, lon as f64))
                .parent(street_level)
                .0;
            records.push((cell, point));
            kept += 1;
            if kept % 1_000_000 == 0 {
                eprintln!("    {}M records kept", kept / 1_000_000);
            }
        })?;
    }

    eprintln!(
        "  scanned {} rows, kept {}, dropped {} (no coord) + {} (no number)",
        scanned, kept, dropped_no_coord, dropped_no_number,
    );

    if records.is_empty() {
        eprintln!("  nothing to write for {}", std::str::from_utf8(cc).unwrap_or("??"));
        return Ok(());
    }

    // Sort by S2 cell so entries and points are written contiguously per cell.
    records.sort_by_key(|(c, _)| *c);

    let prefix = oa_prefix(*cc);
    let paths = BuildOutputPaths::for_prefix(out_dir, &prefix);
    write_output(&records, &strings, &paths)?;
    eprintln!(
        "  wrote {} ({} points, {} MB points.bin)",
        prefix,
        records.len(),
        records.len() * std::mem::size_of::<AddressPoint>() / (1 << 20),
    );
    Ok(())
}

fn list_csvs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    collect_csvs(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_csvs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("read_dir {}: {}", dir.display(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_csvs(&path, out)?;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("csv"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Minimal CSV parser for OpenAddresses data. OA's exports are clean
/// UTF-8 with standard double-quote escaping, so we don't need a full
/// RFC-4180 implementation — this handles the common cases (quoted
/// fields, escaped quotes inside quoted fields, commas inside quotes).
fn read_csv<F>(path: &Path, mut on_row: F) -> Result<(), String>
where
    F: FnMut(&CsvRow),
{
    let file = File::open(path).map_err(|e| format!("open {}: {}", path.display(), e))?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut lines = reader.lines();
    let header_line = match lines.next() {
        Some(Ok(l)) => l,
        Some(Err(e)) => return Err(format!("read {}: {}", path.display(), e)),
        None => return Ok(()),
    };
    let header = parse_csv_line(&header_line);
    let columns: HashMap<String, usize> = header
        .iter()
        .enumerate()
        .map(|(i, name)| (name.to_ascii_uppercase(), i))
        .collect();

    let mut row = CsvRow::new(&columns);
    for line in lines {
        let line = line.map_err(|e| format!("read {}: {}", path.display(), e))?;
        let fields = parse_csv_line(&line);
        row.set_fields(fields);
        on_row(&row);
    }
    Ok(())
}

/// Lookup-by-name row view. Keeps the column→index map out of the loop
/// body so `row.get("LON")` is a single HashMap hit plus a Vec index.
struct CsvRow<'cols> {
    columns: &'cols HashMap<String, usize>,
    fields: Vec<String>,
}

impl<'cols> CsvRow<'cols> {
    fn new(columns: &'cols HashMap<String, usize>) -> Self {
        CsvRow { columns, fields: Vec::new() }
    }
    fn set_fields(&mut self, fields: Vec<String>) {
        self.fields = fields;
    }
    fn get(&self, name: &str) -> Option<&str> {
        self.columns
            .get(name)
            .and_then(|&i| self.fields.get(i))
            .map(|s| s.as_str())
    }
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, in_quotes) {
            ('"', false) => in_quotes = true,
            ('"', true) => {
                if chars.peek() == Some(&'"') {
                    // Escaped quote inside quoted field.
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            (',', false) => {
                out.push(std::mem::take(&mut cur));
            }
            (c, _) => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn write_output(
    records: &[(u64, AddressPoint)],
    strings: &StringPool,
    paths: &BuildOutputPaths,
) -> Result<(), String> {
    let mut points_file = File::create(&paths.points)
        .map_err(|e| format!("create {}: {}", paths.points.display(), e))?;
    let mut cells_file = File::create(&paths.cells)
        .map_err(|e| format!("create {}: {}", paths.cells.display(), e))?;
    let mut entries_file = File::create(&paths.entries)
        .map_err(|e| format!("create {}: {}", paths.entries.display(), e))?;

    let mut current_entry_offset: u32 = 0;
    let mut run_start = 0usize;
    while run_start < records.len() {
        let cell_id = records[run_start].0;
        let mut run_end = run_start + 1;
        while run_end < records.len() && records[run_end].0 == cell_id {
            run_end += 1;
        }
        let run_len = (run_end - run_start).min(u16::MAX as usize);

        cells_file
            .write_all(&cell_id.to_le_bytes())
            .map_err(io_err(&paths.cells))?;
        cells_file
            .write_all(&current_entry_offset.to_le_bytes())
            .map_err(io_err(&paths.cells))?;

        entries_file
            .write_all(&(run_len as u16).to_le_bytes())
            .map_err(io_err(&paths.entries))?;
        for (i, _) in records[run_start..run_end].iter().enumerate() {
            if i >= u16::MAX as usize {
                break;
            }
            let id = (run_start + i) as u32;
            entries_file
                .write_all(&id.to_le_bytes())
                .map_err(io_err(&paths.entries))?;
        }
        current_entry_offset += 2 + (run_len as u32) * 4;

        run_start = run_end;
    }

    for (_, point) in records {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                point as *const AddressPoint as *const u8,
                std::mem::size_of::<AddressPoint>(),
            )
        };
        points_file
            .write_all(bytes)
            .map_err(io_err(&paths.points))?;
    }

    fs::write(&paths.strings, strings.as_bytes()).map_err(io_err(&paths.strings))?;
    Ok(())
}

fn io_err(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("write {}: {}", path.display(), e)
}

struct StringPool {
    data: Vec<u8>,
    index: HashMap<String, u32>,
}

impl StringPool {
    fn new() -> Self {
        // Offset 0 is reserved for the empty string — any `string_id=0`
        // resolves to "" at read time.
        let mut data = Vec::with_capacity(1024);
        data.push(0);
        let mut index = HashMap::new();
        index.insert(String::new(), 0);
        StringPool { data, index }
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

    fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}
