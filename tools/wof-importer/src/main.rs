//! Import Who's on First country-level polygons as an admin fallback.
//!
//! Pelias's approach to admin lookup is to ignore OSM's hierarchy
//! entirely and use WoF polygons for every country/region/county/locality.
//! We adopt a scoped-down version: WoF *country* polygons only, used
//! as a fallback when our OSM-derived admin lookup returns no
//! `country_code` (the common case for Geofabrik country extracts
//! that don't include the `admin_level=2` relation).
//!
//! Input: a directory of per-country WoF SQLite files. Each file
//! contains an `spr` table (standard places response) + a `geojson`
//! table (raw polygon bodies). We filter to `placetype='country'`
//! and `is_current=1`.
//!
//! Output: `wof_countries.bin` + `wof_countries_vertices.bin` +
//! `wof_countries_strings.bin` in a target index directory. Schema
//! deliberately mirrors our existing `AdminPolygon` record layout
//! so the server reads the file the same way.
//!
//! Usage:
//!   wof-importer <wof-sqlite-dir> <output-index-dir>

use rusqlite::Connection;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Mirrors `query_server::AdminPolygon`. Kept as a local repr here so
/// the importer has no dependency on the server crate — it's a pure
/// file-format transform.
#[repr(C)]
#[derive(Clone, Copy)]
struct WofCountryPolygon {
    vertex_offset: u32,
    // u32 not u16: WoF country rings can exceed 65 k vertices
    // (UK mainland is ~297 k). Must match `WofCountry` in server.
    vertex_count: u32,
    name_id: u32,
    admin_level: u8,
    area: f32,
    country_code: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NodeCoord {
    lat: f32,
    lng: f32,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <wof-sqlite-dir> <output-index-dir>", args[0]);
        std::process::exit(2);
    }
    let sqlite_dir = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);

    if let Err(e) = run(&sqlite_dir, &out_dir) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(sqlite_dir: &Path, out_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {}", out_dir.display(), e))?;

    // Scan sqlite_dir for all `whosonfirst-data-admin-*.db` files.
    let mut dbs: Vec<PathBuf> = fs::read_dir(sqlite_dir)
        .map_err(|e| format!("read_dir {}: {}", sqlite_dir.display(), e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("db")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("whosonfirst-data-admin-"))
        })
        .collect();
    dbs.sort();

    if dbs.is_empty() {
        return Err(format!(
            "no whosonfirst-data-admin-*.db files in {}",
            sqlite_dir.display()
        ));
    }

    let mut polygons: Vec<WofCountryPolygon> = Vec::new();
    let mut vertices: Vec<NodeCoord> = Vec::new();
    let mut strings: Vec<u8> = vec![0]; // reserve offset 0 for the empty string
    // Each WoF per-country SQLite file ships country polygons for
    // *every* country in its parent hierarchy, which means the US
    // country entity appears identically in AU's, GB's, CA's, NZ's,
    // and US's SQLite files. Skip repeats so one WoF country entity
    // contributes exactly one set of polygon rings to the output.
    let mut seen_ccs: std::collections::HashSet<u16> =
        std::collections::HashSet::new();

    for db_path in &dbs {
        eprintln!("==> {}", db_path.display());
        let rows_before = polygons.len();
        import_one_db(
            db_path,
            &mut polygons,
            &mut vertices,
            &mut strings,
            &mut seen_ccs,
        )?;
        eprintln!(
            "    +{} polygons, cumulative total: {}",
            polygons.len() - rows_before,
            polygons.len()
        );
    }

    let polygons_path = out_dir.join("wof_countries.bin");
    let vertices_path = out_dir.join("wof_countries_vertices.bin");
    let strings_path = out_dir.join("wof_countries_strings.bin");

    write_struct_array(&polygons_path, &polygons)?;
    write_struct_array(&vertices_path, &vertices)?;
    fs::write(&strings_path, &strings)
        .map_err(|e| format!("write {}: {}", strings_path.display(), e))?;

    eprintln!(
        "\nwrote {} polygons ({} distinct countries), {} vertices, {} string bytes",
        polygons.len(),
        seen_ccs.len(),
        vertices.len(),
        strings.len()
    );
    Ok(())
}

fn import_one_db(
    db_path: &Path,
    polygons: &mut Vec<WofCountryPolygon>,
    vertices: &mut Vec<NodeCoord>,
    strings: &mut Vec<u8>,
    seen_ccs: &mut std::collections::HashSet<u16>,
) -> Result<(), String> {
    let conn =
        Connection::open(db_path).map_err(|e| format!("open {}: {}", db_path.display(), e))?;

    // Current, non-deprecated, primary (not alt) country geometries only.
    let sql = r#"
        SELECT spr.id, spr.name, spr.country, g.body
        FROM spr
        JOIN geojson g ON g.id = spr.id AND g.is_alt = 0
        WHERE spr.placetype = 'country'
          AND spr.is_current = 1
          AND spr.is_deprecated = 0
          AND spr.is_ceased = 0
    "#;
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
    let rows_iter = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| format!("query: {e}"))?;

    for row in rows_iter {
        let (_wof_id, name, cc, body) = row.map_err(|e| format!("row: {e}"))?;

        // Pack `cc` into u16. Skip entries whose country column isn't a
        // clean 2-letter ISO 3166-1 alpha-2.
        let cc_packed = match pack_cc(&cc) {
            Some(c) => c,
            None => {
                eprintln!("    skip {name}: non-ISO country code {cc:?}");
                continue;
            }
        };

        // Skip if we already ingested this cc from a previous SQLite
        // (WoF per-country files redundantly ship the full parent
        // hierarchy — every US country polygon appears in 5 of our 5
        // input files).
        if !seen_ccs.insert(cc_packed) {
            continue;
        }

        let gj: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("    skip {name}: geojson parse: {e}");
                continue;
            }
        };

        let rings = extract_outer_rings(&gj);
        if rings.is_empty() {
            eprintln!("    skip {name}: no usable outer rings in geometry");
            continue;
        }

        let name_id = intern_string(strings, &name);

        for ring in rings {
            // Douglas-Peucker simplify: country-level point-in-polygon
            // tolerates ~km-scale boundary approximation. The UK
            // mainland raw ring is ~297 k vertices, simplified to
            // ~5 k at eps=0.01° (~1 km). Cuts PIP cost by ~60× at
            // build time (5M+ find_country calls during autocomplete
            // FST build).
            let simplified = simplify_dp(&ring, 0.01);

            // Any ring that collapses to <3 distinct points isn't a
            // valid polygon — skip it. Shouldn't happen for real
            // country boundaries, but defensive.
            if simplified.len() < 3 {
                continue;
            }

            let vertex_offset = vertices.len() as u32;
            for (lng, lat) in &simplified {
                vertices.push(NodeCoord {
                    lat: *lat as f32,
                    lng: *lng as f32,
                });
            }
            let vertex_count = vertices.len() as u32 - vertex_offset;
            let area = approx_area_sq_deg(&simplified);
            polygons.push(WofCountryPolygon {
                vertex_offset,
                vertex_count,
                name_id,
                admin_level: 2,
                area,
                country_code: cc_packed,
            });
        }
    }

    Ok(())
}

/// Pack a 2-character ISO 3166-1 alpha-2 string (e.g. "AU", "US", "GB")
/// into a single u16: high byte is the first character (uppercase),
/// low byte is the second. Returns None if the string isn't exactly
/// two ASCII letters. This matches the packing used by the C++ builder
/// in `build_index.cpp` so the server reads both sources identically.
fn pack_cc(cc: &str) -> Option<u16> {
    let bytes = cc.as_bytes();
    if bytes.len() != 2 {
        return None;
    }
    let a = bytes[0].to_ascii_uppercase();
    let b = bytes[1].to_ascii_uppercase();
    if !(a.is_ascii_alphabetic() && b.is_ascii_alphabetic()) {
        return None;
    }
    Some(((a as u16) << 8) | (b as u16))
}

/// Return every outer ring from a GeoJSON `Polygon` or `MultiPolygon`
/// as a Vec<(lng, lat)>. Holes are deliberately dropped — for
/// country-level point-in-polygon, the outer ring is the contract;
/// treating holes as "outside the country" is wrong only for things
/// like Lesotho inside South Africa, which we don't care about at the
/// country-code fallback level (Lesotho has its own WoF country
/// polygon that we import separately).
fn extract_outer_rings(gj: &Value) -> Vec<Vec<(f64, f64)>> {
    let geom = gj
        .get("geometry")
        .or(Some(gj)) // handle both wrapped and bare geometries
        .cloned()
        .unwrap_or(Value::Null);
    let gtype = geom
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let coords = match geom.get("coordinates") {
        Some(c) => c,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    match gtype {
        "Polygon" => {
            if let Some(ring) = coords.get(0) {
                if let Some(pts) = parse_ring(ring) {
                    out.push(pts);
                }
            }
        }
        "MultiPolygon" => {
            if let Some(polys) = coords.as_array() {
                for poly in polys {
                    if let Some(ring) = poly.get(0) {
                        if let Some(pts) = parse_ring(ring) {
                            out.push(pts);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn parse_ring(v: &Value) -> Option<Vec<(f64, f64)>> {
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for pair in arr {
        let p = pair.as_array()?;
        let lng = p.get(0)?.as_f64()?;
        let lat = p.get(1)?.as_f64()?;
        out.push((lng, lat));
    }
    if out.len() < 3 {
        return None;
    }
    Some(out)
}

/// Shoelace formula on (lng, lat) in degrees. Used purely to sort
/// candidate polygons at runtime (larger first → more likely match
/// first → early-exit wins). Not a real-world area measurement.
fn approx_area_sq_deg(ring: &[(f64, f64)]) -> f32 {
    let mut sum = 0.0f64;
    let n = ring.len();
    for i in 0..n {
        let (x1, y1) = ring[i];
        let (x2, y2) = ring[(i + 1) % n];
        sum += x1 * y2 - x2 * y1;
    }
    (sum.abs() / 2.0) as f32
}

/// Iterative Douglas-Peucker polyline simplification using a 2D
/// euclidean perpendicular-distance metric on (lng, lat) in degrees.
/// For country-level boundaries at eps ≈ 0.01° (~1 km) the loss of
/// precision doesn't matter for PIP — a point straying that close to
/// the boundary is near the coastline either way. Returns the closed
/// ring minus duplicated endpoints.
fn simplify_dp(ring: &[(f64, f64)], eps: f64) -> Vec<(f64, f64)> {
    if ring.len() < 3 {
        return ring.to_vec();
    }
    let mut keep = vec![false; ring.len()];
    keep[0] = true;
    keep[ring.len() - 1] = true;

    // Explicit stack avoids deep recursion on the US mainland's
    // ~300 k-vertex input.
    let mut stack: Vec<(usize, usize)> = vec![(0, ring.len() - 1)];
    while let Some((i, j)) = stack.pop() {
        if j <= i + 1 {
            continue;
        }
        let (ax, ay) = ring[i];
        let (bx, by) = ring[j];
        // |ab| (squared avoids the sqrt on the hot path).
        let abx = bx - ax;
        let aby = by - ay;
        let ab_len_sq = abx * abx + aby * aby;

        let mut max_d = 0.0f64;
        let mut max_k = i;
        for k in (i + 1)..j {
            let (px, py) = ring[k];
            // Perpendicular distance from p to segment a-b.
            let d = if ab_len_sq < f64::EPSILON {
                // Degenerate segment: a == b, just use point-point distance.
                ((px - ax).powi(2) + (py - ay).powi(2)).sqrt()
            } else {
                let cross = (abx * (py - ay)) - (aby * (px - ax));
                cross.abs() / ab_len_sq.sqrt()
            };
            if d > max_d {
                max_d = d;
                max_k = k;
            }
        }
        if max_d > eps {
            keep[max_k] = true;
            stack.push((i, max_k));
            stack.push((max_k, j));
        }
    }

    ring.iter()
        .zip(keep.iter())
        .filter_map(|(pt, &k)| if k { Some(*pt) } else { None })
        .collect()
}

fn intern_string(strings: &mut Vec<u8>, s: &str) -> u32 {
    let off = strings.len() as u32;
    strings.extend_from_slice(s.as_bytes());
    strings.push(0);
    off
}

fn write_struct_array<T: Copy>(path: &Path, slice: &[T]) -> Result<(), String> {
    let mut f = BufWriter::new(
        File::create(path).map_err(|e| format!("create {}: {}", path.display(), e))?,
    );
    let byte_ptr = slice.as_ptr() as *const u8;
    let byte_len = std::mem::size_of_val(slice);
    // SAFETY: T is Copy/repr(C), `slice.as_ptr()` is non-null and
    // `byte_len` is the exact element size × element count. No aliasing
    // concern: we only read, and no concurrent writers exist.
    let bytes = unsafe { std::slice::from_raw_parts(byte_ptr, byte_len) };
    f.write_all(bytes)
        .map_err(|e| format!("write {}: {}", path.display(), e))?;
    f.flush()
        .map_err(|e| format!("flush {}: {}", path.display(), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_cc_handles_cases() {
        assert_eq!(pack_cc("AU"), Some(('A' as u16) << 8 | ('U' as u16)));
        assert_eq!(pack_cc("au"), Some(('A' as u16) << 8 | ('U' as u16)));
        assert_eq!(pack_cc(""), None);
        assert_eq!(pack_cc("A"), None);
        assert_eq!(pack_cc("AUS"), None);
        assert_eq!(pack_cc("A1"), None);
    }

    #[test]
    fn parse_polygon_ring() {
        let gj: Value = serde_json::from_str(
            r#"{"type":"Polygon","coordinates":[[[1.0,2.0],[3.0,4.0],[5.0,6.0],[1.0,2.0]]]}"#,
        )
        .unwrap();
        let rings = extract_outer_rings(&gj);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].len(), 4);
    }

    #[test]
    fn parse_multipolygon_all_outer_rings() {
        let gj: Value = serde_json::from_str(
            r#"{"type":"MultiPolygon","coordinates":[
                [[[1.0,2.0],[3.0,4.0],[5.0,6.0],[1.0,2.0]]],
                [[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,0.0]]]
            ]}"#,
        )
        .unwrap();
        let rings = extract_outer_rings(&gj);
        assert_eq!(rings.len(), 2);
    }
}
