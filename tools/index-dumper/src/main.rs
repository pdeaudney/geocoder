//! Dump the geocoder's mmap'd index to CSV so tools like DuckDB
//! can query it.
//!
//! The server serves these records through `Index::query`; this tool
//! just reinterprets the same `.bin` files through the public
//! `query_server` structs and writes them out row-by-row. No extra
//! decoding, no transformation — what you see in CSV is exactly
//! what the server reads.
//!
//! Usage:
//!   index-dumper <index-dir> [<output-dir>]
//!
//! Defaults: output-dir = `<index-dir>/dump-csv/`.
//!
//! Outputs (all CSV, with header rows):
//!   streets.csv           id, osm_way_id, name, node_count, midpoint_lat, midpoint_lng
//!   place_points.csv      id, name, kind_rank, lat, lng
//!   admin_polygons.csv    id, name, admin_level, country_code, vertex_count, area_sq_deg
//!   addr_points.csv       id, housenumber, street_id, street_name, lat, lng
//!   interp_ways.csv       id, street_id, street_name, start_number, end_number, interpolation, node_count
//!   i18n_names.csv        entity_kind, entity_id, lang, localized_name
//!   summary.txt           row counts per CSV + total byte size
//!
//! Postcode-lookup inspection is deliberately not here: its on-disk
//! records store a `u64` hash of `(state, locality)` instead of the
//! strings, so the mapping isn't losslessly recoverable from the
//! index. Inspect `G-NAF *_LOCALITY_psv.psv` + `*_ADDRESS_DETAIL_psv.psv`
//! directly for that signal.
//!
//! Each CSV is streaming-friendly and compact enough that DuckDB can
//! load them directly with `read_csv_auto`. See
//! `docs/inspection/README.md` for the matching query recipes.

use query_server::{
    as_typed_slice, AddrPoint, AdminPolygon, Index, NodeCoord, PlacePoint, WayHeader,
    DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <index-dir> [<output-dir>]", args[0]);
        std::process::exit(2);
    }
    let index_dir = PathBuf::from(&args[1]);
    let out_dir = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| index_dir.join("dump-csv"));

    if let Err(e) = run(&index_dir, &out_dir) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(index_dir: &Path, out_dir: &Path) -> Result<(), String> {
    let dir_str = index_dir
        .to_str()
        .ok_or_else(|| format!("non-utf8 path: {}", index_dir.display()))?;
    let idx = Index::load(
        dir_str,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )?;

    fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {}", out_dir.display(), e))?;

    let mut summary = Vec::<(String, usize)>::new();

    summary.push(("streets.csv".into(), dump_streets(&idx, &out_dir.join("streets.csv"))?));
    summary.push((
        "place_points.csv".into(),
        dump_place_points(&idx, &out_dir.join("place_points.csv"))?,
    ));
    summary.push((
        "admin_polygons.csv".into(),
        dump_admin_polygons(&idx, &out_dir.join("admin_polygons.csv"))?,
    ));
    summary.push((
        "addr_points.csv".into(),
        dump_addr_points(&idx, &out_dir.join("addr_points.csv"))?,
    ));
    summary.push((
        "i18n_names.csv".into(),
        dump_i18n_names(&idx, &out_dir.join("i18n_names.csv"))?,
    ));

    // --- summary.txt ---
    let total_bytes: u64 = summary
        .iter()
        .map(|(name, _)| fs::metadata(out_dir.join(name)).map(|m| m.len()).unwrap_or(0))
        .sum();
    let summary_path = out_dir.join("summary.txt");
    let mut f = BufWriter::new(
        File::create(&summary_path).map_err(|e| format!("create summary: {e}"))?,
    );
    writeln!(f, "Index dump — {}", index_dir.display()).map_err(io_err)?;
    writeln!(f).map_err(io_err)?;
    writeln!(f, "{:<24} {:>12}  {:>12}", "file", "rows", "bytes").map_err(io_err)?;
    writeln!(f, "{:-<24} {:->12}  {:->12}", "", "", "").map_err(io_err)?;
    for (name, rows) in &summary {
        let bytes = fs::metadata(out_dir.join(name)).map(|m| m.len()).unwrap_or(0);
        writeln!(f, "{name:<24} {rows:>12}  {bytes:>12}").map_err(io_err)?;
    }
    writeln!(f, "{:-<24} {:->12}  {:->12}", "", "", "").map_err(io_err)?;
    writeln!(
        f,
        "{:<24} {:>12}  {:>12}",
        "TOTAL",
        summary.iter().map(|(_, r)| r).sum::<usize>(),
        total_bytes
    )
    .map_err(io_err)?;
    f.flush().map_err(io_err)?;

    eprintln!("==> dump written to {}", out_dir.display());
    eprintln!("    see {} for row counts", summary_path.display());
    Ok(())
}

fn io_err(e: std::io::Error) -> String {
    format!("io: {e}")
}

// -----------------------------------------------------------------------------
// Streets
// -----------------------------------------------------------------------------

fn dump_streets(idx: &Index, path: &Path) -> Result<usize, String> {
    let ways: &[WayHeader] = as_typed_slice(&idx.street_ways);
    let nodes: &[NodeCoord] = as_typed_slice(&idx.street_nodes);

    let mut f = new_csv(path)?;
    writeln!(f, "id,name,node_count,midpoint_lat,midpoint_lng").map_err(io_err)?;

    let mut rows = 0usize;
    for (id, way) in ways.iter().enumerate() {
        let off = way.node_offset as usize;
        let count = way.node_count as usize;
        let mid = if count > 0 && off + count <= nodes.len() {
            Some(nodes[off + count / 2])
        } else {
            None
        };
        let name = idx.get_string(way.name_id);
        let (lat, lng) = match mid {
            Some(n) => (format!("{:.6}", n.lat), format!("{:.6}", n.lng)),
            None => (String::new(), String::new()),
        };
        writeln!(
            f,
            "{id},{name},{count},{lat},{lng}",
            name = csv_escape(name),
        )
        .map_err(io_err)?;
        rows += 1;
    }
    f.flush().map_err(io_err)?;
    Ok(rows)
}

// -----------------------------------------------------------------------------
// Place points
// -----------------------------------------------------------------------------

fn dump_place_points(idx: &Index, path: &Path) -> Result<usize, String> {
    let mut f = new_csv(path)?;
    writeln!(f, "id,name,rank,lat,lng").map_err(io_err)?;

    let Some(pp) = idx.place_points.as_ref() else {
        f.flush().map_err(io_err)?;
        return Ok(0);
    };

    let points: &[PlacePoint] = as_typed_slice(pp);
    for (id, p) in points.iter().enumerate() {
        let name = idx.get_string(p.name_id);
        writeln!(
            f,
            "{id},{name},{rank},{lat:.6},{lng:.6}",
            name = csv_escape(name),
            rank = p.rank,
            lat = p.lat,
            lng = p.lng,
        )
        .map_err(io_err)?;
    }
    f.flush().map_err(io_err)?;
    Ok(points.len())
}

// -----------------------------------------------------------------------------
// Admin polygons
// -----------------------------------------------------------------------------

fn dump_admin_polygons(idx: &Index, path: &Path) -> Result<usize, String> {
    let polys: &[AdminPolygon] = as_typed_slice(&idx.admin_polygons);

    let mut f = new_csv(path)?;
    writeln!(f, "id,name,admin_level,country_code,vertex_count,area_sq_deg").map_err(io_err)?;

    for (id, p) in polys.iter().enumerate() {
        let name = idx.get_string(p.name_id);
        let cc = if p.country_code != 0 {
            let bytes = [(p.country_code >> 8) as u8, (p.country_code & 0xFF) as u8];
            std::str::from_utf8(&bytes).unwrap_or("").to_owned()
        } else {
            String::new()
        };
        writeln!(
            f,
            "{id},{name},{lvl},{cc},{vc},{area:.6}",
            name = csv_escape(name),
            lvl = p.admin_level,
            cc = cc,
            vc = p.vertex_count,
            area = p.area,
        )
        .map_err(io_err)?;
    }
    f.flush().map_err(io_err)?;
    Ok(polys.len())
}

// -----------------------------------------------------------------------------
// Address points (OSM-derived; G-NAF and OpenAddresses live in their own
// subsystems with their own tooling — out of scope for this dumper).
// -----------------------------------------------------------------------------

fn dump_addr_points(idx: &Index, path: &Path) -> Result<usize, String> {
    let addrs: &[AddrPoint] = as_typed_slice(&idx.addr_points);

    let mut f = new_csv(path)?;
    // street_or_place_id is the interned name string (street name, or
    // place name when flags & FLAG_ADDR_PLACE) — it is NOT a way index.
    writeln!(
        f,
        "id,housenumber,street_or_place,is_addr_place,is_housename,unit,floor,parent_place,lat,lng"
    )
    .map_err(io_err)?;

    for (id, a) in addrs.iter().enumerate() {
        let hn = idx.get_string(a.housenumber_id);
        let primary = idx.get_string(a.street_or_place_id);
        let unit = if a.unit_id != 0 { idx.get_string(a.unit_id) } else { "" };
        let floor = if a.floor_id != 0 { idx.get_string(a.floor_id) } else { "" };
        let parent = if a.parent_place_id != 0 {
            idx.get_string(a.parent_place_id)
        } else {
            ""
        };
        let is_place = a.flags & query_server::FLAG_ADDR_PLACE != 0;
        let is_housename = a.flags & query_server::FLAG_IS_HOUSENAME != 0;
        writeln!(
            f,
            "{id},{hn},{primary},{is_place},{is_housename},{unit},{floor},{parent},{lat:.6},{lng:.6}",
            hn = csv_escape(hn),
            primary = csv_escape(primary),
            unit = csv_escape(unit),
            floor = csv_escape(floor),
            parent = csv_escape(parent),
            lat = a.lat,
            lng = a.lng,
        )
        .map_err(io_err)?;
    }
    f.flush().map_err(io_err)?;
    Ok(addrs.len())
}

// -----------------------------------------------------------------------------
// i18n names (name:<lang> tags from OSM, emitted by the C++ indexer).
// Layout accessed here is intentionally format-stable with the i18n module;
// see server/src/i18n.rs.
// -----------------------------------------------------------------------------

fn dump_i18n_names(idx: &Index, path: &Path) -> Result<usize, String> {
    let mut f = new_csv(path)?;
    writeln!(f, "entity_kind,entity_id,lang,localized_name").map_err(io_err)?;

    let Some(i18n) = idx.i18n_names.as_ref() else {
        f.flush().map_err(io_err)?;
        return Ok(0);
    };

    let mut rows = 0usize;
    for rec in i18n.records() {
        let kind_label = match rec.entity_type {
            query_server::i18n::ENTITY_ADMIN => "admin",
            query_server::i18n::ENTITY_PLACE => "place",
            _ => "other",
        };
        // lang_code is packed `a | (b << 8)` lowercase ASCII.
        let lang = [
            (rec.lang_code & 0xFF) as u8,
            ((rec.lang_code >> 8) & 0xFF) as u8,
        ];
        let lang_str = std::str::from_utf8(&lang).unwrap_or("??");
        let name = idx.get_string(rec.name_id);
        writeln!(
            f,
            "{kind_label},{id},{lang_str},{name}",
            id = rec.entity_id,
            name = csv_escape(name),
        )
        .map_err(io_err)?;
        rows += 1;
    }
    f.flush().map_err(io_err)?;
    Ok(rows)
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn new_csv(path: &Path) -> Result<BufWriter<File>, String> {
    let f = File::create(path).map_err(|e| format!("create {}: {}", path.display(), e))?;
    Ok(BufWriter::new(f))
}

/// Minimal CSV field escaping: wrap in quotes if the field contains a
/// comma, quote, or newline; double any embedded quotes. Good enough for
/// DuckDB's default `read_csv_auto`.
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}
