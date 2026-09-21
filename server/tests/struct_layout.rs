//! Pins the on-disk sizes of the mmap-reinterpret structs so a future Rust or
//! C++ field reorder (review item #8) turns into a compile-clean, run-time
//! visible failure instead of silent data corruption.
//!
//! These sizes must match the C++ `sizeof(...)` of the corresponding structs in
//! `builder/src/build_index.cpp`. The expected values encode the current
//! natural alignment / padding of each struct.

use std::mem::{align_of, size_of};

use query_server::address_points::AddressPoint;
use query_server::{AddrPoint, AdminPolygon, InterpWay, NodeCoord, PlacePoint, PoiPoint, WayHeader};

#[test]
fn way_header_size() {
    // u32 + u8 + (3 bytes pad) + u32 = 12 on default alignment.
    assert_eq!(size_of::<WayHeader>(), 12);
    assert_eq!(align_of::<WayHeader>(), 4);
}

#[test]
fn addr_point_size() {
    // f32 + f32 + u32 + u32 + u32 + u32 + u32 + u8 + (3 pad) = 32
    // (housenumber_id, street_or_place_id, unit_id, floor_id,
    // parent_place_id, flags + pad)
    assert_eq!(size_of::<AddrPoint>(), 32);
    assert_eq!(align_of::<AddrPoint>(), 4);
}

#[test]
fn interp_way_size() {
    // u32 + u8 + (3 pad) + u32 + u32 + u32 + u8 + (3 pad) = 24
    assert_eq!(size_of::<InterpWay>(), 24);
    assert_eq!(align_of::<InterpWay>(), 4);
}

#[test]
fn admin_polygon_size() {
    // u32 + u16 + (2 pad) + u32 + u8 + u8 + (2 pad) + f32 + u16 + (2 pad) = 24
    // (one byte of the original 3-byte padding after admin_level was
    //  repurposed as the new `importance` field — size unchanged.)
    assert_eq!(size_of::<AdminPolygon>(), 24);
    assert_eq!(align_of::<AdminPolygon>(), 4);
}

#[test]
fn node_coord_size() {
    assert_eq!(size_of::<NodeCoord>(), 8);
    assert_eq!(align_of::<NodeCoord>(), 4);
}

#[test]
fn place_point_size() {
    // f32 + f32 + u32 + u8 (rank) + u8 (importance) + 2B pad = 16
    assert_eq!(size_of::<PlacePoint>(), 16);
    assert_eq!(align_of::<PlacePoint>(), 4);
}

#[test]
fn address_point_size() {
    // f32 + f32 + u32 + u32 + u32 + u32 + u32 = 28
    // (lat, lng, housenumber_id, street_id, locality_id, postcode_id,
    //  unit_id). Pinned so a future field add to the G-NAF/OpenAddresses
    //  on-disk format is caught at compile/test time, not after a 13 h
    //  G-NAF rebuild ships a corrupt index.
    assert_eq!(size_of::<AddressPoint>(), 28);
    assert_eq!(align_of::<AddressPoint>(), 4);
}

#[test]
fn poi_point_size() {
    // f32 + f32 + u32 + u32 + u8 + (3 pad) + u32 = 24
    assert_eq!(size_of::<PoiPoint>(), 24);
    assert_eq!(align_of::<PoiPoint>(), 4);
}

// --- Field-offset round-trip tests ---
//
// `*_size` tests above pin only the byte size, not field offsets. A
// silent reorder of fields between the C++ writer and the Rust mirror
// (e.g. swapping `importance` with `_pad` when both are u8) would
// preserve the size assertion while reading garbage at runtime. The
// round-trip tests below hand-construct the exact byte sequence the
// C++ builder writes, cast it via the same `repr(C)` mirror the
// runtime uses, and assert every field reads back correctly. A drift
// in field order surfaces in milliseconds instead of after a 13-hour
// rebuild.

/// Read a single value of T from a byte slice via unaligned pointer
/// read — same shape `as_typed_slice` uses internally, but takes
/// `&[u8]` so tests can hand-construct the bytes without a real Mmap.
fn cast_one<T: Copy>(bytes: &[u8]) -> T {
    assert_eq!(bytes.len(), size_of::<T>(), "byte length must match T");
    // SAFETY: caller guarantees `bytes.len() == size_of::<T>()`. The
    // unaligned read is correct regardless of `bytes`'s alignment —
    // the same guarantee `as_typed_slice` relies on against an mmap.
    unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) }
}

#[test]
fn place_point_field_offsets_round_trip() {
    // Layout the C++ builder writes (little-endian on every platform
    // we ship to — x86_64 / aarch64). Field order matches
    // builder/src/build_index.cpp `struct PlacePoint`:
    //   f32 lat | f32 lng | u32 name_id | u8 rank | u8 importance | 2B pad
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&(-33.8688_f32).to_le_bytes());  // lat
    bytes[4..8].copy_from_slice(&151.2093_f32.to_le_bytes());    // lng
    bytes[8..12].copy_from_slice(&0xCAFEBABE_u32.to_le_bytes()); // name_id
    bytes[12] = 16;                                              // rank
    bytes[13] = 200;                                             // importance
    // bytes[14..16] = pad

    let p: PlacePoint = cast_one(&bytes);
    assert!((p.lat - -33.8688).abs() < 1e-4, "lat mismatch: {}", p.lat);
    assert!((p.lng - 151.2093).abs() < 1e-4, "lng mismatch: {}", p.lng);
    assert_eq!(p.name_id, 0xCAFEBABE, "name_id reads wrong field");
    assert_eq!(p.rank, 16, "rank reads wrong byte");
    assert_eq!(
        p.importance, 200,
        "importance reads wrong byte — likely swapped with pad"
    );
}

#[test]
fn admin_polygon_field_offsets_round_trip() {
    // Layout the C++ builder writes. Field order matches
    // builder/src/build_index.cpp `struct AdminPolygon`:
    //   u32 vertex_offset | u16 vertex_count | 2B pad | u32 name_id |
    //   u8 admin_level | u8 importance | 2B pad | f32 area |
    //   u16 country_code | 2B pad
    //
    // `importance` lives in what used to be padding; if it ever
    // accidentally swaps with the admin_level byte (or the bytes that
    // remain padding), this round-trip catches it before a 13h rebuild
    // ships an admin index where every polygon ranks at importance=0.
    let mut bytes = [0u8; 24];
    bytes[0..4].copy_from_slice(&0xDEADBEEF_u32.to_le_bytes());   // vertex_offset
    bytes[4..6].copy_from_slice(&0x1234_u16.to_le_bytes());       // vertex_count
    // bytes[6..8] = pad
    bytes[8..12].copy_from_slice(&0xCAFEBABE_u32.to_le_bytes());  // name_id
    bytes[12] = 6;                                                // admin_level
    bytes[13] = 200;                                              // importance
    // bytes[14..16] = pad
    bytes[16..20].copy_from_slice(&12345.5_f32.to_le_bytes());    // area
    bytes[20..22].copy_from_slice(&0xABCD_u16.to_le_bytes());     // country_code
    // bytes[22..24] = pad

    let p: AdminPolygon = cast_one(&bytes);
    assert_eq!(p.vertex_offset, 0xDEADBEEF, "vertex_offset reads wrong field");
    assert_eq!(p.vertex_count, 0x1234, "vertex_count reads wrong field");
    assert_eq!(p.name_id, 0xCAFEBABE, "name_id reads wrong field");
    assert_eq!(p.admin_level, 6, "admin_level reads wrong byte");
    assert_eq!(
        p.importance, 200,
        "importance reads wrong byte — likely swapped with admin_level or pad"
    );
    assert!((p.area - 12345.5).abs() < 1e-1, "area mismatch: {}", p.area);
    assert_eq!(p.country_code, 0xABCD, "country_code reads wrong field");
}

#[test]
fn poi_point_field_offsets_round_trip() {
    // Layout the C++ builder writes. Field order matches
    // builder/src/build_index.cpp `struct PoiPoint`:
    //   f32 lat | f32 lng | u32 name_id | u32 category_id |
    //   u8 rank | u8 importance | 2B pad | u32 parent_place_id
    let mut bytes = [0u8; 24];
    bytes[0..4].copy_from_slice(&(48.8566_f32).to_le_bytes());   // lat
    bytes[4..8].copy_from_slice(&2.3522_f32.to_le_bytes());      // lng
    bytes[8..12].copy_from_slice(&0xAAAAAAAA_u32.to_le_bytes()); // name_id
    bytes[12..16].copy_from_slice(&0xBBBBBBBB_u32.to_le_bytes());// category_id
    bytes[16] = 10;                                              // rank
    bytes[17] = 180;                                             // importance
    // bytes[18..20] = pad
    bytes[20..24].copy_from_slice(&0xCCCCCCCC_u32.to_le_bytes());// parent_place_id

    let p: PoiPoint = cast_one(&bytes);
    assert!((p.lat - 48.8566).abs() < 1e-4, "lat mismatch: {}", p.lat);
    assert!((p.lng - 2.3522).abs() < 1e-4, "lng mismatch: {}", p.lng);
    assert_eq!(p.name_id, 0xAAAAAAAA, "name_id reads wrong field");
    assert_eq!(p.category_id, 0xBBBBBBBB, "category_id reads wrong field");
    assert_eq!(p.rank, 10, "rank reads wrong byte");
    assert_eq!(
        p.importance, 180,
        "importance reads wrong byte — likely swapped with pad"
    );
    assert_eq!(
        p.parent_place_id, 0xCCCCCCCC,
        "parent_place_id at wrong offset — likely the rank/importance/pad block shifted"
    );
}

#[test]
fn kind_constants_agree_across_modules() {
    // forward::KIND_* and autocomplete::KIND_* are duplicated so the
    // autocomplete module compiles without depending on forward (and
    // vice-versa). Their numeric values MUST agree — the FST
    // payload's `kind` byte is read by code that imports either
    // module's constants. A drift would silently misclassify hits.
    use query_server::{
        autocomplete::{KIND_PLACE as A_PLACE, KIND_POI as A_POI, KIND_STREET as A_STREET},
        forward::{KIND_PLACE as F_PLACE, KIND_POI as F_POI, KIND_STREET as F_STREET},
    };
    assert_eq!(A_PLACE as u64, F_PLACE);
    assert_eq!(A_STREET as u64, F_STREET);
    assert_eq!(A_POI as u64, F_POI);
    // Sanity: all three values distinct from each other.
    assert_ne!(A_PLACE, A_STREET);
    assert_ne!(A_STREET, A_POI);
    assert_ne!(A_PLACE, A_POI);
}
