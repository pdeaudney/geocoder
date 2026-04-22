# Worldwide Pelias baseline — 2026-04-22

First run of the full Pelias acceptance-tests corpus against a
5-country combined index (AU, NZ, GB, CA, US). Captures the pass rate
we start from and the systemic reasons for the current failures, so
future regressions can be interpreted against a known baseline.

## Headline

**25 / 121 cases pass (20.7 %)** across the 5 countries we currently
build against.

| Country | Cases | Passed | Rate |
|---|---:|---:|---:|
| AU | 13 | 7 | 54 % |
| GB | 13 | 3 | 23 % |
| US | 79 | 14 | 18 % |
| CA | 12 | 1 | 8 % |
| NZ | 4 | 0 | 0 % |
| **Total** | **121** | **25** | **20.7 %** |

Command: `INDEX_DIR=./data/index-worldwide ./scripts/run-worldwide-regression.sh`.
Full per-case JSON lives in `tests/regression/reports/pelias-*-20260422T024724Z.json`
(gitignored — corpora in `tests/regression/corpora/` are the source of truth).

## Why "only" 20.7 %?

The Pelias gold set targets Pelias's own geocoder, which aggregates
Who's on First + OpenAddresses + TIGER + OSM. Our index is OSM-only
(plus G-NAF for AU). The failure modes below account for the bulk of
the gap:

### 1. Geofabrik country extracts miss the `admin_level=2` relation

The single biggest issue. Of the 102,073 admin polygons we built
across 5 countries, **only 54 carry a country_code**:

| cc | polygons |
|---|---:|
| NULL | 102,019 |
| AU | 39 |
| NZ | 11 |
| CA | 3 |
| IM | 1 |

**GB and US have zero country-level polygons.** Geofabrik's
`great-britain-latest.osm.pbf` and `us-latest.osm.pbf` extracts don't
include the country-level boundary relations because those relations
reference ways outside the extract window. The consequences:

- The autocomplete FST only knows about 3 countries (AU, CA, NZ) —
  every street/place we tried to index for GB or US fell through the
  country-code filter and was skipped.
- The reverse-geocode output still returns the road/city for a coord
  in GB or US, but often with an empty country_code field — breaking
  Pelias assertions that check `country_a = "USA"`.
- Forward search still works through the monolithic tantivy index,
  but per-country filtering is inconsistent.

**Fix path** (not yet done): download the Geofabrik
`*-latest.osm.poly` or the OSM world admin-boundary extract separately,
synthesise the missing country-level polygons before the reverse index
build. Or switch from per-country extracts to continent-level extracts
where the country relations are self-contained.

### 2. Coverage gaps in OSM data

Several Pelias cases target streets that exist in Pelias's WoF /
OpenAddresses index but aren't named in OSM or have different
tokenisation. Examples we hit:

- NZ cases all target "glen rd, kelburn" and "glasgow street, kelburn"
  in Wellington. Our index does contain Kelburn, Kelburn Parade, and
  Kelburn Viaduct, but not Glen Road or Glasgow Street — either not in
  OSM, not in Geofabrik's NZ extract, or named differently.
- Several US cases target addresses that Pelias resolves through TIGER
  (US Census Bureau road data). We don't ingest TIGER.

**Fix path**: augment OSM with OpenAddresses per-country data (already
supported via `build-openaddresses-index`, just not fetched for
worldwide yet) and/or consider TIGER ingestion for the US.

### 3. Ranking differences

Pelias's gold set expects a specific result ordering ("result 0
contains city=Brooklyn"). Our ranking uses a different signal mix
(tantivy BM25 + rank boost + per-country prior), so we sometimes
return the right place at rank 1 or 2 instead of rank 0. Currently the
runner asserts on `results.0.*`; these are flagged as failures even
when the correct answer is a couple rows down.

**Fix path**: tune the forward ranker (minor wins available) or relax
the runner's assertions to check "contains X in top N" rather than
strictly result 0 (bigger semantic shift — worth its own design doc).

### 4. Schema differences

Pelias emits a `layer` field per hit (`layer=locality`,
`layer=neighbourhood`, etc.) rooted in WoF. We don't emit that field.
Cases that assert on `layer` currently just skip those assertions
(the adapter drops them), but some Pelias cases implicitly depend on
the layer classification to select the right hit from a
multi-result response.

## What passes and why

The 25 passing cases are mostly:
- AU freeform queries with G-NAF backing (Deakin, Prahran, Rivett —
  the kinds of cases we wrote our own AU regression around).
- GB cases targeting London-area landmarks where OSM coverage is rich
  enough to rank our result similarly to Pelias's.
- US cases targeting major cities where "New York", "Los Angeles",
  "Chicago" as place=*  hits are dominant regardless of ranking.

## Next moves (ordered by value)

1. **Synthesise the missing country-level admin polygons for GB and
   US.** Highest-leverage single fix — unlocks the country_code
   plumbing for everything downstream.
2. **Build OpenAddresses per-country for US + GB + CA + NZ.** Closes
   most of the "street exists in Pelias but not in our OSM extract"
   failures.
3. **Revisit the runner's `results.0.*` assertions.** Changing to
   "top-N contains" where Pelias allows it would lift another 5–10 %.
4. **Per-country forward index partitioning.** Already supported via
   `build-forward-index --partition-by-country` but not used in the
   current worldwide build; with country_code fixed (item 1), this
   gives meaningful per-country query latency improvements too.

## Reproducibility

```bash
# 1. Download all 5 PBFs (~20 GB).
bash -c 'cd data/pbf && for u in \
    https://download.geofabrik.de/australia-oceania/australia-latest.osm.pbf \
    https://download.geofabrik.de/australia-oceania/new-zealand-latest.osm.pbf \
    https://download.geofabrik.de/europe/great-britain-latest.osm.pbf \
    https://download.geofabrik.de/north-america/canada-latest.osm.pbf \
    https://download.geofabrik.de/north-america/us-latest.osm.pbf; do curl -fSL -O "$u"; done'

# 2. Build combined reverse index (~3 h 15 min on M1 Max 64 GB).
./build/build-index data/index-worldwide data/pbf/*.osm.pbf

# 3. Build forward + autocomplete (~6 min).
./target/release/build-forward-index data/index-worldwide
./target/release/build-autocomplete-fst data/index-worldwide --layout both

# 4. Refresh per-country Pelias corpora (~2 s).
make pelias-full-refresh

# 5. Run the suite.
make regression-worldwide
```

Total end-to-end from clean clone: ~4–5 h assuming good Geofabrik
bandwidth (10–30 MB/s). On slow links (1 MB/s), add ~3 h of pure
download time.
