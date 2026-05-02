//! Core reverse-geocoding library used by the query-server binary, tests, and benches.
//!
//! This module owns the binary index format, S2 helpers, distance math, and the
//! query entry points (`Index::query`, `Index::query_geo`, `Index::find_admin`).
//! It is deliberately `pub` in the crate root so integration tests and Criterion
//! benchmarks in `benches/` can exercise the same code paths the HTTP handler
//! uses.

use memmap2::Mmap;
use s2::cellid::CellID;
use s2::latlng::LatLng;
use serde::Serialize;
use std::borrow::Cow;
use std::fs::File;

pub mod address_points;
pub mod admin_config;
pub mod autocomplete;
pub mod fetcher;
pub mod geo;
pub mod gnaf;
pub mod i18n;
pub mod ip_geo;
pub mod h3_cell;
pub mod limits;
pub mod manifest;
pub mod metrics;
pub mod openaddresses;
pub mod postcode;
pub mod shadow;
pub mod telemetry;
#[cfg(feature = "translit")]
pub mod translit;
pub mod wof_countries;

#[cfg(feature = "grpc")]
pub mod grpc_service;
use admin_config::{AdminConfig, AdminField};
use gnaf::Gnaf;
use i18n::{pack_lang_code, I18nNames, ALIAS_PRIMARY, ENTITY_ADMIN};
use openaddresses::OpenAddresses;
use postcode::PostcodeLookup;
use std::path::Path;

#[cfg(feature = "forward")]
pub mod forward;

// --- Defaults ---

pub const DEFAULT_STREET_CELL_LEVEL: u64 = 17;
pub const DEFAULT_ADMIN_CELL_LEVEL: u64 = 10;
pub const DEFAULT_SEARCH_DISTANCE: f64 = 75.0;

// --- S2 helpers ---

pub fn cell_id_at_level(lat: f64, lng: f64, level: u64) -> u64 {
    let ll = LatLng::from_degrees(lat, lng);
    CellID::from(ll).parent(level).0
}

pub fn cell_neighbors_at_level(cell_id: u64, level: u64) -> Vec<u64> {
    let cell = CellID(cell_id);
    cell.all_neighbors(level).into_iter().map(|c| c.0).collect()
}

// --- Binary format structs (must match C++ build pipeline) ---

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WayHeader {
    pub node_offset: u32,
    pub node_count: u8,
    pub name_id: u32,
}

/// AddrPoint flag bits stored in `AddrPoint.flags`. These let one
/// 32-byte record represent both `addr:street` and `addr:place`
/// addresses without forking the on-disk format. Mirrors `FLAG_*`
/// constants in `builder/src/build_index.cpp`.
pub const FLAG_ADDR_PLACE: u8 = 0x01;
pub const FLAG_IS_HOUSENAME: u8 = 0x02;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AddrPoint {
    pub lat: f32,
    pub lng: f32,
    pub housenumber_id: u32,
    /// Street name (default), or place name when `flags & FLAG_ADDR_PLACE`.
    /// Resolves through `Index::get_string`.
    pub street_or_place_id: u32,
    /// `addr:unit` / `addr:flat` / `addr:door`, 0 if absent.
    pub unit_id: u32,
    /// `addr:floor` / `addr:level`, 0 if absent.
    pub floor_id: u32,
    /// Tagged parent locality from `addr:city|suburb|locality|state`,
    /// 0 if absent. Forward indexer prefers this over geometric
    /// `find_admin()` enrichment when non-zero.
    pub parent_place_id: u32,
    pub flags: u8,
    pub _pad: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InterpWay {
    pub node_offset: u32,
    pub node_count: u8,
    pub street_id: u32,
    pub start_number: u32,
    pub end_number: u32,
    pub interpolation: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AdminPolygon {
    pub vertex_offset: u32,
    pub vertex_count: u16,
    pub name_id: u32,
    pub admin_level: u8,
    pub area: f32,
    pub country_code: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NodeCoord {
    pub lat: f32,
    pub lng: f32,
}

/// A `place=*` locality point (city, town, suburb, etc.). Used as a
/// nearest-neighbour fallback when admin-polygon lookup didn't produce a
/// city — mostly matters in rural areas where no level-N boundary covers
/// the query.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlacePoint {
    pub lat: f32,
    pub lng: f32,
    pub name_id: u32,
    /// Nominatim-style address rank: 16 = city/town/village, 19 = suburb,
    /// 20 = hamlet. Lower rank = larger/more prominent feature.
    pub rank: u8,
    _pad: [u8; 3],
}

/// A POI (amenity/shop/tourism/aeroway/historic/leisure/office/
/// healthcare/military/man_made/railway-non-track/natural-subset/
/// waterway-subset). Indexed at the same S2 cell level as addresses
/// and streets so /reverse can do a 9-cell neighbour lookup.
///
/// `rank` mirrors the Nominatim importance scale loosely: 10 means
/// the POI is wikidata- or wikipedia-backed (almost always the right
/// answer for autocomplete), 15 is everything else. The autocomplete
/// FST builder gates on rank ≤ 10 to keep the FST size bounded.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PoiPoint {
    pub lat: f32,
    pub lng: f32,
    pub name_id: u32,
    /// Interned `<key>:<value>` string (e.g. `"amenity:cafe"`).
    pub category_id: u32,
    pub rank: u8,
    _pad: [u8; 3],
    /// Tagged parent locality from `addr:city|suburb|locality`, 0 if
    /// absent. When zero, forward indexer falls back to geometric
    /// `find_admin()` enrichment.
    pub parent_place_id: u32,
}

// --- Index data ---

pub struct Index {
    pub geo_cells: Mmap,
    pub street_entries: Mmap,
    pub street_ways: Mmap,
    pub street_nodes: Mmap,
    pub addr_entries: Mmap,
    pub addr_points: Mmap,
    pub interp_entries: Mmap,
    pub interp_ways: Mmap,
    pub interp_nodes: Mmap,
    pub admin_cells: Mmap,
    pub admin_entries: Mmap,
    pub admin_polygons: Mmap,
    pub admin_vertices: Mmap,
    // `place_*` files are optional — old indexes predate them. When None,
    // `find_place` is a no-op and the server falls back to admin-only.
    pub place_cells: Option<Mmap>,
    pub place_entries: Option<Mmap>,
    pub place_points: Option<Mmap>,
    // `poi_*` files are optional in the same sense — indexes built
    // before commit 4 don't have them and `find_poi` returns None
    // accordingly. /reverse responses simply omit the `poi` field on
    // those deployments.
    pub poi_cells: Option<Mmap>,
    pub poi_entries: Option<Mmap>,
    pub poi_points: Option<Mmap>,
    pub strings: Mmap,
    pub street_cell_level: u64,
    pub admin_cell_level: u64,
    pub max_distance_sq: f64,
    pub admin_config: AdminConfig,
    /// Optional G-NAF-derived postcode lookup. Used to populate postcode
    /// on reverse queries when the admin polygon scan doesn't find one
    /// (common for AU — OSM's `boundary=postal_code` coverage is <5%).
    pub postcode_lookup: Option<PostcodeLookup>,
    /// Optional G-NAF address-point index. When present, provides
    /// authoritative AU geocodes: exact lat/lng for any house number and
    /// per-address postcodes (more accurate than the suburb-modal fallback
    /// in `postcode_lookup`).
    pub gnaf: Option<Gnaf>,
    /// Optional per-country address-point indexes derived from
    /// OpenAddresses.io. Covers ~60 countries with authoritative address
    /// datasets. AU entries here would duplicate G-NAF; in practice a
    /// deployment picks one or the other per country.
    pub open_addresses: Option<OpenAddresses>,
    /// Optional localized-name index (`name:<lang>` tags from OSM).
    /// Loaded when `i18n_names.bin` is present. `query_with_lang` uses
    /// it to override the default name for a given entity when the
    /// caller passes `lang=`.
    pub i18n_names: Option<I18nNames>,
    /// Optional Who's on First country-level polygon fallback. When
    /// present, `find_admin` consults it after the OSM admin scan
    /// to fill in a `country_code` that would otherwise be missing
    /// (common for Geofabrik country extracts that don't include
    /// the `admin_level=2` relation — Great Britain, USA). See
    /// [`wof_countries`] for the on-disk layout.
    pub wof_countries: Option<crate::wof_countries::WofCountries>,
}

// Typed views over the record-array mmaps. Reconstructed once per query at
// the top of `query()` — Rust can't express a safe self-referential
// `&'_ [T]` on `Index` without an external self-ref crate, and the cast
// itself is effectively free (one pointer + division). The previous concern
// was unsafe on the hot path; it is now confined to these helpers.
#[inline]
pub fn as_typed_slice<T: Copy>(mmap: &Mmap) -> &[T] {
    unsafe {
        std::slice::from_raw_parts(
            mmap.as_ptr() as *const T,
            mmap.len() / std::mem::size_of::<T>(),
        )
    }
}

pub const NO_DATA: u32 = 0xFFFFFFFF;

/// Upper bound on distinct street IDs seen across the 9-cell neighbourhood
/// scan in `query_geo`. Sized well above observed urban worst case (~200).
/// If exceeded, we gracefully stop deduping (extra streets may be re-scanned
/// but the result is still correct).
const SEEN_STREETS_CAP: usize = 256;

#[inline]
fn seen_streets_contains(seen: &[u32], id: u32) -> bool {
    // Linear scan. N is bounded by SEEN_STREETS_CAP and a u32 equality check
    // is single-cycle, so this is consistently faster than hashing for the
    // sizes we see in practice.
    seen.iter().any(|&s| s == id)
}

pub struct GeoCellOffsets {
    pub street: u32,
    pub addr: u32,
    pub interp: u32,
}

fn mmap_file(path: &str) -> Result<Mmap, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open {}: {}", path, e))?;
    let mmap = unsafe { Mmap::map(&file).map_err(|e| format!("Failed to mmap {}: {}", path, e))? };
    log_loaded_file("reverse", path, mmap.len() as u64);
    Ok(mmap)
}

fn mmap_file_optional(path: &str) -> Option<Mmap> {
    let mmap = File::open(path).ok().and_then(|f| unsafe { Mmap::map(&f).ok() })?;
    log_loaded_file("reverse", path, mmap.len() as u64);
    Some(mmap)
}

/// Emit one structured log line per index file loaded at startup. Operators
/// compare this manifest against an expected build to spot truncated /
/// wrong-version files: a 0-byte `addr_points.bin` or an unexpectedly
/// small `strings.bin` jumps out instantly.
///
/// Public so optional companion indexes (FST autocomplete, MaxMind, …)
/// can emit the same shape from their own load paths.
pub fn log_loaded_file(index: &'static str, path: &str, size_bytes: u64) {
    let mtime_unix = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    tracing::info!(
        target: "query_server::manifest",
        index,
        path,
        size_bytes,
        mtime_unix,
        "loaded index file"
    );
}

/// Mmap a record-array file and verify its length is a positive multiple
/// of `record_size`. Truncated or zero-length indexes otherwise turn into
/// silently-short typed slices that serve partial data; we'd rather
/// refuse to start than mmap corruption.
fn mmap_records(path: &str, record_size: usize, record_name: &str) -> Result<Mmap, String> {
    let mmap = mmap_file(path)?;
    if record_size == 0 {
        return Err(format!("internal: record_size=0 for {path}"));
    }
    if mmap.is_empty() {
        return Err(format!(
            "{path}: empty (expected at least one {record_name}) — index is corrupt or incomplete"
        ));
    }
    if mmap.len() % record_size != 0 {
        return Err(format!(
            "{path}: length {} is not a multiple of {record_size} ({record_name}) — \
             likely truncated or format-mismatched; rebuild the index",
            mmap.len()
        ));
    }
    Ok(mmap)
}

/// Mmap an entries/cells file that contains variable-length payloads,
/// refusing only an empty file. Entries files have a u16 count header
/// so the mmap length isn't a simple multiple; we validate structural
/// consistency opportunistically at query time.
fn mmap_nonempty(path: &str, role: &str) -> Result<Mmap, String> {
    let mmap = mmap_file(path)?;
    if mmap.is_empty() {
        return Err(format!(
            "{path}: empty (expected a non-empty {role}) — index is corrupt or incomplete"
        ));
    }
    Ok(mmap)
}

impl Index {
    pub fn load(dir: &str, street_cell_level: u64, admin_cell_level: u64, search_distance: f64) -> Result<Self, String> {
        Self::load_with_admin_config(
            dir,
            street_cell_level,
            admin_cell_level,
            search_distance,
            AdminConfig::embedded_default(),
        )
    }

    pub fn load_with_admin_config(
        dir: &str,
        street_cell_level: u64,
        admin_cell_level: u64,
        search_distance: f64,
        admin_config: AdminConfig,
    ) -> Result<Self, String> {
        let meters_to_rad = search_distance / 111_320.0;
        let max_distance_sq = meters_to_rad * meters_to_rad;
        let postcode_lookup = PostcodeLookup::open(Path::new(dir))?;
        let gnaf = Gnaf::open(Path::new(dir))?;
        let open_addresses = OpenAddresses::open(Path::new(dir))?;
        let i18n_names = I18nNames::open(Path::new(dir))?;
        let wof_countries = crate::wof_countries::WofCountries::open(Path::new(dir))?;
        // Fixed-size record files: length must be a positive multiple
        // of the record size. Variable-payload entry/cells files just
        // need to be non-empty; the reader validates per-cell structure
        // on each access.
        let geo_cells = mmap_records(&format!("{}/geo_cells.bin", dir), 20, "GeoCell")?;
        let street_entries = mmap_nonempty(&format!("{}/street_entries.bin", dir), "street entries")?;
        let street_ways = mmap_records(&format!("{}/street_ways.bin", dir), std::mem::size_of::<WayHeader>(), "WayHeader")?;
        let street_nodes = mmap_records(&format!("{}/street_nodes.bin", dir), std::mem::size_of::<NodeCoord>(), "NodeCoord")?;
        let addr_entries = mmap_nonempty(&format!("{}/addr_entries.bin", dir), "addr entries")?;
        let addr_points = mmap_records(&format!("{}/addr_points.bin", dir), std::mem::size_of::<AddrPoint>(), "AddrPoint")?;
        let interp_entries = mmap_nonempty(&format!("{}/interp_entries.bin", dir), "interp entries")?;
        let interp_ways = mmap_records(&format!("{}/interp_ways.bin", dir), std::mem::size_of::<InterpWay>(), "InterpWay")?;
        let interp_nodes = mmap_records(&format!("{}/interp_nodes.bin", dir), std::mem::size_of::<NodeCoord>(), "NodeCoord")?;
        let admin_cells = mmap_nonempty(&format!("{}/admin_cells.bin", dir), "admin cells")?;
        let admin_entries = mmap_nonempty(&format!("{}/admin_entries.bin", dir), "admin entries")?;
        let admin_polygons = mmap_records(&format!("{}/admin_polygons.bin", dir), std::mem::size_of::<AdminPolygon>(), "AdminPolygon")?;
        let admin_vertices = mmap_records(&format!("{}/admin_vertices.bin", dir), std::mem::size_of::<NodeCoord>(), "NodeCoord")?;
        // Place files are optional (old indexes may not have them); when
        // present, they still have to be well-formed.
        let place_points_path = format!("{}/place_points.bin", dir);
        let place_points = if Path::new(&place_points_path).exists() {
            Some(mmap_records(&place_points_path, std::mem::size_of::<PlacePoint>(), "PlacePoint")?)
        } else {
            None
        };
        let place_cells = mmap_file_optional(&format!("{}/place_cells.bin", dir));
        let place_entries = mmap_file_optional(&format!("{}/place_entries.bin", dir));
        // POI files are optional. The mmap call validates record-size
        // alignment when the file exists; missing/empty file just means
        // /reverse omits the `poi` field and forward search has no
        // POI documents.
        let poi_points_path = format!("{}/poi_points.bin", dir);
        let poi_points = if Path::new(&poi_points_path).exists() {
            Some(mmap_records(&poi_points_path, std::mem::size_of::<PoiPoint>(), "PoiPoint")?)
        } else {
            None
        };
        let poi_cells = mmap_file_optional(&format!("{}/poi_cells.bin", dir));
        let poi_entries = mmap_file_optional(&format!("{}/poi_entries.bin", dir));
        let strings = mmap_nonempty(&format!("{}/strings.bin", dir), "string pool")?;

        Ok(Index {
            geo_cells,
            street_entries,
            street_ways,
            street_nodes,
            addr_entries,
            addr_points,
            interp_entries,
            interp_ways,
            interp_nodes,
            admin_cells,
            admin_entries,
            admin_polygons,
            admin_vertices,
            place_cells,
            place_entries,
            place_points,
            poi_cells,
            poi_entries,
            poi_points,
            strings,
            street_cell_level,
            admin_cell_level,
            max_distance_sq,
            admin_config,
            postcode_lookup,
            gnaf,
            open_addresses,
            i18n_names,
            wof_countries,
        })
    }

    pub fn get_string(&self, offset: u32) -> &str {
        let bytes = &self.strings[offset as usize..];
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..end]).unwrap_or("")
    }

    fn read_u16(data: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([data[offset], data[offset + 1]])
    }

    fn read_u32(data: &[u8], offset: usize) -> u32 {
        // .expect is safe here: callers always slice exactly 4 bytes,
        // and slice-to-[u8; 4] only fails when the length mismatches.
        u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .expect("read_u32: 4-byte slice"),
        )
    }

    fn read_u64(data: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(
            data[offset..offset + 8]
                .try_into()
                .expect("read_u64: 8-byte slice"),
        )
    }

    fn for_each_entry(entries: &[u8], offset: u32, mut f: impl FnMut(u32)) {
        if offset == NO_DATA { return; }
        let offset = offset as usize;
        if offset + 2 > entries.len() { return; }

        let id_count = Self::read_u16(entries, offset) as usize;
        let data_start = offset + 2;
        if data_start + id_count * 4 > entries.len() { return; }

        for i in 0..id_count {
            f(Self::read_u32(entries, data_start + i * 4));
        }
    }

    fn lookup_geo_cell(cells: &[u8], cell_id: u64) -> GeoCellOffsets {
        let entry_size: usize = 20;
        let count = cells.len() / entry_size;
        let empty = GeoCellOffsets { street: NO_DATA, addr: NO_DATA, interp: NO_DATA };
        if count == 0 { return empty; }

        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_id = Self::read_u64(cells, mid * entry_size);
            if mid_id == cell_id {
                return GeoCellOffsets {
                    street: Self::read_u32(cells, mid * entry_size + 8),
                    addr: Self::read_u32(cells, mid * entry_size + 12),
                    interp: Self::read_u32(cells, mid * entry_size + 16),
                };
            } else if mid_id < cell_id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        empty
    }

    fn lookup_admin_cell(cells: &[u8], cell_id: u64) -> u32 {
        let entry_size: usize = 12;
        let count = cells.len() / entry_size;
        if count == 0 { return NO_DATA; }

        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_id = Self::read_u64(cells, mid * entry_size);
            if mid_id == cell_id {
                return Self::read_u32(cells, mid * entry_size + 8);
            } else if mid_id < cell_id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        NO_DATA
    }

    pub fn query_geo(&self, lat: f64, lng: f64) -> (Option<(f64, &AddrPoint)>, Option<(f64, &str, u32)>, Option<(f64, &WayHeader)>) {
        let cell = cell_id_at_level(lat, lng, self.street_cell_level);
        let neighbors = cell_neighbors_at_level(cell, self.street_cell_level);

        let all_points: &[AddrPoint] = as_typed_slice(&self.addr_points);
        let all_ways: &[WayHeader] = as_typed_slice(&self.street_ways);
        let all_street_nodes: &[NodeCoord] = as_typed_slice(&self.street_nodes);
        let all_interps: &[InterpWay] = as_typed_slice(&self.interp_ways);
        let all_interp_nodes: &[NodeCoord] = as_typed_slice(&self.interp_nodes);

        let cos_lat = lat.to_radians().cos();

        let mut best_addr_dist = f64::MAX;
        let mut best_addr: Option<&AddrPoint> = None;
        let mut best_street_dist = f64::MAX;
        let mut best_street: Option<&WayHeader> = None;
        let mut best_interp_dist = f64::MAX;
        let mut best_interp: Option<&InterpWay> = None;
        let mut best_interp_t: f64 = 0.0;

        // A neighbour-cell scan visits at most 9 cells × ~tens of streets; a
        // small sorted scratch buffer outperforms both a real HashSet (heap
        // alloc + hashing overhead) and the previous 64-slot direct-mapped
        // table (collisions silently overwrote and re-processed streets).
        let mut seen_streets: [u32; SEEN_STREETS_CAP] = [0; SEEN_STREETS_CAP];
        let mut seen_streets_len: usize = 0;

        for c in std::iter::once(cell).chain(neighbors.into_iter()) {
            let offsets = Self::lookup_geo_cell(&self.geo_cells, c);

            Self::for_each_entry(&self.addr_entries, offsets.addr, |id| {
                let point = &all_points[id as usize];
                let dlat = (point.lat as f64 - lat).to_radians();
                let dlng = (point.lng as f64 - lng).to_radians();
                let dist = dist_sq(dlat, dlng, cos_lat);
                if dist < best_addr_dist {
                    best_addr_dist = dist;
                    best_addr = Some(point);
                }
            });

            Self::for_each_entry(&self.street_entries, offsets.street, |id| {
                if seen_streets_contains(&seen_streets[..seen_streets_len], id) {
                    return;
                }
                if seen_streets_len < SEEN_STREETS_CAP {
                    seen_streets[seen_streets_len] = id;
                    seen_streets_len += 1;
                }

                let way = &all_ways[id as usize];
                let offset = way.node_offset as usize;
                let count = way.node_count as usize;
                // Defensive: a street with <2 nodes has no segments. The
                // builder guarantees ≥2 per way, but a truncated or
                // adversarial `street_nodes.bin` could violate this —
                // without the guard, `nodes.len() - 1` would underflow
                // usize and the subsequent index would segfault. Crashing
                // a worker on a bad record is not an acceptable 99.99%
                // failure mode.
                if offset + count > all_street_nodes.len() || count < 2 {
                    return;
                }
                let nodes = &all_street_nodes[offset..offset + count];

                for i in 0..nodes.len() - 1 {
                    let dist = point_to_segment_distance(
                        lat, lng,
                        nodes[i].lat as f64, nodes[i].lng as f64,
                        nodes[i + 1].lat as f64, nodes[i + 1].lng as f64,
                        cos_lat,
                    );
                    if dist < best_street_dist {
                        best_street_dist = dist;
                        best_street = Some(way);
                    }
                }
            });

            Self::for_each_entry(&self.interp_entries, offsets.interp, |id| {
                let iw = &all_interps[id as usize];
                if iw.start_number == 0 || iw.end_number == 0 { return; }

                let offset = iw.node_offset as usize;
                let count = iw.node_count as usize;
                // Same defensive guard as the street loop above: corrupt
                // `interp_nodes.bin` must not panic the worker.
                if offset + count > all_interp_nodes.len() || count < 2 {
                    return;
                }
                let nodes = &all_interp_nodes[offset..offset + count];

                if let Some((dist, t)) = project_point_on_polyline(lat, lng, nodes, cos_lat) {
                    if dist < best_interp_dist {
                        best_interp_dist = dist;
                        best_interp = Some(iw);
                        best_interp_t = t;
                    }
                }
            });
        }

        let addr_result = best_addr.map(|p| (best_addr_dist, p));
        let street_result = best_street.map(|w| (best_street_dist, w));
        let interp_result = best_interp.map(|iw| {
            let start = iw.start_number as f64;
            let end = iw.end_number as f64;
            let raw = start + best_interp_t * (end - start);

            let step: u32 = match iw.interpolation {
                1 | 2 => 2,
                _ => 1,
            };

            let number = if step == 2 {
                let base = iw.start_number;
                let offset = ((raw - base as f64) / step as f64).round() as u32 * step;
                base + offset
            } else {
                raw.round() as u32
            };

            (best_interp_dist, self.get_string(iw.street_id), number)
        });

        (addr_result, interp_result, street_result)
    }

    pub fn find_admin(&self, lat: f64, lng: f64) -> AdminResult<'_> {
        let cell = cell_id_at_level(lat, lng, self.admin_cell_level);
        let neighbors = cell_neighbors_at_level(cell, self.admin_cell_level);

        let all_polygons: &[AdminPolygon] = as_typed_slice(&self.admin_polygons);
        let all_vertices: &[NodeCoord] = as_typed_slice(&self.admin_vertices);

        let mut best_by_level: [Option<(f32, &AdminPolygon)>; 12] = [None; 12];

        const INTERIOR_FLAG: u32 = 0x80000000;
        const ID_MASK: u32 = 0x7FFFFFFF;

        for c in std::iter::once(cell).chain(neighbors.into_iter()) {
            Self::for_each_entry(&self.admin_entries, Self::lookup_admin_cell(&self.admin_cells, c), |id| {
                let is_interior = (id & INTERIOR_FLAG) != 0;
                let poly_id = (id & ID_MASK) as usize;
                if poly_id >= all_polygons.len() { return; }
                let poly = &all_polygons[poly_id];
                let level = poly.admin_level as usize;
                if level >= 12 { return; }

                if let Some((best_area, _)) = best_by_level[level] {
                    if poly.area >= best_area { return; }
                }

                let offset = poly.vertex_offset as usize;
                let count = poly.vertex_count as usize;
                // Defensive: reject out-of-range vertex slices from a
                // corrupt admin_polygons.bin / admin_vertices.bin pair
                // rather than panicking the worker.
                if offset + count > all_vertices.len() || count < 3 {
                    return;
                }
                if is_interior || point_in_polygon_f64(lat, lng, &all_vertices[offset..offset + count]) {
                    best_by_level[level] = Some((poly.area, poly));
                }
            });
        }

        let mut result = AdminResult::default();

        // First pass: extract the country code so subsequent levels can apply
        // country-specific mapping. `best_by_level` is indexed by admin_level
        // directly, so level 2 is at index 2.
        if let Some((_, poly)) = best_by_level[2] {
            result.country = Some(self.get_string(poly.name_id));
            result.country_poly_id = Some(poly_id_of(all_polygons, poly));
            if poly.country_code != 0 {
                result.country_code = Some([
                    (poly.country_code >> 8) as u8,
                    (poly.country_code & 0xFF) as u8,
                ]);
            }
        }

        // Fallback: when OSM didn't produce a country_code (either no
        // admin_level=2 polygon covered the point, or the polygon
        // carried no packed cc — typical of Geofabrik country extracts
        // that omit the global boundary relation), consult the
        // Who's on First country polygons. Lifted from Pelias's
        // admin-lookup architecture, scoped to country level only.
        if result.country_code.is_none() {
            if let Some(wof) = self.wof_countries.as_ref() {
                if let Some(m) = wof.find_country(lat, lng) {
                    result.country_code = Some(m.country_code);
                    if result.country.is_none() {
                        result.country = Some(m.name);
                    }
                }
            }
        }

        let cc = result
            .country_code
            .and_then(|c| std::str::from_utf8(&c).ok().map(str::to_owned));

        // `best_by_level` is indexed by admin_level. We process in level
        // order (smallest admin_level = largest area first). Later entries
        // overwrite earlier ones when they target the same output field,
        // which is fine for city: we want the most specific match to win.
        //
        // Mapping is delegated to `self.admin_config` — see
        // `server/config/admin-mapping.json` for the defaults and per-country
        // overrides.
        for level in 0..12usize {
            let Some((_, poly)) = best_by_level[level] else { continue };
            if poly.admin_level == 2 {
                continue; // already handled above
            }
            let Some(entry) = self.admin_config.lookup(cc.as_deref(), poly.admin_level) else {
                continue;
            };
            // Sanity cap: skip polygons that exceed `max_area` for this
            // (country, level) — stops AU pastoral stations tagged
            // admin_level=9 from hijacking the city field. See the _note
            // in config/admin-mapping.json for AU level 9.
            if let Some(max) = entry.max_area {
                if poly.area > max {
                    continue;
                }
            }
            let name = self.get_string(poly.name_id);
            let pid = poly_id_of(all_polygons, poly);
            match entry.field {
                AdminField::Country => { /* handled above */ }
                AdminField::State => {
                    result.state = Some(name);
                    result.state_poly_id = Some(pid);
                }
                AdminField::County => {
                    result.county = Some(name);
                    result.county_poly_id = Some(pid);
                }
                AdminField::CountyIfEmpty => {
                    if result.county.is_none() {
                        result.county = Some(name);
                        result.county_poly_id = Some(pid);
                    }
                }
                AdminField::City => {
                    result.city = Some(name);
                    result.city_poly_id = Some(pid);
                }
                AdminField::CityIfEmpty => {
                    if result.city.is_none() {
                        result.city = Some(name);
                        result.city_poly_id = Some(pid);
                    }
                }
                AdminField::Postcode => result.postcode = Some(name),
                AdminField::Ignore => {}
            }
        }

        result
    }

    /// Nearest-neighbour lookup over `place=*` points within the 9-cell
    /// admin neighbourhood, preferring more prominent features: a nearby
    /// city beats a closer hamlet, a nearby suburb beats a closer farm.
    /// This matches how Nominatim treats address rank — lower rank is more
    /// important. Returns `None` if no point is reachable or the index
    /// has no place data (old build).
    pub fn find_place(&self, lat: f64, lng: f64) -> Option<PlaceMatch<'_>> {
        let points_mmap = self.place_points.as_ref()?;
        let cells_mmap = self.place_cells.as_ref()?;
        let entries_mmap = self.place_entries.as_ref()?;

        let all_places: &[PlacePoint] = as_typed_slice(points_mmap);
        if all_places.is_empty() {
            return None;
        }

        let cell = cell_id_at_level(lat, lng, self.admin_cell_level);
        let neighbors = cell_neighbors_at_level(cell, self.admin_cell_level);
        let cos_lat = lat.to_radians().cos();

        // Single pass: find the best match under an ordering that prefers
        // lower rank, then closer distance. This keeps us from returning a
        // nearby cattle station ("Mount Clarence Station") when a real town
        // ("Coober Pedy") is in the same neighbourhood.
        let mut best: Option<(u8, f64, &PlacePoint)> = None;

        for c in std::iter::once(cell).chain(neighbors.into_iter()) {
            let offset = Self::lookup_admin_cell(cells_mmap, c);
            Self::for_each_entry(entries_mmap, offset, |id| {
                let p = &all_places[id as usize];
                let dlat = (p.lat as f64 - lat).to_radians();
                let dlng = (p.lng as f64 - lng).to_radians();
                let dist = dist_sq(dlat, dlng, cos_lat);
                let take = match best {
                    None => true,
                    Some((best_rank, best_dist, _)) => {
                        // Prefer lower rank. Break ties by closer distance.
                        p.rank < best_rank || (p.rank == best_rank && dist < best_dist)
                    }
                };
                if take {
                    best = Some((p.rank, dist, p));
                }
            });
        }

        best.map(|(_, _, p)| PlaceMatch {
            name: self.get_string(p.name_id),
            rank: p.rank,
        })
    }

    /// Nearest-neighbour POI lookup at street-cell resolution. Used by
    /// /reverse to surface the closest amenity/shop/tourism/etc. as a
    /// sibling field on the response, never replacing the address.
    ///
    /// Capped at `max_distance_m` (~30 m by default — POIs are point
    /// features and a 100 m POI is rarely the user's intent). Within
    /// the threshold, picks lowest rank first (wikipedia/wikidata
    /// POIs win over generic ones), then closest distance.
    pub fn find_poi(&self, lat: f64, lng: f64, max_distance_m: f64) -> Option<PoiMatch<'_>> {
        let points_mmap = self.poi_points.as_ref()?;
        let cells_mmap = self.poi_cells.as_ref()?;
        let entries_mmap = self.poi_entries.as_ref()?;

        let all_pois: &[PoiPoint] = as_typed_slice(points_mmap);
        if all_pois.is_empty() {
            return None;
        }

        let max_rad = max_distance_m / 111_320.0;
        let max_dist_sq = max_rad * max_rad;

        let cell = cell_id_at_level(lat, lng, self.street_cell_level);
        let neighbors = cell_neighbors_at_level(cell, self.street_cell_level);
        let cos_lat = lat.to_radians().cos();

        let mut best: Option<(u8, f64, &PoiPoint)> = None;
        for c in std::iter::once(cell).chain(neighbors.into_iter()) {
            // poi_cells uses the same single-offset-per-cell layout
            // as admin/place cells (12-byte records keyed on u64
            // cell id, written by write_cell_index in the C++
            // builder). lookup_admin_cell does the binary search.
            let off = Self::lookup_admin_cell(cells_mmap, c);
            Self::for_each_entry(entries_mmap, off, |id| {
                let p = &all_pois[id as usize];
                let dlat = (p.lat as f64 - lat).to_radians();
                let dlng = (p.lng as f64 - lng).to_radians();
                let dist = dist_sq(dlat, dlng, cos_lat);
                if dist > max_dist_sq {
                    return;
                }
                let take = match best {
                    None => true,
                    Some((best_rank, best_dist, _)) => {
                        p.rank < best_rank || (p.rank == best_rank && dist < best_dist)
                    }
                };
                if take {
                    best = Some((p.rank, dist, p));
                }
            });
        }

        best.map(|(_, dist_sq_val, p)| {
            // Convert dist_sq (radians^2) back to metres for the
            // response. sqrt + the 111_320 m/rad approximation that
            // matches dist_sq's input scaling.
            let dist_m = dist_sq_val.sqrt() * 111_320.0;
            PoiMatch {
                name: self.get_string(p.name_id),
                category: self.get_string(p.category_id),
                distance_m: dist_m,
            }
        })
    }

    /// Find the closest `addr_point` with a matching house number (and,
    /// optionally, a street-name substring match) within a one-cell
    /// neighbourhood of `(near_lat, near_lng)` at `street_cell_level`.
    ///
    /// This is the refinement step behind forward-geocoding: once we've
    /// located a street centroid via the tantivy index, we look up the
    /// specific house number in the addr_points we already have from the
    /// reverse-geocoding build. Returns `None` when no matching address
    /// exists in the searched cells — typical for long streets where the
    /// centroid sits far from the numbered houses, or for streets with no
    /// OSM addr:housenumber coverage at all.
    pub fn find_addr_point(
        &self,
        housenumber: &str,
        street_name_hint: Option<&str>,
        near_lat: f64,
        near_lng: f64,
    ) -> Option<AddrPointMatch<'_>> {
        self.find_addr_point_in_country(housenumber, street_name_hint, near_lat, near_lng, None)
    }

    /// Housenumber lookup with an explicit country hint.
    ///
    /// When the caller knows the country (typically the `country_code`
    /// field from a forward-search hit), we can route straight to the
    /// right per-country OpenAddresses index without scanning others.
    /// When `country_code` is `None`, we fall back to G-NAF (AU) →
    /// OpenAddresses nearest-across-all → OSM addr_points.
    #[tracing::instrument(
        name = "find_addr_point",
        skip_all,
        fields(
            geocoder.address.country_code = country_code
                .map(|c| String::from_utf8_lossy(c).to_string())
                .unwrap_or_default(),
            geocoder.address.source = tracing::field::Empty,
        )
    )]
    pub fn find_addr_point_in_country(
        &self,
        housenumber: &str,
        street_name_hint: Option<&str>,
        near_lat: f64,
        near_lng: f64,
        country_code: Option<&[u8; 2]>,
    ) -> Option<AddrPointMatch<'_>> {
        let span = tracing::Span::current();

        // G-NAF is authoritative for AU and covers every AU address
        // directly from Geoscape (fresher than the OpenAddresses aggregator).
        // Only try it when we don't have a country hint or the hint is AU.
        let try_gnaf = country_code.map_or(true, |cc| cc.eq_ignore_ascii_case(b"AU"));
        if try_gnaf {
            if let Some(gnaf) = &self.gnaf {
                if let Some(m) = gnaf.find_by_housenumber(
                    housenumber,
                    street_name_hint,
                    near_lat,
                    near_lng,
                    self.street_cell_level,
                ) {
                    span.record("geocoder.address.source", "gnaf");
                    // G-NAF / OpenAddresses ladders don't carry
                    // sub-building or tagged-parent details — those
                    // fields belong to the OSM AddrPoint format only.
                    return Some(AddrPointMatch {
                        lat: m.lat,
                        lng: m.lng,
                        housenumber: m.housenumber,
                        street: m.street,
                        unit: "",
                        floor: "",
                        parent_place: "",
                        flags: 0,
                    });
                }
            }
        }

        // OpenAddresses covers ~60 countries. With a country hint the
        // lookup is O(1) on the per-country map; without one we'd need a
        // global scan, which is expensive, so we skip it in that case
        // and let the OSM fallback handle it.
        if let (Some(oa), Some(cc)) = (&self.open_addresses, country_code) {
            if let Some(m) = oa.find_by_housenumber(
                cc,
                housenumber,
                street_name_hint,
                near_lat,
                near_lng,
                self.street_cell_level,
            ) {
                span.record(
                    "geocoder.address.source",
                    format!("open_addresses_{}", String::from_utf8_lossy(cc).to_ascii_lowercase()).as_str(),
                );
                return Some(AddrPointMatch {
                    lat: m.lat,
                    lng: m.lng,
                    housenumber: m.housenumber,
                    street: m.street,
                    unit: "",
                    floor: "",
                    parent_place: "",
                    flags: 0,
                });
            }
        }

        let cell = cell_id_at_level(near_lat, near_lng, self.street_cell_level);
        let neighbors = cell_neighbors_at_level(cell, self.street_cell_level);
        let all_points: &[AddrPoint] = as_typed_slice(&self.addr_points);
        if all_points.is_empty() {
            return None;
        }

        let hn_needle = housenumber.trim();
        if hn_needle.is_empty() {
            return None;
        }
        let street_hint = street_name_hint
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let cos_lat = near_lat.to_radians().cos();
        let mut best: Option<(f64, &AddrPoint)> = None;

        for c in std::iter::once(cell).chain(neighbors.into_iter()) {
            let offsets = Self::lookup_geo_cell(&self.geo_cells, c);
            Self::for_each_entry(&self.addr_entries, offsets.addr, |id| {
                let p = &all_points[id as usize];
                let p_hn = self.get_string(p.housenumber_id);
                // eq_ignore_ascii_case is zero-alloc — both sides are compared
                // byte-by-byte with per-byte ASCII-lowercase folding. In
                // dense cities this loop visits hundreds of candidates per
                // request; the old `p_hn.to_ascii_lowercase() != hn_needle`
                // allocated a String per visit.
                if !p_hn.eq_ignore_ascii_case(hn_needle) {
                    return;
                }
                if let Some(hint) = street_hint {
                    // street_or_place_id holds the place name when
                    // FLAG_ADDR_PLACE is set, or the street name
                    // otherwise. Either way the street-name hint
                    // matches against it as the most-specific locality
                    // string we have for the address.
                    let p_street = self.get_string(p.street_or_place_id);
                    if !contains_ignore_ascii_case(p_street, hint) {
                        return;
                    }
                }
                let dlat = (p.lat as f64 - near_lat).to_radians();
                let dlng = (p.lng as f64 - near_lng).to_radians();
                let dist = dist_sq(dlat, dlng, cos_lat);
                let take = match best {
                    None => true,
                    Some((best_dist, _)) => dist < best_dist,
                };
                if take {
                    best = Some((dist, p));
                }
            });
        }

        let result = best.map(|(_, p)| {
            // When FLAG_ADDR_PLACE is set, street_or_place_id holds
            // the place name (e.g. `addr:place=Kleindorf`); the address
            // has no street component, so leave `street` empty and
            // surface the place name via parent_place. Callers render
            // this as the city/locality slot in the response.
            let is_place = p.flags & FLAG_ADDR_PLACE != 0;
            let primary = self.get_string(p.street_or_place_id);
            AddrPointMatch {
                lat: p.lat as f64,
                lng: p.lng as f64,
                housenumber: self.get_string(p.housenumber_id),
                street: if is_place { "" } else { primary },
                unit: if p.unit_id != 0 { self.get_string(p.unit_id) } else { "" },
                floor: if p.floor_id != 0 { self.get_string(p.floor_id) } else { "" },
                parent_place: if is_place {
                    primary
                } else if p.parent_place_id != 0 {
                    self.get_string(p.parent_place_id)
                } else {
                    ""
                },
                flags: p.flags,
            }
        });
        tracing::Span::current().record(
            "geocoder.address.source",
            if result.is_some() { "osm_addr_points" } else { "none" },
        );
        result
    }

    /// Reverse-geocode `(lat, lng)` and optionally return admin names in
    /// the caller's preferred language via OSM `name:<lang>` tags. Missing
    /// i18n index or no matching tag = falls through to the default name.
    pub fn query_with_lang(&self, lat: f64, lng: f64, lang: Option<&str>) -> Address<'_> {
        let (mut address, admin) = self.query_and_admin(lat, lng);
        let Some(lang) = lang else { return address };
        let Some(code) = pack_lang_code(lang) else { return address };
        let Some(i18n) = self.i18n_names.as_ref() else { return address };

        // Apply the i18n override for each admin field that carries a
        // poly_id. Fields set via place=* fallback have no poly_id and
        // fall through unchanged.
        let details = &mut address.address;
        // Each admin field localises through ALIAS_PRIMARY (the
        // `name:<lang>` family). OFFICIAL / ALT and other alias
        // families exist on the same entities but are surfaced only
        // via the forward-index `alternates_for` walk, not the
        // /reverse?lang= path.
        if let Some(pid) = admin.country_poly_id {
            if let Some(id) = i18n.lookup(ENTITY_ADMIN, pid, ALIAS_PRIMARY, code) {
                details.country = Some(self.get_string(id));
            }
        }
        if let Some(pid) = admin.state_poly_id {
            if let Some(id) = i18n.lookup(ENTITY_ADMIN, pid, ALIAS_PRIMARY, code) {
                details.state = Some(self.get_string(id));
            }
        }
        if let Some(pid) = admin.county_poly_id {
            if let Some(id) = i18n.lookup(ENTITY_ADMIN, pid, ALIAS_PRIMARY, code) {
                details.county = Some(self.get_string(id));
            }
        }
        if let Some(pid) = admin.city_poly_id {
            if let Some(id) = i18n.lookup(ENTITY_ADMIN, pid, ALIAS_PRIMARY, code) {
                details.city = Some(self.get_string(id));
            }
        }
        // Re-render display_name with the localized fields.
        address.display_name = format_address(details);
        address
    }

    pub fn query(&self, lat: f64, lng: f64) -> Address<'_> {
        self.query_and_admin(lat, lng).0
    }

    /// Internal: the full query pipeline, returning both the public
    /// `Address` and the `AdminResult` it was built from so
    /// `query_with_lang` can reuse the poly_ids instead of running
    /// `find_admin` twice. ~50 µs saved on every i18n-tagged /reverse.
    fn query_and_admin(&self, lat: f64, lng: f64) -> (Address<'_>, AdminResult<'_>) {
        let max_dist = self.max_distance_sq;

        let mut admin = self.find_admin(lat, lng);

        // Fallback: if admin boundaries didn't produce a city, look up the
        // nearest `place=*` point. This fills the gap in rural areas where
        // admin polygons don't cover every populated place. Only applies
        // when we have *some* admin context (country/state) — offshore
        // queries still return nothing.
        if admin.city.is_none() && admin.country.is_some() {
            if let Some(place) = self.find_place(lat, lng) {
                // Rank 16 = city/town/village → city. Rank 19 = suburb — put
                // in city too since our schema has no dedicated suburb field
                // and the suburb is still the right postal locality.
                admin.city = Some(place.name);
                // Hamlets (rank 20) and other rare ranks fall through
                // unchanged; the match arm above runs for any rank.
                let _ = place.rank;
            }
        }

        // Postcode fallback ladder, in decreasing accuracy:
        //   1. G-NAF nearest address point (exact per-address postcode, AU)
        //   2. G-NAF-derived suburb-modal lookup (approximate for AU)
        //   3. give up — leave postcode empty
        // OSM's `boundary=postal_code` coverage for AU is <5%, so
        // find_admin rarely populates postcode and we almost always enter
        // this fallback for AU queries.
        //
        // Postcode enrichment ladder. Each step is gated so queries that
        // can't benefit don't pay for the spatial scan:
        //   1. G-NAF (AU-only, authoritative direct-from-Geoscape)
        //   2. OpenAddresses per-country (covers ~60 countries, postcode
        //      comes from the upstream authoritative dataset that OA
        //      aggregates for each country)
        //   3. Suburb-modal lookup (AU fallback when the nearest-point
        //      spatial scan misses)
        let country_code_lower = admin
            .country_code
            .as_ref()
            .map(|c| [c[0].to_ascii_lowercase(), c[1].to_ascii_lowercase()]);
        let is_au = country_code_lower.as_ref().is_some_and(|c| c == b"au");

        if admin.postcode.is_none() && is_au {
            if let Some(gnaf) = &self.gnaf {
                if let Some(m) = gnaf.find_nearest(lat, lng, self.street_cell_level) {
                    if !m.postcode.is_empty() {
                        admin.postcode = Some(m.postcode);
                    }
                }
            }
        }
        if admin.postcode.is_none() {
            if let (Some(oa), Some(cc)) = (&self.open_addresses, country_code_lower.as_ref()) {
                if let Some(m) = oa.find_nearest(cc, lat, lng, self.street_cell_level) {
                    if !m.postcode.is_empty() {
                        admin.postcode = Some(m.postcode);
                    }
                }
            }
        }
        if admin.postcode.is_none() && is_au {
            if let (Some(lookup), Some(cc_bytes), Some(locality)) =
                (&self.postcode_lookup, admin.country_code, admin.city)
            {
                if let Some(state_abbr) =
                    country_to_state_abbreviation(&cc_bytes, admin.state)
                {
                    admin.postcode = lookup.postcode(state_abbr, locality);
                }
            }
        }

        let (addr, interp, street) = self.query_geo(lat, lng);

        let mut house_number: Option<Cow<'_, str>> = None;
        let mut road: Option<&str> = None;
        // Track which source resolved the address so we can stamp the
        // response with an honest confidence label. `exact` = we matched
        // a specific addr_point (G-NAF, OSM addr_point, OpenAddresses);
        // `interpolated` = derived from an addr:interpolation way;
        // `fallback` = only the street centroid / admin polygon hit.
        let mut confidence_level: Option<&'static str> = None;

        if let Some((dist, point)) = addr {
            if dist < max_dist {
                house_number = Some(Cow::Borrowed(self.get_string(point.housenumber_id)));
                // For addr:place addresses, street_or_place_id holds
                // the place name; surface it as the city/locality
                // (handled by the AddrPointMatch path / admin merge
                // below) rather than as the road. For street-keyed
                // addresses, this is the road name.
                let is_place = point.flags & FLAG_ADDR_PLACE != 0;
                if !is_place {
                    road = Some(self.get_string(point.street_or_place_id));
                }
                // Tagged parent (addr:city/suburb/locality/state) — the
                // C++ importer captures the verbatim tag value into
                // parent_place_id. Prefer it over geometric find_admin()
                // when the geographic lookup didn't produce a city,
                // since the OSM tag is the data owner's authoritative
                // statement about which locality the address sits in.
                // Note: this skips the i18n localisation pass (which
                // keys on admin polygon ids); a `?lang=de` query that
                // hits a parent_place_id-derived city returns the
                // verbatim tag value, not a translated form.
                if point.parent_place_id != 0 && admin.city.is_none() {
                    admin.city = Some(self.get_string(point.parent_place_id));
                }
                if is_place && admin.city.is_none() {
                    admin.city = Some(self.get_string(point.street_or_place_id));
                }
                confidence_level = Some(confidence::EXACT);
            }
        }
        if road.is_none() {
            if let Some((dist, street_name, number)) = interp {
                if dist < max_dist {
                    house_number = Some(Cow::Owned(number.to_string()));
                    road = Some(street_name);
                    confidence_level = Some(confidence::INTERPOLATED);
                }
            }
        }
        if road.is_none() {
            if let Some((dist, way)) = street {
                if dist < max_dist {
                    road = Some(self.get_string(way.name_id));
                    confidence_level = Some(confidence::FALLBACK);
                }
            }
        }

        // POI lookup runs independently of the address ladder. Tight
        // 30 m threshold means dense areas can return a POI even when
        // the address resolved fine; sparse areas typically miss. The
        // poi field is purely additive to the response — clients that
        // don't know about it ignore it (json), clients that do can
        // surface it alongside the address.
        let poi = self.find_poi(lat, lng, /*max_distance_m=*/ 30.0);

        if road.is_none() && admin.country.is_none() && admin.city.is_none() && poi.is_none() {
            return (Address::default(), admin);
        }

        // If we only have admin data (no road / house), that's still a
        // fallback. Explicit so clients see it instead of missing field.
        if confidence_level.is_none() {
            confidence_level = Some(confidence::FALLBACK);
        }

        let address = AddressDetails {
            house_number,
            road,
            city: admin.city,
            state: admin.state,
            county: admin.county,
            postcode: admin.postcode,
            country: admin.country,
            country_code: admin.country_code.map(|c| String::from_utf8_lossy(&c).into_owned()),
        };
        let display_name = format_address(&address);
        let out = Address {
            display_name,
            address,
            confidence: confidence_level,
            // H3 is populated by the HTTP layer when the caller asked for
            // it — `query` itself is H3-agnostic so the indexing layer
            // stays callable from non-HTTP contexts (tests, benches,
            // builders) without having to think about query-time param.
            h3: None,
            poi,
        };
        (out, admin)
    }
}

// --- Geometry helpers ---

/// Case-insensitive substring check without allocating. Mirrors
/// `str::contains` + `str::to_ascii_lowercase` but comparing byte-by-
/// byte with per-byte ASCII folding, so no `String` is materialised.
/// Used in housenumber / street matching where the hot loop visits
/// hundreds of candidates per request. ASCII-only folding is
/// acceptable: upstream normalisation (G-NAF, OpenAddresses) already
/// strips diacritics, and the comparison is against street names from
/// the same pipeline.
#[inline]
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    let last = h.len() - n.len();
    for i in 0..=last {
        let mut matched = true;
        for j in 0..n.len() {
            if !h[i + j].eq_ignore_ascii_case(&n[j]) {
                matched = false;
                break;
            }
        }
        if matched {
            return true;
        }
    }
    false
}

/// Resolve a `&AdminPolygon` reference back to its index in the
/// `admin_polygons.bin` array, via pointer arithmetic against the typed
/// slice we already hold. Used to carry poly_ids out of find_admin so
/// query_with_lang can look up i18n overrides.
fn poly_id_of(all_polygons: &[AdminPolygon], poly: &AdminPolygon) -> u32 {
    let base = all_polygons.as_ptr() as usize;
    let here = poly as *const AdminPolygon as usize;
    let offset = here.saturating_sub(base) / std::mem::size_of::<AdminPolygon>();
    offset as u32
}

/// Map an admin state name to the abbreviation our postcode lookup keys
/// expect. Currently only AU states are covered — the lookup itself is
/// AU-specific via its G-NAF data source. Returns `None` for countries
/// or states we don't recognise; the caller then skips the postcode
/// lookup for that query.
fn country_to_state_abbreviation(
    country_code: &[u8; 2],
    state_name: Option<&str>,
) -> Option<&'static str> {
    if country_code != b"AU" {
        return None;
    }
    match state_name? {
        "New South Wales" => Some("NSW"),
        "Victoria" => Some("VIC"),
        "Queensland" => Some("QLD"),
        "Western Australia" => Some("WA"),
        "South Australia" => Some("SA"),
        "Tasmania" => Some("TAS"),
        "Northern Territory" => Some("NT"),
        "Australian Capital Territory" => Some("ACT"),
        _ => None,
    }
}

pub fn dist_sq(dlat: f64, dlng: f64, cos_lat: f64) -> f64 {
    dlat * dlat + dlng * dlng * cos_lat * cos_lat
}

pub fn point_to_segment_with_t(
    px: f64, py: f64,
    ax: f64, ay: f64,
    bx: f64, by: f64,
    cos_lat: f64,
) -> (f64, f64) {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let t = if len_sq == 0.0 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0)
    };

    let proj_x = ax + t * dx;
    let proj_y = ay + t * dy;
    let dlat = (px - proj_x).to_radians();
    let dlng = (py - proj_y).to_radians();
    (dist_sq(dlat, dlng, cos_lat), t)
}

pub fn point_to_segment_distance(
    px: f64, py: f64,
    ax: f64, ay: f64,
    bx: f64, by: f64,
    cos_lat: f64,
) -> f64 {
    point_to_segment_with_t(px, py, ax, ay, bx, by, cos_lat).0
}

/// Project `(lat, lng)` onto a polyline and return `(dist_sq, t)` where `t` is
/// the parametric position along the polyline in `[0.0, 1.0]`, proportional to
/// arc length, used as the house-number interpolation parameter.
///
/// Returns `None` if the polyline has fewer than 2 nodes or zero total length.
pub fn project_point_on_polyline(
    lat: f64,
    lng: f64,
    nodes: &[NodeCoord],
    cos_lat: f64,
) -> Option<(f64, f64)> {
    if nodes.len() < 2 {
        return None;
    }

    // Single pass: compute each segment length, project onto each segment,
    // and carry a running total of arc length. A fixed-size buffer on the
    // stack holds per-segment (length, dist_sq, seg_t) so we don't need a
    // second pass to divide by total length. Streets have `node_count: u8`
    // in the on-disk format, so up to 255 segments is bounded.
    let mut seg_lens: [f64; 255] = [0.0; 255];
    let mut best_seg_dist = f64::MAX;
    let mut best_seg_idx: usize = 0;
    let mut best_seg_t: f64 = 0.0;
    let mut total_len: f64 = 0.0;

    for i in 0..nodes.len() - 1 {
        let dlat = (nodes[i + 1].lat as f64 - nodes[i].lat as f64).to_radians();
        let dlng = (nodes[i + 1].lng as f64 - nodes[i].lng as f64).to_radians();
        let seg_len = dist_sq(dlat, dlng, cos_lat).sqrt();
        seg_lens[i] = seg_len;
        total_len += seg_len;

        let (dist, seg_t) = point_to_segment_with_t(
            lat,
            lng,
            nodes[i].lat as f64,
            nodes[i].lng as f64,
            nodes[i + 1].lat as f64,
            nodes[i + 1].lng as f64,
            cos_lat,
        );
        if dist < best_seg_dist {
            best_seg_dist = dist;
            best_seg_idx = i;
            best_seg_t = seg_t;
        }
    }

    if total_len == 0.0 {
        return None;
    }

    // Convert the in-segment parameter to a polyline-wide parameter using
    // cumulative arc length up to the winning segment.
    let mut prefix: f64 = 0.0;
    for i in 0..best_seg_idx {
        prefix += seg_lens[i];
    }
    let t = (prefix + best_seg_t * seg_lens[best_seg_idx]) / total_len;
    Some((best_seg_dist, t))
}

/// Thin wrapper kept for test compatibility; promotes to f64 and delegates.
pub fn point_in_polygon(lat: f32, lng: f32, vertices: &[NodeCoord]) -> bool {
    point_in_polygon_f64(lat as f64, lng as f64, vertices)
}

/// Ray-casting point-in-polygon test. Vertices are stored as f32 (half the
/// index size) but the edge equation runs in f64 to avoid precision loss near
/// long polygon edges — see `tests/polygon_precision.rs`.
pub fn point_in_polygon_f64(lat: f64, lng: f64, vertices: &[NodeCoord]) -> bool {
    let mut inside = false;
    let n = vertices.len();
    if n == 0 { return false; }
    let mut j = n - 1;

    for i in 0..n {
        let vi_lat = vertices[i].lat as f64;
        let vi_lng = vertices[i].lng as f64;
        let vj_lat = vertices[j].lat as f64;
        let vj_lng = vertices[j].lng as f64;

        if ((vi_lng > lng) != (vj_lng > lng))
            && (lat < (vj_lat - vi_lat) * (lng - vi_lng) / (vj_lng - vi_lng) + vi_lat)
        {
            inside = !inside;
        }
        j = i;
    }

    inside
}

// --- API types ---

#[derive(Default)]
pub struct AdminResult<'a> {
    pub country: Option<&'a str>,
    pub country_code: Option<[u8; 2]>,
    pub state: Option<&'a str>,
    pub county: Option<&'a str>,
    pub city: Option<&'a str>,
    pub postcode: Option<&'a str>,

    // Polygon IDs for each populated field — index into admin_polygons.bin.
    // `query_with_lang` uses these to replace the name with its
    // `name:<lang>` translation from i18n_names.bin, if one exists.
    // Populated by find_admin; fields that came from place=* fallback or
    // postcode lookup leave these as None (no i18n for those).
    #[doc(hidden)]
    pub country_poly_id: Option<u32>,
    #[doc(hidden)]
    pub state_poly_id: Option<u32>,
    #[doc(hidden)]
    pub county_poly_id: Option<u32>,
    #[doc(hidden)]
    pub city_poly_id: Option<u32>,
}

/// Result of `find_place`: a nearby place=* feature with its Nominatim-style
/// rank so the caller can map it to the right output field (city vs suburb).
pub struct PlaceMatch<'a> {
    pub name: &'a str,
    pub rank: u8,
}

/// Result of `find_poi`: a nearby POI with its name, category
/// (`amenity:cafe`, `tourism:attraction`, ...), and distance in
/// metres. /reverse surfaces this on the response as a sibling field
/// to the address — POIs never replace the address fields.
#[derive(Serialize)]
pub struct PoiMatch<'a> {
    pub name: &'a str,
    pub category: &'a str,
    pub distance_m: f64,
}

/// Result of `find_addr_point`: exact lat/lng of a house number on a
/// street, plus any sub-building / parent-locality details we captured
/// from OSM `addr:*` tags.
///
/// `street` is empty when the source address used `addr:place` instead
/// of `addr:street` (`flags & FLAG_ADDR_PLACE`); in that case the
/// parent place name is in `parent_place`. `unit` / `floor` / `parent_place`
/// are empty when the corresponding `addr:unit` / `addr:floor` /
/// `addr:city|suburb|locality|state` tag was absent.
pub struct AddrPointMatch<'a> {
    pub lat: f64,
    pub lng: f64,
    pub housenumber: &'a str,
    pub street: &'a str,
    pub unit: &'a str,
    pub floor: &'a str,
    pub parent_place: &'a str,
    pub flags: u8,
}

#[derive(Serialize, Default)]
pub struct AddressDetails<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub house_number: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub road: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub county: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postcode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
}

#[derive(Serialize, Default)]
pub struct Address<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub address: AddressDetails<'a>,
    /// How strongly the result maps to the requested point. Mirrors Radar's
    /// `confidence` and Nominatim's `match_code` so clients can downgrade
    /// trust on fallback matches. One of `exact`, `interpolated`, `fallback`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<&'static str>,
    /// Optional H3 cell identifiers, keyed by resolution. Populated when
    /// the caller passes `h3_res=...` on the request. Map shape means a
    /// single-resolution request and a multi-resolution request share
    /// the same schema: `{"9": "8928308280fffff"}` vs
    /// `{"7": "87283082fffffff", "9": "8928308280fffff"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h3: Option<std::collections::BTreeMap<String, String>>,
    /// Closest amenity / shop / tourism / etc. POI within ~30 m of
    /// the query coord. Sibling field — never replaces the address.
    /// Old indexes (built before commit 4) and queries that hit no POI
    /// in range simply leave this `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poi: Option<PoiMatch<'a>>,
}

/// Canonical confidence labels used on `Address::confidence` and the
/// `/search` hit enrichment. Kept as `&'static str` so serde emits them
/// directly without allocation.
pub mod confidence {
    pub const EXACT: &str = "exact";
    pub const INTERPOLATED: &str = "interpolated";
    pub const FALLBACK: &str = "fallback";
}

pub fn format_rules(country_code: Option<&str>) -> (bool, bool, bool) {
    match country_code {
        Some("US") | Some("CA") | Some("AU") | Some("NZ")
        | Some("GB") | Some("IE") | Some("ZA") | Some("IN")
        | Some("NG") | Some("KE") | Some("GH") | Some("PK")
        | Some("PH") | Some("TH") | Some("MY") => (false, false, true),

        Some("JP") | Some("KR") | Some("CN") | Some("TW") => (false, true, true),

        _ => (true, true, false),
    }
}

pub fn format_address(addr: &AddressDetails<'_>) -> Option<String> {
    if addr.road.is_none() && addr.city.is_none() && addr.country.is_none() {
        return None;
    }

    let (number_after, postcode_before_city, include_state) = format_rules(addr.country_code.as_deref());

    // Conservative upper bound sized to fit a typical full address without
    // reallocation. One allocation (the backing buffer) covers the whole call.
    let mut out = String::with_capacity(128);

    let append_comma = |s: &mut String| {
        if !s.is_empty() {
            s.push_str(", ");
        }
    };

    if let Some(road) = addr.road {
        if let Some(hn) = addr.house_number.as_deref() {
            if number_after {
                out.push_str(road);
                out.push(' ');
                out.push_str(hn);
            } else {
                out.push_str(hn);
                out.push(' ');
                out.push_str(road);
            }
        } else {
            out.push_str(road);
        }
    }

    // Build the "city section" directly into `out`, remembering the start
    // index so we can detect whether anything was actually written and trim
    // a trailing space if needed.
    let city_section_start = out.len();
    let needs_comma = !out.is_empty();
    if needs_comma {
        out.push_str(", ");
    }
    let city_section_body_start = out.len();

    if postcode_before_city {
        if let Some(pc) = addr.postcode {
            out.push_str(pc);
            out.push(' ');
        }
        if let Some(city) = addr.city {
            out.push_str(city);
        }
        if include_state {
            if let Some(state) = addr.state {
                if out.len() > city_section_body_start {
                    out.push_str(", ");
                }
                out.push_str(state);
            }
        }
    } else {
        if let Some(city) = addr.city {
            out.push_str(city);
        }
        if include_state {
            if let Some(state) = addr.state {
                if out.len() > city_section_body_start {
                    out.push_str(", ");
                }
                out.push_str(state);
            }
        }
        if let Some(pc) = addr.postcode {
            if out.len() > city_section_body_start {
                out.push(' ');
            }
            out.push_str(pc);
        }
    }

    // Nothing ended up in the city section — roll back the ", " we speculatively
    // appended. Also trim a trailing space left by "postcode " if no city came
    // after it.
    if out.len() == city_section_body_start {
        out.truncate(city_section_start);
    } else if out.ends_with(' ') {
        out.pop();
    }

    if let Some(country) = addr.country {
        append_comma(&mut out);
        out.push_str(country);
    }

    if out.is_empty() { None } else { Some(out) }
}
