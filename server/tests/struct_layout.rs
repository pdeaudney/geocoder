//! Pins the on-disk sizes of the mmap-reinterpret structs so a future Rust or
//! C++ field reorder (review item #8) turns into a compile-clean, run-time
//! visible failure instead of silent data corruption.
//!
//! These sizes must match the C++ `sizeof(...)` of the corresponding structs in
//! `builder/src/build_index.cpp`. The expected values encode the current
//! natural alignment / padding of each struct.

use std::mem::{align_of, size_of};

use query_server::{AddrPoint, AdminPolygon, InterpWay, NodeCoord, PlacePoint, WayHeader};

#[test]
fn way_header_size() {
    // u32 + u8 + (3 bytes pad) + u32 = 12 on default alignment.
    assert_eq!(size_of::<WayHeader>(), 12);
    assert_eq!(align_of::<WayHeader>(), 4);
}

#[test]
fn addr_point_size() {
    // f32 + f32 + u32 + u32 = 16
    assert_eq!(size_of::<AddrPoint>(), 16);
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
    // f32 + f32 + u32 + u8 + 3B pad = 16
    assert_eq!(size_of::<PlacePoint>(), 16);
    assert_eq!(align_of::<PlacePoint>(), 4);
}
