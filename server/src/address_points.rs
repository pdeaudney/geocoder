//! Shared binary format and query helpers for authoritative address-point
//! indexes. Both `gnaf` (Australia-specific G-NAF data) and `openaddresses`
//! (worldwide per-country OpenAddresses.io data) use this layout — they
//! differ only in where the data comes from at build time, not how it
//! looks on disk or how it's queried.
//!
//! # Binary layout (shared between all address-point sources)
//!
//! Four files per index:
//!
//! - `<prefix>_points.bin` — fixed-size 24-byte records:
//!   ```text
//!   struct AddressPoint {
//!       f32 lat, f32 lng,
//!       u32 housenumber_id, u32 street_id,
//!       u32 locality_id,    u32 postcode_id,
//!   }
//!   ```
//! - `<prefix>_cells.bin` — sorted `(u64 cell_id, u32 entry_offset)` pairs
//!   at `street_cell_level`. Same layout as `admin_cells.bin`.
//! - `<prefix>_entries.bin` — per-cell `(u16 count, u32 point_ids...)`.
//! - `<prefix>_strings.bin` — NUL-terminated string pool. Entry 0 is
//!   deliberately an empty string so unset ids resolve to `""`.
//!
//! # Why share
//!
//! G-NAF and OpenAddresses both provide the same kind of data — a lat/lng
//! paired with an address. Different builders feed the same consumer, so
//! the format, the S2 cell lookup, the PIP scan, and the nearest-neighbour
//! search all live here once. Country-specific knowledge (state
//! abbreviations, postcode normalisation) stays in the caller.

use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Fixed-size record in `<prefix>_points.bin`.
///
/// The `#[repr(C)]` layout must stay 24 bytes — the `struct_layout` test
/// pins this. Changing fields is a binary-format change that forces every
/// consumer to rebuild.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AddressPoint {
    pub lat: f32,
    pub lng: f32,
    pub housenumber_id: u32,
    pub street_id: u32,
    pub locality_id: u32,
    pub postcode_id: u32,
}

/// Resolved address returned from the query path.
pub struct AddressMatch<'a> {
    pub lat: f64,
    pub lng: f64,
    pub housenumber: &'a str,
    pub street: &'a str,
    pub locality: &'a str,
    pub postcode: &'a str,
}

/// Four mmap'd files forming one address-point index. Open once at
/// startup; every query just walks these — no allocation, no decode.
pub struct AddressPointIndex {
    points: Mmap,
    cells: Mmap,
    entries: Mmap,
    strings: Mmap,
}

impl AddressPointIndex {
    /// Open an index by explicit file paths. Returns `Ok(None)` when any
    /// of the four files is missing — callers treat this as "no index for
    /// that country" and fall back to whatever comes next in the ladder.
    pub fn open(
        points_path: &Path,
        cells_path: &Path,
        entries_path: &Path,
        strings_path: &Path,
    ) -> Result<Option<Self>, String> {
        Self::open_labeled("address_points", points_path, cells_path, entries_path, strings_path)
    }

    /// Same as [`open`], but tags the manifest entries with a caller-supplied
    /// label (`gnaf`, `open_addresses_us`, …) so each loaded file is grep-able
    /// by source in stdout logs.
    pub fn open_labeled(
        index_label: &'static str,
        points_path: &Path,
        cells_path: &Path,
        entries_path: &Path,
        strings_path: &Path,
    ) -> Result<Option<Self>, String> {
        let paths = [points_path, cells_path, entries_path, strings_path];
        if paths.iter().any(|p| !p.exists()) {
            return Ok(None);
        }
        let mmap = |p: &Path| -> Result<Mmap, String> {
            let f = File::open(p).map_err(|e| format!("open {}: {}", p.display(), e))?;
            let m = unsafe { Mmap::map(&f) }.map_err(|e| format!("mmap {}: {}", p.display(), e))?;
            crate::log_loaded_file(index_label, &p.display().to_string(), m.len() as u64);
            Ok(m)
        };
        Ok(Some(AddressPointIndex {
            points: mmap(points_path)?,
            cells: mmap(cells_path)?,
            entries: mmap(entries_path)?,
            strings: mmap(strings_path)?,
        }))
    }

    /// Convenience: open an index given a directory and a filename prefix
    /// (e.g. `open_with_prefix(dir, "gnaf")` opens
    /// `dir/gnaf_{points,cells,entries,strings}.bin`).
    pub fn open_with_prefix(dir: &Path, prefix: &str) -> Result<Option<Self>, String> {
        Self::open_with_prefix_labeled(dir, prefix, "address_points")
    }

    pub fn open_with_prefix_labeled(
        dir: &Path,
        prefix: &str,
        index_label: &'static str,
    ) -> Result<Option<Self>, String> {
        let points = dir.join(format!("{prefix}_points.bin"));
        let cells = dir.join(format!("{prefix}_cells.bin"));
        let entries = dir.join(format!("{prefix}_entries.bin"));
        let strings = dir.join(format!("{prefix}_strings.bin"));
        Self::open_labeled(index_label, &points, &cells, &entries, &strings)
    }

    pub fn points(&self) -> &[AddressPoint] {
        crate::as_typed_slice(&self.points)
    }

    pub fn string_at(&self, offset: u32) -> &str {
        let off = offset as usize;
        let bytes = self.strings.get(off..).unwrap_or(&[]);
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..end]).unwrap_or("")
    }

    pub fn hydrate(&self, point: &AddressPoint) -> AddressMatch<'_> {
        AddressMatch {
            lat: point.lat as f64,
            lng: point.lng as f64,
            housenumber: self.string_at(point.housenumber_id),
            street: self.string_at(point.street_id),
            locality: self.string_at(point.locality_id),
            postcode: self.string_at(point.postcode_id),
        }
    }

    /// Find the address point matching `housenumber` (case-insensitive
    /// exact) and optionally a street-name substring, within a 9-cell S2
    /// neighbourhood of `(near_lat, near_lng)` at `street_level`.
    pub fn find_by_housenumber(
        &self,
        housenumber: &str,
        street_hint: Option<&str>,
        near_lat: f64,
        near_lng: f64,
        street_level: u64,
    ) -> Option<AddressMatch<'_>> {
        if housenumber.trim().is_empty() {
            return None;
        }
        let points = self.points();
        if points.is_empty() {
            return None;
        }

        use s2::cellid::CellID;
        use s2::latlng::LatLng;
        let origin = CellID::from(LatLng::from_degrees(near_lat, near_lng)).parent(street_level);
        let neighbours = origin.all_neighbors(street_level);

        let hn_needle = housenumber.trim();
        let street_needle = street_hint
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let cos_lat = near_lat.to_radians().cos();
        let mut best: Option<(f64, &AddressPoint)> = None;
        // Fuse the candidate enumeration with the scoring pass — avoids
        // materialising a Vec<u32> of IDs for every query. On dense
        // cities this was ~1–4 KB of heap per call.
        for cell in std::iter::once(origin).chain(neighbours.into_iter()) {
            let Some(entry_offset) = lookup_cell_offset(&self.cells, cell.0) else {
                continue;
            };
            for_each_point_in_cell(&self.entries, entry_offset, |id| {
                let Some(p) = points.get(id as usize) else {
                    return;
                };
                let hn = self.string_at(p.housenumber_id);
                if !hn.eq_ignore_ascii_case(hn_needle) {
                    return;
                }
                if let Some(street_needle) = street_needle {
                    let street = self.string_at(p.street_id);
                    // Zero-alloc case-insensitive substring check. Mirrors
                    // crate::contains_ignore_ascii_case but inlined here
                    // to avoid a module-private dependency.
                    if !contains_ignore_ascii_case(street, street_needle) {
                        return;
                    }
                }
                let dlat = (p.lat as f64 - near_lat).to_radians();
                let dlng = (p.lng as f64 - near_lng).to_radians();
                let dist = dlat * dlat + dlng * dlng * cos_lat * cos_lat;
                let take = match best {
                    None => true,
                    Some((best_dist, _)) => dist < best_dist,
                };
                if take {
                    best = Some((dist, p));
                }
            });
        }

        best.map(|(_, p)| self.hydrate(p))
    }

    /// Nearest address point to `(lat, lng)` within a 9-cell neighbourhood.
    pub fn find_nearest(&self, lat: f64, lng: f64, street_level: u64) -> Option<AddressMatch<'_>> {
        let points = self.points();
        if points.is_empty() {
            return None;
        }

        use s2::cellid::CellID;
        use s2::latlng::LatLng;
        let origin = CellID::from(LatLng::from_degrees(lat, lng)).parent(street_level);
        let neighbours = origin.all_neighbors(street_level);

        let cos_lat = lat.to_radians().cos();
        let mut candidate_ids: Vec<u32> = Vec::new();
        for cell in std::iter::once(origin).chain(neighbours.into_iter()) {
            let Some(entry_offset) = lookup_cell_offset(&self.cells, cell.0) else {
                continue;
            };
            for_each_point_in_cell(&self.entries, entry_offset, |id| candidate_ids.push(id));
        }

        let mut best: Option<(f64, &AddressPoint)> = None;
        for id in candidate_ids {
            let Some(p) = points.get(id as usize) else {
                continue;
            };
            let dlat = (p.lat as f64 - lat).to_radians();
            let dlng = (p.lng as f64 - lng).to_radians();
            let dist = dlat * dlat + dlng * dlng * cos_lat * cos_lat;
            let take = match best {
                None => true,
                Some((best_dist, _)) => dist < best_dist,
            };
            if take {
                best = Some((dist, p));
            }
        }
        best.map(|(_, p)| self.hydrate(p))
    }
}

// --- Build-side re-exports ---
//
// Keep the helpers pub(crate) so the build tools (gnaf + OpenAddresses)
// share the same cell-index layout code without duplicating it. The
// on-disk format lives in exactly one place: this module.

const CELL_ENTRY_SIZE: usize = 12; // u64 + u32

/// Case-insensitive substring check, zero-alloc. ASCII folding only —
/// upstream normalisation in G-NAF / OpenAddresses has already stripped
/// diacritics, so the comparison is against ASCII-normalised names.
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

fn lookup_cell_offset(cells: &[u8], cell_id: u64) -> Option<u32> {
    let count = cells.len() / CELL_ENTRY_SIZE;
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let base = mid * CELL_ENTRY_SIZE;
        let mid_id = u64::from_le_bytes(
            cells[base..base + 8]
                .try_into()
                .expect("cells.bin layout is u64+u32, 12 bytes per entry"),
        );
        if mid_id == cell_id {
            return Some(u32::from_le_bytes(
                cells[base + 8..base + 12]
                    .try_into()
                    .expect("cells.bin layout guarantees 4-byte offset"),
            ));
        }
        if mid_id < cell_id {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    None
}

fn for_each_point_in_cell<F>(entries: &[u8], offset: u32, mut f: F)
where
    F: FnMut(u32),
{
    let off = offset as usize;
    if off + 2 > entries.len() {
        return;
    }
    let count = u16::from_le_bytes(
        entries[off..off + 2]
            .try_into()
            .expect("entries.bin count is u16 LE"),
    ) as usize;
    let ids_start = off + 2;
    if ids_start + count * 4 > entries.len() {
        return;
    }
    for i in 0..count {
        let id = u32::from_le_bytes(
            entries[ids_start + i * 4..ids_start + i * 4 + 4]
                .try_into()
                .expect("entries.bin id is u32 LE"),
        );
        f(id);
    }
}

/// Paths the build tools should write into, keyed by prefix.
pub struct BuildOutputPaths {
    pub points: PathBuf,
    pub cells: PathBuf,
    pub entries: PathBuf,
    pub strings: PathBuf,
}

impl BuildOutputPaths {
    pub fn for_prefix(dir: &Path, prefix: &str) -> Self {
        BuildOutputPaths {
            points: dir.join(format!("{prefix}_points.bin")),
            cells: dir.join(format!("{prefix}_cells.bin")),
            entries: dir.join(format!("{prefix}_entries.bin")),
            strings: dir.join(format!("{prefix}_strings.bin")),
        }
    }
}
