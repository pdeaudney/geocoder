# Performance benchmarks

Machine-generated. Do not hand-edit — re-run `scripts/run-benchmarks.sh` to refresh.

- **Captured**:    2026-04-20T21:44:22Z
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
| `sydney_cbd` | 69.66 µs | 5.91 µs |
| `melbourne_cbd` | 37.40 µs | 7.25 µs |
| `brisbane_cbd` | 21.01 µs | 1.19 µs |
| `perth_cbd` | 31.15 µs | 2.85 µs |
| `adelaide_cbd` | 20.91 µs | 3.56 µs |
| `sydney_parramatta` | 55.63 µs | 5.03 µs |
| `melbourne_footscray` | 28.17 µs | 1.49 µs |
| `brisbane_chermside` | 34.20 µs | 15.58 µs |
| `regional_orange` | 16.81 µs | 3.49 µs |
| `regional_mildura` | 14.35 µs | 2.38 µs |
| `tasman_sea` | 2.01 µs | 1.52 µs |
| `great_australian_bight` | 1.76 µs | 480 ns |

### `reverse_geocode`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `query_mixed` | 32.77 µs | 4.51 µs |

### `find_admin`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `sydney_cbd` | 57.41 µs | 10.35 µs |
| `melbourne_cbd` | 35.16 µs | 3.50 µs |
| `brisbane_cbd` | 18.75 µs | 2.21 µs |
| `perth_cbd` | 29.74 µs | 7.66 µs |
| `adelaide_cbd` | 17.55 µs | 1.62 µs |
| `sydney_parramatta` | 48.70 µs | 5.11 µs |
| `melbourne_footscray` | 23.83 µs | 36.67 µs |
| `brisbane_chermside` | 31.86 µs | 2.65 µs |
| `regional_orange` | 11.01 µs | 1.17 µs |
| `regional_mildura` | 10.82 µs | 3.02 µs |
| `tasman_sea` | 656 ns | 74 ns |
| `great_australian_bight` | 669 ns | 81 ns |

### `query_geo`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `sydney_cbd` | 1.81 µs | 482 ns |
| `melbourne_cbd` | 2.42 µs | 212 ns |
| `brisbane_cbd` | 2.19 µs | 729 ns |
| `perth_cbd` | 1.59 µs | 110 ns |
| `adelaide_cbd` | 1.43 µs | 256 ns |
| `sydney_parramatta` | 1.79 µs | 179 ns |
| `melbourne_footscray` | 1.64 µs | 117 ns |
| `brisbane_chermside` | 1.34 µs | 269 ns |
| `regional_orange` | 1.33 µs | 198 ns |
| `regional_mildura` | 1.36 µs | 370 ns |
| `tasman_sea` | 1.09 µs | 173 ns |
| `great_australian_bight` | 1.03 µs | 181 ns |

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
