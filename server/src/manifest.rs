//! Per-builder index manifest emitter.
//!
//! Index builders write JSON manifests at the index root. These record
//! build provenance and counts. `manifest_reverse.json` also carries the
//! binary schema and file lengths, which `Index::load` checks before mmap.

use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// Refuse to reinterpret an index built with a different C++ record layout.
/// The builder stores field offsets as well as sizes, since a same-size
/// field reorder can otherwise corrupt queries without failing to load.
pub fn verify_reverse(dir: &Path) -> Result<(), String> {
    if !cfg!(target_endian = "little") {
        return Err("reverse index requires a little-endian host".into());
    }
    let path = dir.join("manifest_reverse.json");
    let bytes = std::fs::read(&path).map_err(|e| {
        format!(
            "{}: missing reverse schema ({e}); rebuild the index",
            path.display()
        )
    })?;
    let manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{}: invalid reverse schema: {e}", path.display()))?;

    use crate::i18n::I18nRecord;
    use crate::{AddrPoint, AdminPolygon, InterpWay, NodeCoord, PlacePoint, PoiPoint, WayHeader};
    use std::mem::{offset_of, size_of};
    let records = json!({
        "WayHeader": {"size": size_of::<WayHeader>(), "fields": {
            "node_offset:u32": offset_of!(WayHeader, node_offset), "node_count:u8": offset_of!(WayHeader, node_count), "name_id:u32": offset_of!(WayHeader, name_id)
        }},
        "AddrPoint": {"size": size_of::<AddrPoint>(), "fields": {
            "lat:f32": offset_of!(AddrPoint, lat), "lng:f32": offset_of!(AddrPoint, lng), "housenumber_id:u32": offset_of!(AddrPoint, housenumber_id),
            "street_or_place_id:u32": offset_of!(AddrPoint, street_or_place_id), "unit_id:u32": offset_of!(AddrPoint, unit_id),
            "floor_id:u32": offset_of!(AddrPoint, floor_id), "parent_place_id:u32": offset_of!(AddrPoint, parent_place_id),
            "postcode_id:u32": offset_of!(AddrPoint, postcode_id), "flags:u8": offset_of!(AddrPoint, flags)
        }},
        "InterpWay": {"size": size_of::<InterpWay>(), "fields": {
            "node_offset:u32": offset_of!(InterpWay, node_offset), "node_count:u8": offset_of!(InterpWay, node_count), "street_id:u32": offset_of!(InterpWay, street_id),
            "start_number:u32": offset_of!(InterpWay, start_number), "end_number:u32": offset_of!(InterpWay, end_number), "interpolation:u8": offset_of!(InterpWay, interpolation)
        }},
        "AdminPolygon": {"size": size_of::<AdminPolygon>(), "fields": {
            "vertex_offset:u32": offset_of!(AdminPolygon, vertex_offset), "vertex_count:u16": offset_of!(AdminPolygon, vertex_count), "name_id:u32": offset_of!(AdminPolygon, name_id),
            "admin_level:u8": offset_of!(AdminPolygon, admin_level), "importance:u8": offset_of!(AdminPolygon, importance), "area:f32": offset_of!(AdminPolygon, area),
            "country_code:u16": offset_of!(AdminPolygon, country_code)
        }},
        "NodeCoord": {"size": size_of::<NodeCoord>(), "fields": {"lat:f32": offset_of!(NodeCoord, lat), "lng:f32": offset_of!(NodeCoord, lng)}},
        "PlacePoint": {"size": size_of::<PlacePoint>(), "fields": {
            "lat:f32": offset_of!(PlacePoint, lat), "lng:f32": offset_of!(PlacePoint, lng), "name_id:u32": offset_of!(PlacePoint, name_id),
            "rank:u8": offset_of!(PlacePoint, rank), "importance:u8": offset_of!(PlacePoint, importance)
        }},
        "PoiPoint": {"size": size_of::<PoiPoint>(), "fields": {
            "lat:f32": offset_of!(PoiPoint, lat), "lng:f32": offset_of!(PoiPoint, lng), "name_id:u32": offset_of!(PoiPoint, name_id),
            "category_id:u32": offset_of!(PoiPoint, category_id), "rank:u8": offset_of!(PoiPoint, rank), "importance:u8": offset_of!(PoiPoint, importance),
            "parent_place_id:u32": offset_of!(PoiPoint, parent_place_id)
        }},
        "I18nName": {"size": size_of::<I18nRecord>(), "fields": {
            "entity_type:u8": offset_of!(I18nRecord, entity_type), "alias_type:u8": offset_of!(I18nRecord, alias_type), "lang_code:u16": offset_of!(I18nRecord, lang_code),
            "entity_id:u32": offset_of!(I18nRecord, entity_id), "name_id:u32": offset_of!(I18nRecord, name_id), "_pad1:u32": offset_of!(I18nRecord, _pad1)
        }},
        "GeoCell": {"size": 20, "fields": {"cell_id:u64": 0, "street_offset:u32": 8, "addr_offset:u32": 12, "interp_offset:u32": 16}},
        "CellOffset": {"size": 12, "fields": {"cell_id:u64": 0, "entry_offset:u32": 8}},
    });
    let mut files = serde_json::Map::new();
    for name in [
        "addr_entries.bin",
        "addr_points.bin",
        "admin_cells.bin",
        "admin_entries.bin",
        "admin_polygons.bin",
        "admin_vertices.bin",
        "geo_cells.bin",
        "i18n_names.bin",
        "interp_entries.bin",
        "interp_nodes.bin",
        "interp_ways.bin",
        "place_cells.bin",
        "place_entries.bin",
        "place_points.bin",
        "poi_cells.bin",
        "poi_entries.bin",
        "poi_points.bin",
        "street_entries.bin",
        "street_nodes.bin",
        "street_ways.bin",
        "strings.bin",
    ] {
        let file = dir.join(name);
        let len = std::fs::metadata(&file)
            .map_err(|e| format!("{}: {e}; reverse schema incomplete", file.display()))?
            .len();
        files.insert(name.into(), Value::from(len));
    }
    let expected = json!({"version": 3, "byte_order": "little", "string_pool_zero_empty": true,
        "records": records, "files": files});
    if manifest.get("schema") != Some(&expected) {
        return Err(format!(
            "{}: reverse schema or file lengths do not match this reader; rebuild the index",
            path.display()
        ));
    }
    // Size/layout checks cannot detect a missing empty-string sentinel.
    // A zero ID would then resolve to the first real tag for every unset
    // optional field, so validate its actual byte before mapping records.
    use std::io::Read;
    let mut first = [0u8; 1];
    std::fs::File::open(dir.join("strings.bin"))
        .and_then(|mut file| file.read_exact(&mut first))
        .map_err(|e| format!("{}: cannot read string pool sentinel: {e}", dir.display()))?;
    if first[0] != 0 {
        return Err(format!(
            "{}: string pool offset zero is not empty; rebuild the index",
            dir.display()
        ));
    }
    Ok(())
}

/// Git SHA of the source tree when the binary was compiled. Captured
/// by `build.rs`. `"unknown"` when the build host has no git access.
pub fn git_sha() -> &'static str {
    env!("GEOCODER_GIT_SHA")
}

/// Working-tree state at compile time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitDirty {
    Clean,
    Dirty,
    Unknown,
}

/// Parsed view of the `GEOCODER_GIT_DIRTY` build-time env var. Used
/// by the manifest writer; downstream code should prefer this enum
/// over re-parsing the raw string.
pub fn git_dirty_state() -> GitDirty {
    match env!("GEOCODER_GIT_DIRTY") {
        "true" => GitDirty::Dirty,
        "false" => GitDirty::Clean,
        _ => GitDirty::Unknown,
    }
}

/// Write `<dir>/manifest_<tool>.json`. `extra` is merged into the
/// top-level object so callers can record their tool-specific stats
/// (place counts, country breakdown, etc.) without going through this
/// module.
pub fn write(dir: &Path, tool: &str, extra: Value) -> std::io::Result<()> {
    let now = chrono::Utc::now();
    let dirty = git_dirty_state();
    let mut obj = json!({
        "tool": tool,
        "git_sha": git_sha(),
        "git_dirty": dirty == GitDirty::Dirty,
        "git_dirty_known": dirty != GitDirty::Unknown,
        "built_at_unix": now.timestamp(),
        "built_at_iso": now.to_rfc3339(),
    });
    if let (Some(top), Value::Object(extras)) = (obj.as_object_mut(), extra) {
        for (k, v) in extras {
            top.insert(k, v);
        }
    }
    let path = dir.join(format!("manifest_{tool}.json"));
    let mut f = std::fs::File::create(&path)?;
    serde_json::to_writer_pretty(&mut f, &obj)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    f.write_all(b"\n")?;
    eprintln!("wrote {}", path.display());
    Ok(())
}
