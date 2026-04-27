# Planet load-test snapshot — pending GB10 measurement (2026-04-27)

> **Snapshot doc, placeholder.** Built the planet load-test
> infrastructure on 2026-04-27 (fixture-build script, three new k6
> scenarios, `--workload planet` flag in `scripts/bench-http.sh`).
> The numbers themselves come from a future run against the GB10's
> 13.5-hour planet index. This file gets filled in once that run
> happens; the structure mirrors the existing perf snapshots so a
> future engineer reviewing the cluster can compare like-for-like.

## What's measurable now

The fixture build emitted on the dev machine (committed into git):

| File | Entries | Size |
|---|---:|---:|
| `scripts/bench/fixtures/places.json` | 113,371 | 9.7 MiB |
| `scripts/bench/fixtures/reverse_coords.json` | 5,000 | 353 KiB |
| `scripts/bench/fixtures/search_queries.json` | 2,000 | 154 KiB |
| `scripts/bench/fixtures/autocomplete_prefixes.json` | 1,458 | 58 KiB |

Per-country balance (`reverse_coords.json`, 625 each):
US, GB, FR, DE, NL, ES, AU, CA.

Autocomplete prefix length distribution: lengths 1–6 evenly weighted
(~250 each, except length 1 capped at 208 because there are fewer
distinct first-char × country pairs than the 250 target).

## Reproduction recipe (to be run on the GB10)

```bash
cd /path/to/traccar-geocoder
git pull origin main

# One-time: refresh fixtures from upstream (idempotent; cached zips
# under scripts/bench/fixtures/.cache/ skip re-download).
./scripts/bench/build-fixtures.sh

# Run the planet load test against the planet index.
./scripts/bench-http.sh \
    --workload planet \
    --index /data/index \
    --label gb10-planet-$(date +%Y%m%d)
```

Expected wall-time: ~3 minutes per mode (cold + warm = ~6 minutes
total). The wrapper drops the OS page cache before the cold pass via
`drop_caches`.

## Numbers to capture once the run completes

For each of the two modes (cold + warm), record:

| Scenario | p50 | p90 | p95 | p99 | max | RPS | error rate |
|---|---:|---:|---:|---:|---:|---:|---:|
| `reverse_planet` | ? | ? | ? | ? | ? | ? | ? |
| `search_planet` | ? | ? | ? | ? | ? | ? | ? |
| `autocomplete_typeahead` | ? | ? | ? | ? | ? | ? | ? |

p99 thresholds (from `docs/sli-slo.md`):

  - `reverse_planet`: ≤ 50 ms
  - `search_planet`: ≤ 100 ms
  - `autocomplete_typeahead`: ≤ 30 ms

If any exceed their threshold, the k6 run exits non-zero and the
JSON summary records the breach.

## What to look for in the warm vs cold delta

For a properly-warmed planet index on the GB10:

| Phase | What dominates |
|---|---|
| Cold pass | Page-cache misses on the first read from each region. tantivy term-dictionary loads, FST opens, admin-polygon mmap. The first ~100 requests are well outside steady state; rest of the run is bandwidth-bound on the M.2. |
| Warm pass | Steady state. Almost all reads from page cache. p99 dominated by the long tail of the most-expensive queries (broad-prefix autocomplete walks, multi-token freeform searches). |

A "healthy" warm-vs-cold delta: warm p99 ≈ 1/3 to 1/10 of cold p99
on autocomplete + search; reverse is typically less affected because
admin/geo cells are small enough that the cold-pass page-fault cost
amortises quickly.

## Known caveats

1. **Geonames places are mostly cities/towns, not addresses.** The
   `search_planet` workload exercises city-name → coords resolution,
   not full-address parsing. For an address-level forward workload,
   pull OpenAddresses CSV samples (with the caveat that they're 100 %
   overlap with the index → unrealistic hit rate) or the Pelias
   acceptance-tests `search_*.json` corpus (~500 hand-curated cases,
   AU/FR/US/KR coverage only).
2. **No structured `/search` traffic.** Every `search_planet` request
   uses the freeform `q=` path. Operators serving a heavy
   structured-query mix (typical of address-validation use cases)
   should add a structured scenario or run the existing AU
   workload's `search` scenario alongside.
3. **No `/validate` coverage.** Out of scope for the initial
   workload. `/validate` shares the tantivy + admin-polygon hot path
   with `/search`, so its perf characteristics track `search_planet`
   closely; we'd add a dedicated scenario only if `/validate` becomes
   a meaningful share of production traffic.
4. **Geonames data is independent of the index but not
   distribution-realistic.** Real users don't query random cities
   uniformly — a small head of high-traffic queries (NYC, LA, London,
   Sydney CBD) handles most production load. Uniform-pick over 113K
   places exercises tail latency more aggressively than production
   would. Useful for finding regressions; less useful for predicting
   real p99.

## What goes here once the run completes

  - The two tables above filled in with real numbers.
  - A "what we learned" paragraph: which p99 was the bottleneck,
    whether thresholds passed, any surprises in the warm-vs-cold
    delta or per-country variance.
  - Specific call-outs of any queries that hit > 1 s (likely indicators
    of edge-case parsing or unbounded prefix walks worth fixing).
  - Cross-reference to the GB10 build-time snapshot
    (`planet-build-on-gb10-2026-04-27.md`) so the build + serve
    profile of the same hardware is in one place.
