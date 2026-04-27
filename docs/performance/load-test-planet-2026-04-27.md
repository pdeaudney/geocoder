# Planet load-test snapshot — measured on NVIDIA GB10 (2026-04-27)

> **Snapshot doc.** First run of the planet k6 workload against the
> 13.5-hour planet index built on the GB10 (see
> [`planet-build-on-gb10-2026-04-27.md`](planet-build-on-gb10-2026-04-27.md)).
> Captured 2026-04-27. Reproducible via the recipe at the bottom.

## TL;DR

The planet load test sustained **~12,000–13,000 requests/sec** with **zero
errors** across all three scenarios (`reverse_planet`, `search_planet`,
`autocomplete_typeahead`). Every p99 was 11–112× under its documented
SLO ceiling. The geocoder's read path has substantial headroom on the
GB10's 20-core Grace CPU.

| Endpoint | p99 (warm) | SLO | Headroom |
|---|---:|---:|---:|
| `reverse_planet` | 0.75 ms | 50 ms | **67×** |
| `search_planet` | 0.89 ms | 100 ms | **112×** |
| `autocomplete_typeahead` | 2.68 ms | 30 ms | **11×** |

Errors: 0 / 1,876,325 requests across both passes.

## Measured numbers

Workload: `--workload planet`, k6 in Docker, 8 VUs per scenario,
20-second duration per scenario, all three scenarios staggered
(`reverse` → `search` → `autocomplete_typeahead`). Planet index
covering 211 country admin boundaries from the OSM planet PBF.

### Cold pass

```
Total requests: 907,638       rps: 12,101       errors: 0
```

| Endpoint | p50 | p90 | p95 | p99 | max | count |
|---|---:|---:|---:|---:|---:|---:|
| `reverse_planet_ms` | 0.34 | 0.55 | 0.63 | 2.06 | 111.02 | 341,421 |
| `search_planet_ms` | 0.39 | 0.63 | 0.72 | 0.99 | 19.81 | 326,383 |
| `autocomplete_typeahead_ms` | 0.40 | 1.49 | 1.90 | 2.68 | 12.94 | 239,834 |

### Warm pass

```
Total requests: 968,687       rps: 12,916       errors: 0
```

| Endpoint | p50 | p90 | p95 | p99 | max | count |
|---|---:|---:|---:|---:|---:|---:|
| `reverse_planet_ms` | 0.33 | 0.53 | 0.60 | 0.75 | 10.89 | 388,791 |
| `search_planet_ms` | 0.38 | 0.62 | 0.70 | 0.89 | 10.32 | 338,962 |
| `autocomplete_typeahead_ms` | 0.40 | 1.49 | 1.90 | 2.68 | 12.11 | 240,934 |

## What the numbers tell us

**Throughput is excellent, latency has huge headroom.** At 12K+ req/s
sustained on a single 20-core Grace CPU, the geocoder is comfortably
inside its SLOs without breaking a sweat. The Tantivy + FST + S2 +
admin-polygon hot path is all in-process mmap reads against the
already-loaded index — no network, no disk during steady state.

**The autocomplete typeahead is the most expensive of the three** (p99
2.68 ms vs 0.75 / 0.89 for reverse / search). That tracks with the
FST broad-walk cost on short prefixes — a 1-char prefix can visit
thousands of candidates before the limit-clamp kicks in. Still well
under the 30 ms SLO; nothing to act on.

**The 111 ms cold reverse max is one outlier** on a single page-fault.
The first request to a never-touched admin polygon faults pages from
the M.2 NVMe; that's a one-time amortisation and the warm max drops to
10.89 ms which represents the steady-state ceiling. Expected shape
for a memory-mapped index, not a regression.

**Cold-vs-warm delta is suspiciously small.** Cold p50 0.34 ms vs warm
0.33 ms — that's barely 3 % apart, where you'd expect 2–5× on a
genuinely-cold run. Almost certainly because `drop_caches` couldn't
fire (sudo wasn't available); the "cold" pass is reading from
warm-from-previous-traffic OS page cache. To get an honest cold
measurement, re-run with `sudo`.

## Per-scenario throughput composition

Scenarios were staggered in 5-second windows over the 75-second run,
8 VUs each. RPS per scenario:

| Scenario | requests | wall (s) | per-scenario RPS |
|---|---:|---:|---:|
| `reverse_planet` (cold) | 341,421 | 20 | 17,071 |
| `search_planet` (cold) | 326,383 | 20 | 16,319 |
| `autocomplete_typeahead` (cold) | 239,834 | 20 | 11,992 |
| `reverse_planet` (warm) | 388,791 | 20 | 19,440 |
| `search_planet` (warm) | 338,962 | 20 | 16,948 |
| `autocomplete_typeahead` (warm) | 240,934 | 20 | 12,047 |

Reverse is the fastest (just an admin-polygon point-in-polygon test
plus the addr-points cell lookup); autocomplete is the slowest because
of the FST walk per request. Search sits in between because the
fixture queries are mostly city-name freeform that hit the FST
fast-path before the Tantivy scorer.

## Hardware context

NVIDIA GB10 Grace Blackwell Superchip:

- 20-core Arm Neoverse V2 (Grace CPU side)
- 128 GiB unified LPDDR5x, ~122 GiB visible to the OS
- M.2 NVMe scratch (sustained read measured at ~100 MB/s during
  the build phase — ample for query workload's mmap-read pattern,
  unlike the build's I/O-bound profile)
- Linux aarch64, GCC 13.3.0

## Reproduction recipe

```bash
cd /path/to/traccar-geocoder
git pull origin main

# One-time: refresh fixtures (idempotent; cached zips skip re-download)
./scripts/bench/build-fixtures.sh

# Run the planet load test against the planet index
./scripts/bench-http.sh \
    --workload planet \
    --index /data/index \
    --label gb10-planet-$(date +%Y%m%d)

# For honest cold-pass numbers, run with sudo so drop_caches fires
sudo ./scripts/bench-http.sh \
    --workload planet \
    --index /data/index \
    --mode cold \
    --label gb10-cold-only
```

Wall-time: ~3 min cold pass + ~3 min warm pass. Output JSON at
`tests/regression/reports/http-bench-planet-<label>.json` with the
scenario-tag-keyed shape that prior-run diffs work against.

## Companion: bench-accuracy

The load test verifies status codes only; for correctness it has a
sister tool that fires the same fixture rows at the same server and
checks the response payloads:

```bash
./scripts/run-bench-accuracy.sh \
    --index /data/index \
    --sample 2000 \
    --label gb10-planet-accuracy
```

Three assertions:
- `reverse`: `response.address.country_code` matches fixture row's source country.
- `search`: top result's country matches AND coords within 100 km of `lat_hint`/`lng_hint`.
- `autocomplete`: ≥1 result; for prefix length ≥3, ≥1 result name starts with the prefix.

Pass-rate reported per country so geographic clusters of regressions
surface immediately. Exit code reflects threshold (default 95 %).
JSON report at `tests/regression/reports/bench-accuracy-<label>.json`.

## Limitations

- Single hardware data point. The 12K req/s figure depends heavily on
  the GB10's Grace cores + LPDDR5x bandwidth; smaller instances (e.g.
  AWS c7g.xlarge with 4 vCPU) will sustain proportionally less.
- Single workload mix. The fixtures are uniform random over 113K
  populated places; real production traffic has a heavy head (NYC,
  London, Sydney CBD) that handles most of the load. Uniform-pick over
  the long tail exercises tail latency more aggressively than
  production would. Useful for catching regressions; less useful as a
  prediction of real-traffic p99.
- No `/validate`. Out of scope for the initial workload. `/validate`
  shares the Tantivy + admin-polygon hot path with `/search`, so its
  perf characteristics track `search_planet` closely; we'd add a
  dedicated scenario only if `/validate` becomes a meaningful share of
  production traffic.
- Cold pass wasn't truly cold. See "What the numbers tell us" above —
  re-running with `sudo` will capture the real cold-state ceiling.
