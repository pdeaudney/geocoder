//! Criterion benchmarks for Index::query against a real index.
//!
//! Requires the environment variable `GEOCODER_INDEX_DIR` to point at a directory
//! containing a built index (the 14 .bin files produced by `build-index`). When
//! the variable is unset the benchmark exits cleanly without failing the build,
//! so running `cargo bench` on a machine without the index is still valid.
//!
//! Run:
//!   GEOCODER_INDEX_DIR=../data/index cargo bench

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use query_server::{
    Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};
use std::hint::black_box;

/// Representative query coordinates for Australian reverse-geocoding benchmarks.
/// Mix of urban / suburban / rural / no-hit so the benchmark surfaces both hot
/// and cold paths (empty cells, dense streets, polygon-in-point tests).
const FIXTURES: &[(&str, f64, f64)] = &[
    // Dense urban
    ("sydney_cbd",        -33.8688, 151.2093),
    ("melbourne_cbd",     -37.8136, 144.9631),
    ("brisbane_cbd",      -27.4698, 153.0251),
    ("perth_cbd",         -31.9523, 115.8613),
    ("adelaide_cbd",      -34.9285, 138.6007),
    // Suburban
    ("sydney_parramatta", -33.8150, 151.0011),
    ("melbourne_footscray", -37.8000, 144.8996),
    ("brisbane_chermside", -27.3847, 153.0306),
    // Rural / regional
    ("regional_orange",   -33.2833, 149.1000),
    ("regional_mildura",  -34.1889, 142.1583),
    // No-hit / ocean
    ("tasman_sea",        -35.0000, 155.0000),
    ("great_australian_bight", -36.0000, 130.0000),
];

fn load_index() -> Option<Index> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    match Index::load(
        &dir,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    ) {
        Ok(idx) => Some(idx),
        Err(e) => {
            eprintln!("WARN: failed to load index at {}: {}", dir, e);
            None
        }
    }
}

fn bench_reverse_geocode(c: &mut Criterion) {
    let Some(index) = load_index() else {
        eprintln!("SKIP: set GEOCODER_INDEX_DIR to run benchmarks against a real index");
        return;
    };

    let mut group = c.benchmark_group("reverse_geocode");
    group.throughput(Throughput::Elements(1));

    for (name, lat, lng) in FIXTURES {
        group.bench_with_input(BenchmarkId::new("query", name), &(*lat, *lng), |b, (lat, lng)| {
            b.iter(|| {
                let addr = index.query(black_box(*lat), black_box(*lng));
                black_box(addr);
            })
        });
    }

    // Mixed-fixture loop — approximates real traffic hitting unrelated cells.
    group.bench_function("query_mixed", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let (_, lat, lng) = FIXTURES[i % FIXTURES.len()];
            i = i.wrapping_add(1);
            let addr = index.query(black_box(lat), black_box(lng));
            black_box(addr);
        })
    });

    group.finish();

    // Micro-bench just the admin-boundary lookup (point-in-polygon work) to
    // isolate admin cost from the street/address/interp scans.
    let mut admin_group = c.benchmark_group("find_admin");
    admin_group.throughput(Throughput::Elements(1));
    for (name, lat, lng) in FIXTURES {
        admin_group.bench_with_input(BenchmarkId::new("find_admin", name), &(*lat, *lng), |b, (lat, lng)| {
            b.iter(|| {
                let r = index.find_admin(black_box(*lat), black_box(*lng));
                black_box(r.country);
                black_box(r.city);
            })
        });
    }
    admin_group.finish();

    // Isolate the geo (street / address / interpolation) scan.
    let mut geo_group = c.benchmark_group("query_geo");
    geo_group.throughput(Throughput::Elements(1));
    for (name, lat, lng) in FIXTURES {
        geo_group.bench_with_input(BenchmarkId::new("query_geo", name), &(*lat, *lng), |b, (lat, lng)| {
            b.iter(|| {
                let r = index.query_geo(black_box(*lat), black_box(*lng));
                black_box(r);
            })
        });
    }
    geo_group.finish();
}

criterion_group!(benches, bench_reverse_geocode);
criterion_main!(benches);
