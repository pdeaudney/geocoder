# Pelias worldwide after WoF country-fallback — 2026-04-23

Follow-up to [`pelias-worldwide-baseline-2026-04-22.md`](pelias-worldwide-baseline-2026-04-22.md).
Adds Who's on First country-level polygons as a fallback for
Geofabrik extracts that omit their own `admin_level=2` relation
(GB, US). Rerun of the full per-country Pelias suite against the
rebuilt worldwide index.

## Headline

| Corpus | Before (OSM-only) | After (OSM + WoF fallback) | Δ |
|---|---:|---:|---:|
| au | 7/13 (54 %) | 6/13 (46 %) | −1 |
| ca | 1/12 (8 %) | 1/12 (8 %) | — |
| gb | 3/13 (23 %) | 3/13 (23 %) | — |
| nz | 0/4 (0 %) | 0/4 (0 %) | — |
| us | 14/79 (18 %) | 19/79 (24 %) | **+5** |
| **Total** | **25/121 (20.7 %)** | **29/121 (24.0 %)** | **+4** |

## What the WoF fix actually did for the index

Downstream coverage grew substantially — the headline Pelias number
doesn't reflect the full story.

| Metric | Before | After |
|---|---:|---:|
| OSM admin polygons with country_code | 54 / 102 073 | 54 / 102 073 (unchanged) |
| WoF country polygons loaded | — | 3 929 (after Douglas-Peucker) |
| `fst_unified` distinct keys | 555 078 (3 countries) | **3 028 026 (5 countries)** |
| `fst_us` | — | 4.7 M entries / 2.08 M keys |
| `fst_gb` | — | 692 k entries / 390 k keys |
| Forward (tantivy) streets indexed | 584 250 | 6 161 733 (10.5×) |

In other words: before this change, the US and GB autocomplete/search
paths were largely empty because `find_admin` returned no
country_code and the FST builder skipped every row. After WoF
fallback, full coverage for those two countries is live.

## Why Pelias pass rate only moved modestly

Pelias's corpus asserts on specific rankings (`results.0.*`) and
specific WoF-sourced fields (`layer`, `locality`, `country_a`). The
majority of the 92 failures trace back to causes unaffected by this
fix:

1. **Coverage gaps in our OSM data** — many Pelias cases target
   addresses present in Pelias's WoF+OA+TIGER stack but not in OSM
   (NZ side-streets in Kelburn, specific US street numbers sourced
   from TIGER).
2. **Ranking differences** — BM25 over the full-text index
   re-ordered some previously-top results because the corpus grew
   5.5× overnight. Net effect: +5 US cases, −1 AU case (see below).
3. **Schema differences** — the adapter drops Pelias's `layer`
   assertions silently, but some cases rely on layer filtering to
   select among multiple hits. No amount of admin fixing helps
   those.

## The AU regression

`pelias-pelias-combined-address_matching-6` — query
`22 HENSON STREET, NSW, 2204` — previously returned Henson Street
in Marrickville as the top hit. Now returns "Whinmoor Street". No
data was removed; the ranker chose a different document because the
BM25 corpus is larger. This is a real BM25 drift, not a WoF bug.

Fix paths (future work, not blocking):

- Tune the forward ranker to weight `country_code + postcode`
  co-occurrence more heavily.
- Explicitly per-country-partition the tantivy index (already
  supported via `build-forward-index --partition-by-country`);
  isolates BM25 corpora per country so US growth doesn't affect
  AU scores.

## How to reproduce

```bash
# 1. Once: download WoF per-country SQLite files.
WOF_COUNTRIES="au nz gb ca us" ./scripts/fetch-test-data.sh

# 2. Import country polygons into an existing combined index.
make wof-import WOF_INDEX_DIR=./data/index-worldwide

# 3. Rebuild forward + autocomplete FST (they read country_code through
#    find_admin, so they pick up the fallback automatically).
./target/release/build-forward-index ./data/index-worldwide
./target/release/build-autocomplete-fst ./data/index-worldwide --layout both

# 4. Run the suite.
make regression-worldwide
```

End-to-end adds ~15 min on top of a pre-existing worldwide build
(import: <3 s; forward rebuild: ~4 min; FST rebuild: ~4 min).

## What went into making it fast

Naive implementation — linear scan with on-the-fly bbox computation
per polygon per call — produced a 200 k-vertex US mainland PIP that
blew the FST build budget past half an hour with no visible progress.
Two optimisations got it to ~4 min:

1. **Precomputed sorted bboxes** at load time: each `find_country`
   call now does N four-comparison checks instead of N iterations of
   ring-length-many vertex comparisons.
2. **Douglas-Peucker simplification** at import time (ε ≈ 0.01° ≈
   1 km): UK mainland from 297 k vertices to ~5 k with no observable
   change in country-level PIP correctness — the 5 WoF probe coords
   still match their expected countries.

The tradeoff: a query ~1 km outside a country's actual boundary
(inside territorial waters, say) may return a stale classification.
Fine for land-based geocoding.

## Next moves

Top leverage items, ordered:

1. **Per-country tantivy partitioning** — the Pelias AU regression
   hints that our global BM25 corpus is now large enough that growth
   in one country perturbs scoring in another. Already built;
   flipping it on is a one-line change to `build-forward-index`.
2. **OpenAddresses supplement for US + GB + CA + NZ** — closes the
   "address exists in Pelias but not in OSM" gap, which is most of
   the remaining failures.
3. **Runner adjustment: top-N contains instead of strict
   `results.0`** — Pelias's own tester is already permissive; our
   runner is stricter than it needs to be. A small semantic
   relaxation would lift the pass rate without changing the code.
