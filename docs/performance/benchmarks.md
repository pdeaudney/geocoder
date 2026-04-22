# Performance benchmarks

Machine-generated. Do not hand-edit — re-run `scripts/run-benchmarks.sh` to refresh.

- **Captured**:    2026-04-22T21:01:43Z
- **Platform**:    Darwin 24.6.0
- **CPU**:         Apple M1 Max
- **RAM**:         64 GB
- **Compiler**:    rustc 1.89.0 (29483883e 2025-08-04)
- **Index**:       ./data/index (AU only)
- **Bench tool**:  criterion (warmup=1s, measure=3s)

## Reverse geocoding

Three benchmark groups, each re-run against the twelve fixtures
defined in `server/benches/reverse_geocode.rs`:

- **`reverse_geocode/query`** — full `Index::query(lat, lng)`:
  S2 cell lookup → admin polygon point-in-polygon → postcode
  enrichment → display-name formatting.
- **`find_admin`** — admin polygon resolution only (the historical
  hot spot). Useful to see how much of the full-query cost is
  polygon containment vs. formatting.
- **`query_geo`** — the low-level geo cell lookup without admin
  processing. Closest to "hash + mmap read" cost.

Fixtures mix dense-urban, suburban, rural, and no-hit coords so
medians capture both hot (dense admin cells) and cold (ocean —
polygon index returns quickly) paths.

### `reverse_geocode/query`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `sydney_cbd` | 66.53 µs | 1.27 µs |
| `melbourne_cbd` | 35.07 µs | 3.12 µs |
| `brisbane_cbd` | 20.00 µs | 184 ns |
| `perth_cbd` | 29.12 µs | 462 ns |
| `adelaide_cbd` | 19.73 µs | 379 ns |
| `sydney_parramatta` | 51.83 µs | 1.19 µs |
| `melbourne_footscray` | 26.59 µs | 490 ns |
| `brisbane_chermside` | 31.10 µs | 522 ns |
| `regional_orange` | 14.47 µs | 230 ns |
| `regional_mildura` | 11.38 µs | 196 ns |
| `tasman_sea` | 1.53 µs | 32 ns |
| `great_australian_bight` | 1.53 µs | 19 ns |

### `reverse_geocode`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `query_mixed` | 27.17 µs | 649 ns |

### `find_admin`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `sydney_cbd` | 54.57 µs | 31.98 µs |
| `melbourne_cbd` | 32.71 µs | 564 ns |
| `brisbane_cbd` | 17.96 µs | 318 ns |
| `perth_cbd` | 25.51 µs | 600 ns |
| `adelaide_cbd` | 16.35 µs | 315 ns |
| `sydney_parramatta` | 45.02 µs | 1.69 µs |
| `melbourne_footscray` | 22.61 µs | 5.97 µs |
| `brisbane_chermside` | 29.75 µs | 415 ns |
| `regional_orange` | 10.17 µs | 90 ns |
| `regional_mildura` | 8.96 µs | 1.44 µs |
| `tasman_sea` | 612 ns | 12 ns |
| `great_australian_bight` | 609 ns | 8 ns |

### `query_geo`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `sydney_cbd` | 1.51 µs | 109 ns |
| `melbourne_cbd` | 2.15 µs | 34 ns |
| `brisbane_cbd` | 1.80 µs | 51 ns |
| `perth_cbd` | 1.55 µs | 144 ns |
| `adelaide_cbd` | 1.30 µs | 29 ns |
| `sydney_parramatta` | 1.59 µs | 24 ns |
| `melbourne_footscray` | 1.57 µs | 279 ns |
| `brisbane_chermside` | 1.21 µs | 13 ns |
| `regional_orange` | 1.27 µs | 73 ns |
| `regional_mildura` | 1.23 µs | 85 ns |
| `tasman_sea` | 924 ns | 20 ns |
| `great_australian_bight` | 927 ns | 15 ns |

## Notes

- Criterion's µs-to-ns threshold is 1000; sub-µs fixtures are
  rendered in ns to preserve precision.
- The cold-path fixtures (`tasman_sea`, `great_australian_bight`)
  measure empty-cell lookup — the index correctly bails without
  probing any polygon. The sub-µs number is mostly cache-resident
  hash math.
- Urban centroids hit the densest admin polygons (Sydney has
  hundreds of overlapping suburb boundaries). Rural coords scan a
  much smaller candidate set, which is why they run faster.

## How to re-run

```bash
# Defaults: ./data/index as the index dir, 1s warmup + 3s measurement.
./scripts/run-benchmarks.sh

# Longer runs produce tighter confidence intervals at the cost of time.
BENCH_MEASURE_SEC=10 BENCH_WARMUP_SEC=3 ./scripts/run-benchmarks.sh
```

Criterion also writes HTML reports under `target/criterion/` — open
`target/criterion/report/index.html` for per-fixture histograms and
change detection vs. the previous run.
