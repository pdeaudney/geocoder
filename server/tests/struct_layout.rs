//! Pins the on-disk sizes of the mmap-reinterpret structs so a future Rust or
//! C++ field reorder (review item #8) turns into a compile-clean, run-time
//! visible failure instead of silent data corruption.
//!
//! These sizes must match the C++ `sizeof(...)` of the corresponding structs in
//! `builder/src/build_index.cpp`. The expected values encode the current
//! natural alignment / padding of each struct.

use std::mem::{align_of, size_of};

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
    // u32 + u16 + (2 pad) + u32 + u8 + (3 pad) + f32 + u16 + (2 pad) = 24
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
fn poi_point_size() {
    // f32 + f32 + u32 + u32 + u8 + (3 pad) + u32 = 24
    assert_eq!(size_of::<PoiPoint>(), 24);
    assert_eq!(align_of::<PoiPoint>(), 4);
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
