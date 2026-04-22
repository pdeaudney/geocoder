//! Who's on First country-level admin fallback.
//!
//! When our OSM-derived admin lookup returns no `country_code` — which
//! happens whenever a Geofabrik country extract doesn't include its
//! own `admin_level=2` boundary relation (Great Britain, USA) — this
//! module provides a secondary lookup against country polygons
//! imported from Who's on First. Pelias's long-standing pattern of
//! relying on WoF for the admin hierarchy informs the choice.
//!
//! Deliberately scoped to country-level polygons only; finer WoF
//! levels would duplicate OSM admin coverage, which is already dense
//! enough inside our country extracts. The goal is to fill the
//! country-code gap, not to replace the OSM admin stack.
//!
//! On-disk layout (produced by `tools/wof-importer`):
//!
//! - `wof_countries.bin`          — array of [`WofCountry`] records
//! - `wof_countries_vertices.bin` — packed [`NodeCoord`] vertex lists
//! - `wof_countries_strings.bin`  — NUL-terminated UTF-8 string pool
//!
//! Record layout is intentionally the same width as
//! [`crate::AdminPolygon`] so the importer can mirror the serialisation
//! without cross-crate coupling.

use crate::{as_typed_slice, point_in_polygon_f64, NodeCoord};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WofCountry {
    pub vertex_offset: u32,
    /// Unlike `AdminPolygon::vertex_count` (which is `u16`), this is
    /// a `u32`: WoF country rings routinely exceed 65 k vertices —
    /// the UK mainland alone is ~297 k — and a `u16` silently wraps.
    pub vertex_count: u32,
    pub name_id: u32,
    /// Always 2 for country-level. Kept so the record stays roughly
    /// shape-compatible with `AdminPolygon`; readers treat it as a
    /// constant.
    pub admin_level: u8,
    /// Shoelace area in deg² for rough ordering. Not a real geodesic
    /// area.
    pub area: f32,
    /// Packed ISO 3166-1 alpha-2: high byte is the first letter
    /// (uppercase), low byte is the second. Matches the packing used
    /// by the C++ `build-index` so the rest of the runtime can treat
    /// both sources uniformly.
    pub country_code: u16,
}

pub struct WofCountries {
    polygons: Mmap,
    vertices: Mmap,
    strings: Mmap,
    /// Precomputed bounding boxes per polygon, sorted by area
    /// descending. Each entry is
    /// `(poly_index, min_lat, max_lat, min_lng, max_lng)`.
    ///
    /// Without this, every `find_country` call would scan every
    /// polygon's vertex list to compute a bbox on the fly — for
    /// polygons like the UK mainland (~297 k vertices) that's 297 k
    /// float comparisons per call. Index builders that call
    /// `find_admin` for every street/place (millions of times) quickly
    /// hit a wall. Precomputing once makes each bbox check O(1) so the
    /// whole scan is ~7 k × 4-compare = tens of microseconds.
    sorted_bboxes: Vec<(u32, f64, f64, f64, f64)>,
}

/// Lightweight match result — mirrors what `AdminResult` carries for
/// country, minus the poly-id plumbing (WoF polys live in a separate
/// id space and don't feed back into `poly_ids` for i18n lookups).
#[derive(Clone, Copy)]
pub struct WofCountryMatch<'a> {
    pub name: &'a str,
    pub country_code: [u8; 2],
}

impl WofCountries {
    pub fn open(dir: &Path) -> Result<Option<Self>, String> {
        let polygons_path = dir.join("wof_countries.bin");
        let vertices_path = dir.join("wof_countries_vertices.bin");
        let strings_path = dir.join("wof_countries_strings.bin");

        if !polygons_path.exists() || !vertices_path.exists() || !strings_path.exists() {
            return Ok(None);
        }

        let polygons =
            mmap_file(&polygons_path).map_err(|e| format!("wof polygons: {e}"))?;
        let vertices =
            mmap_file(&vertices_path).map_err(|e| format!("wof vertices: {e}"))?;
        let strings = mmap_file(&strings_path).map_err(|e| format!("wof strings: {e}"))?;

        // Width sanity: WofCountry is 20 bytes, NodeCoord is 8 bytes.
        if polygons.len() % std::mem::size_of::<WofCountry>() != 0 {
            return Err(format!(
                "wof_countries.bin size {} not a multiple of {}",
                polygons.len(),
                std::mem::size_of::<WofCountry>()
            ));
        }
        if vertices.len() % std::mem::size_of::<NodeCoord>() != 0 {
            return Err(format!(
                "wof_countries_vertices.bin size {} not a multiple of {}",
                vertices.len(),
                std::mem::size_of::<NodeCoord>()
            ));
        }

        let polys: &[WofCountry] = as_typed_slice(&polygons);
        let verts: &[NodeCoord] = as_typed_slice(&vertices);
        let mut sorted_bboxes: Vec<(u32, f64, f64, f64, f64)> =
            Vec::with_capacity(polys.len());
        for (idx, p) in polys.iter().enumerate() {
            let off = p.vertex_offset as usize;
            let count = p.vertex_count as usize;
            if count == 0 || off + count > verts.len() {
                continue;
            }
            let ring = &verts[off..off + count];
            let mut min_lat = f64::INFINITY;
            let mut max_lat = f64::NEG_INFINITY;
            let mut min_lng = f64::INFINITY;
            let mut max_lng = f64::NEG_INFINITY;
            for v in ring {
                let la = v.lat as f64;
                let ln = v.lng as f64;
                if la < min_lat { min_lat = la; }
                if la > max_lat { max_lat = la; }
                if ln < min_lng { min_lng = ln; }
                if ln > max_lng { max_lng = ln; }
            }
            sorted_bboxes.push((
                idx as u32,
                min_lat,
                max_lat,
                min_lng,
                max_lng,
            ));
        }
        // Sort by area descending (proxy: bbox area). Larger landmasses
        // first means the common case (a point inside a country's main
        // polygon) hits on an early iteration.
        sorted_bboxes.sort_by(|a, b| {
            let area_a = (a.2 - a.1) * (a.4 - a.3);
            let area_b = (b.2 - b.1) * (b.4 - b.3);
            area_b.partial_cmp(&area_a).unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(Some(WofCountries {
            polygons,
            vertices,
            strings,
            sorted_bboxes,
        }))
    }

    /// Point-in-polygon scan across every WoF country. Linear scan —
    /// the global WoF-country count is ~250 and each PIP is a
    /// no-op except for polygons whose bbox contains the point, so
    /// expected cost is roughly `O(bbox-check × 250)` with a handful
    /// of real PIP evaluations. Fine for a fallback path that only
    /// fires when OSM didn't resolve the country.
    pub fn find_country(&self, lat: f64, lng: f64) -> Option<WofCountryMatch<'_>> {
        let polys: &[WofCountry] = as_typed_slice(&self.polygons);
        let all_vertices: &[NodeCoord] = as_typed_slice(&self.vertices);
        if polys.is_empty() {
            return None;
        }

        // Fast path: iterate the precomputed (poly_idx, bbox) tuples.
        // bbox check is 4 compares per poly; PIP only runs on bbox hits.
        for &(idx, min_lat, max_lat, min_lng, max_lng) in &self.sorted_bboxes {
            if lat < min_lat || lat > max_lat || lng < min_lng || lng > max_lng {
                continue;
            }
            let p = polys[idx as usize];
            let off = p.vertex_offset as usize;
            let count = p.vertex_count as usize;
            let ring = &all_vertices[off..off + count];
            if !point_in_polygon_f64(lat, lng, ring) {
                continue;
            }
            let cc = [(p.country_code >> 8) as u8, (p.country_code & 0xFF) as u8];
            let name = read_cstr(&self.strings, p.name_id as usize);
            return Some(WofCountryMatch { name, country_code: cc });
        }
        None
    }
}

fn mmap_file(path: &Path) -> Result<Mmap, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {}", path.display(), e))?;
    unsafe { Mmap::map(&file) }.map_err(|e| format!("mmap {}: {}", path.display(), e))
}

fn read_cstr(pool: &[u8], offset: usize) -> &str {
    let bytes = pool.get(offset..).unwrap_or(&[]);
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

