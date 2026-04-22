# Performance benchmarks

Machine-generated. Do not hand-edit — re-run `scripts/run-benchmarks.sh` to refresh.

- **Captured**:    2026-04-22T21:07:07Z
- **Platform**:    Darwin 24.6.0
- **CPU**:         Apple M1 Max
- **RAM**:         64 GB
- **Compiler**:    rustc 1.89.0 (29483883e 2025-08-04)
- **Index**:       ./data/index-worldwide (AU + NZ + GB + CA + US combined)
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
| `sydney_cbd` | 56.42 µs | 2.50 µs |
| `melbourne_cbd` | 35.26 µs | 426 ns |
| `brisbane_cbd` | 20.72 µs | 2.79 µs |
| `perth_cbd` | 27.41 µs | 462 ns |
| `adelaide_cbd` | 18.28 µs | 510 ns |
| `sydney_parramatta` | 47.03 µs | 6.13 µs |
| `melbourne_footscray` | 24.68 µs | 594 ns |
| `brisbane_chermside` | 31.56 µs | 330 ns |
| `regional_orange` | 11.96 µs | 908 ns |
| `regional_mildura` | 10.48 µs | 115 ns |
| `tasman_sea` | 6.99 µs | 260 ns |
| `great_australian_bight` | 10.04 µs | 975 ns |

### `reverse_geocode`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `query_mixed` | 25.93 µs | 1.53 µs |

### `find_admin`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `sydney_cbd` | 54.27 µs | 845 ns |
| `melbourne_cbd` | 32.55 µs | 3.05 µs |
| `brisbane_cbd` | 17.95 µs | 243 ns |
| `perth_cbd` | 25.34 µs | 562 ns |
| `adelaide_cbd` | 16.54 µs | 272 ns |
| `sydney_parramatta` | 45.26 µs | 969 ns |
| `melbourne_footscray` | 22.90 µs | 281 ns |
| `brisbane_chermside` | 30.42 µs | 720 ns |
| `regional_orange` | 10.50 µs | 462 ns |
| `regional_mildura` | 9.12 µs | 1.10 µs |
| `tasman_sea` | 5.86 µs | 87 ns |
| `great_australian_bight` | 8.98 µs | 688 ns |

### `query_geo`

| Fixture | Median | ±σ |
|---------|-------:|---:|
| `sydney_cbd` | 1.62 µs | 22 ns |
| `melbourne_cbd` | 2.25 µs | 32 ns |
| `brisbane_cbd` | 1.95 µs | 103 ns |
| `perth_cbd` | 1.63 µs | 31 ns |
| `adelaide_cbd` | 1.44 µs | 32 ns |
| `sydney_parramatta` | 1.71 µs | 176 ns |
| `melbourne_footscray` | 1.70 µs | 32 ns |
| `brisbane_chermside` | 1.32 µs | 121 ns |
| `regional_orange` | 1.41 µs | 29 ns |
| `regional_mildura` | 1.36 µs | 95 ns |
| `tasman_sea` | 1.04 µs | 70 ns |
| `great_australian_bight` | 1.04 µs | 180 ns |

## Notes

- Criterion's µs-to-ns threshold is 1000; sub-µs fixtures are
  rendered in ns to preserve precision.
- **Ocean / no-match fixtures are noticeably slower here than in
  the AU-only benchmarks.** `tasman_sea` rises from 1.5 µs to 7 µs;
  the Great Australian Bight's `find_admin` rises from 0.6 µs to
  9 µs. The cause is the Who's on First country fallback added to
  `find_admin`: when OSM returns no country_code, the fallback
  scans ~3 929 precomputed bboxes (one per simplified WoF country
  ring). Every bbox misses for ocean points, so the full scan runs
  before returning None. Adds ~5–10 µs to every oceanic /
  unclassified coord. Land-based queries unaffected.
- Urban centroids hit the densest admin polygons (Sydney has
  hundreds of overlapping suburb boundaries). Rural coords scan a
  much smaller candidate set, which is why they run faster.
- Several AU fixtures are faster on the worldwide index than on
  the AU-only index (e.g. `sydney_cbd` 66 µs → 56 µs). Within
  criterion noise + warming differences; the hot path is
  unchanged by the WoF code.

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
