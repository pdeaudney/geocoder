# Architecture

This document is the single reference for how the geocoder is built and
runs. It describes the data flow from OSM PBF → binary indexes → live
query service, the on-disk formats, the query paths, and the deployment
model.

## High-level picture

```
                     ┌───────────────────┐
  planet.osm.pbf ───▶│  build-index      │   C++, libosmium + S2
                     │  (C++)            │
                     └─────────┬─────────┘
                               │  writes 17 binary files
                               ▼
              ┌──────────────────────────────────┐
              │  data/index/*.bin                │
              │    geo_cells, addr_points,       │
              │    street_ways, street_nodes,    │
              │    admin_*, place_*, strings,    │
              │    interp_*, ...                 │
              └────┬─────────┬─────────┬─────────┘
                   │         │         │
    build-forward- │   build-gnaf-     │   build-openaddresses-
       index       │      index        │       index
                   │         │         │
                   ▼         ▼         ▼
              ┌───────┐ ┌────────┐ ┌──────────────┐
              │tantivy│ │ gnaf_* │ │ oa_<cc>_*    │
              │  /    │ │ (AU)   │ │ (~60 ctries) │
              │tantivy│ └────────┘ └──────────────┘
              │_<cc>/ │
              └───────┘

                     ┌─────────────────────┐
                     │  query-server       │   Rust + axum + tantivy
                     │  (single binary)    │   REST on :3000, gRPC on :3001
                     └───────┬─────────────┘
                             │  mmap all files, serve HTTP + gRPC
                             ▼
          /reverse  /search  /validate  /autocomplete  /geocode/ip
                   /healthz  /healthz/indexes
```

Everything on the left of `query-server` is offline build work. The
server itself is stateless — give it a directory of binaries, it serves
queries. The REST surface is unauthenticated; gate at the network layer
for internal-only deployments.

## On-disk format

All data lives as memory-mapped flat binary files. We deliberately avoid
RocksDB / LMDB / SQLite / any embedded DB — every query is a pointer
arithmetic operation over pre-sorted arrays indexed by S2 cell.

### Files produced by the C++ `build-index`

| File | Record | Bytes / record | Purpose |
|---|---|---:|---|
| `addr_points.bin` | `AddrPoint{lat, lng, housenumber_id, street_or_place_id, unit_id, floor_id, parent_place_id, flags}` | 32 | OSM addr:housenumber points (incl. addr:place addresses, addr:unit, addr:floor, tagged parent locality) |
| `addr_entries.bin` | `u16 count, u32 ids...` | variable | Per-cell list of addr_point IDs |
| `street_ways.bin` | `WayHeader{u32 node_offset, u8 node_count, u32 name_id}` | 12 | OSM highways with names |
| `street_nodes.bin` | `NodeCoord{f32 lat, f32 lng}` | 8 | Street polyline nodes |
| `street_entries.bin` | `u16 count, u32 ids...` | variable | Per-cell street way IDs |
| `geo_cells.bin` | `u64 cell_id, u32 street_off, u32 addr_off, u32 interp_off` | 20 | Sorted merged S2 cell index for streets/addrs/interps |
| `admin_polygons.bin` | `AdminPolygon{vertex_offset, vertex_count, name_id, admin_level, importance, area, country_code}` | 24 | `boundary=administrative`/`postal_code` polygons; `importance` is the same 0..255 prominence score (population log + wikidata + wikipedia) `PlacePoint`/`PoiPoint` carry — admin docs use it for same-name disambiguation |
| `admin_vertices.bin` | `NodeCoord` | 8 | Polygon vertex pool (Douglas-Peucker simplified; cap is 500–32 000 verts depending on admin_level + country area) |
| `admin_cells.bin`, `admin_entries.bin` | S2 index | variable | Admin polygon cell lookup with INTERIOR_FLAG short-circuit |
| `place_points.bin` | `PlacePoint{lat, lng, name_id, rank, importance}` | 16 | `place=city/town/village/suburb/hamlet/neighbourhood/quarter/locality/island/islet/isolated_dwelling/farm` points; `importance` 0..255 |
| `place_cells.bin`, `place_entries.bin` | S2 index | variable | Place point cell lookup |
| `poi_points.bin` | `PoiPoint{lat, lng, name_id, category_id, rank, importance, parent_place_id}` | 24 | Named amenity/shop/tourism/aeroway/historic/leisure/office/healthcare/military/man_made/railway-non-track/natural-subset/waterway-subset POIs; `importance` 0..255 |
| `poi_cells.bin`, `poi_entries.bin` | S2 index | variable | POI cell lookup |
| `interp_ways.bin`, `interp_nodes.bin`, `interp_entries.bin` | Interpolation | variable | `addr:interpolation` ways |
| `strings.bin` | NUL-terminated UTF-8 | variable | Deduplicated string pool for every `*_id` above |

**`AddrPoint.flags`**: bit 0 = `FLAG_ADDR_PLACE` (street_or_place_id holds a place name from `addr:place`, not a street); bit 1 = `FLAG_IS_HOUSENAME` (housenumber_id holds a free-form `addr:full`/`addr:housename` string, not a numeric housenumber).

### Files produced by Rust builders

| File | Built by | Purpose |
|---|---|---|
| `tantivy/` or `tantivy_<cc>/` | `build-forward-index` | Forward search via tantivy (monolithic or per-country) |
| `fst_<cc>.fst`, `fst_<cc>.bin`, `fst_<cc>_strings.bin` | `build-autocomplete-fst` | Per-country FST for `/autocomplete` + `/search` fast-path (400 ns exact-match lookups) |
| `gnaf_points.bin`, `gnaf_cells.bin`, `gnaf_entries.bin`, `gnaf_strings.bin` | `build-gnaf-index` | G-NAF-derived AU address points (16.4M records) |
| `oa_<cc>_*.bin` | `build-openaddresses-index` | Per-country OpenAddresses address points |
| `postcode_lookup.bin`, `postcode_lookup_strings.bin` | `build-postcode-lookup` | Suburb-modal postcode lookup (AU G-NAF) |
| `i18n_names.bin` | emitted by `build-index` from alias-family OSM tags (`name:<lang>`, `official_name`, `alt_name`, `short_name`, `old_name`, `loc_name`, `int_name`, `reg_name`, `ref`, `int_ref`, `nat_ref`, plus per-language variants) | Localised / alternate names keyed on `(entity_type, entity_id, alias_type, lang_code)`; used by `/reverse?lang=...` and forward-index alternates expansion |
| `wof_countries.bin`, `wof_countries_vertices.bin`, `wof_countries_strings.bin` | `wof-importer` (tools/) | Who's on First country polygons. Fallback for `find_admin` when an extract lacks the OSM `admin_level=2` relation (common on per-country Geofabrik extracts). |

**Not on disk**: H3 cell IDs are computed at query time from the response
coord via the `h3o` crate when the caller passes `h3_res=...`. No file
is produced, so no rebuild path to worry about.

### Spatial indexing

All geometry lookups go through **S2 cells** (Google's Hilbert-curve
based sphere indexing). Two levels:

- **street_cell_level = 17** (~80 m per cell) for streets, addresses,
  place-points, G-NAF and OpenAddresses address points.
- **admin_cell_level = 10** (~10 km per cell) for admin polygons.

Queries compute a leaf cell from (lat, lng), take the 9-cell
neighbourhood (centre + 8 neighbours), binary-search the sorted
`*_cells.bin` to find the entry offset for each, then iterate the
`*_entries.bin` IDs for that cell. Each hit is an index into the
corresponding `*_points.bin` / `*_ways.bin` flat array.

Admin polygons carry an `INTERIOR_FLAG` when the cell is entirely inside
the polygon, letting us skip the point-in-polygon test for the common
deep-inside case.

### Record → string pool

`name_id`, `housenumber_id`, `postcode_id` etc. are offsets into
`strings.bin` (or the parallel G-NAF / OA string pools). Offset 0 is
reserved for an empty string so unset IDs resolve to `""`.

## Runtime components

```
Index {
  // OSM (always present; built from planet.osm.pbf)
  geo_cells, street_*, addr_*, admin_*, place_*, interp_*, strings: Mmap

  // Runtime settings
  street_cell_level, admin_cell_level, max_distance_sq
  admin_config: AdminConfig        // Nominatim-style (country, level) → field

  // Optional enrichment (loaded lazily if files present)
  postcode_lookup: Option<PostcodeLookup>     // G-NAF suburb-modal
  gnaf: Option<Gnaf>                          // G-NAF AU authoritative
  open_addresses: Option<OpenAddresses>        // Per-country OA
  i18n_names: Option<I18nNames>                // name:<lang> OSM tag lookup
  wof_countries: Option<WofCountries>          // Country-level polygon fallback
}

Forward {
  default: Option<FieldedIndex>                // Monolithic `tantivy/`
  per_country: HashMap<[u8;2], FieldedIndex>   // `tantivy_<cc>/`
}

Autocomplete {
  per_country: HashMap<[u8;2], AutocompleteCountry>  // fst_<cc>.*
}
// Exposed via main.rs Extension<Option<Arc<Autocomplete>>>; used by
// both /autocomplete (prefix walk) and /search (exact-match fast-path).
```

### Reverse geocoding: `Index::query(lat, lng)` / `query_with_lang(lat, lng, lang)`

```
1. find_admin(lat, lng)
     Walk 9 admin cells at admin_cell_level.
     For each candidate polygon: check INTERIOR_FLAG, else PIP in f64.
     Track best-by-admin-level by smallest area, respecting per-country
     max_area caps (catches AU pastoral stations tagged admin_level=9).
     If no country-level (admin_level=2) polygon matched and
     wof_countries.bin is loaded, consult the WoF country index to
     back-fill country_code. This covers per-country Geofabrik extracts
     that don't include the country relation (GB, US, etc.).

2. Map admin_level → output field per country (AdminConfig lookup).
     e.g. AU: 6 → county, 9 → city, 11 → postcode.

3. If admin.city is None, find_place(lat, lng) — nearest place=* point,
     rank-aware (city > suburb > hamlet).

4. query_geo(lat, lng)
     Walk 9 street cells at street_cell_level.
     Collect nearest addr_point, nearest street_way, best interpolation way.
     Return whichever has the smallest squared-distance under max_distance_sq.

5. Postcode fallback ladder (AU-gated):
     G-NAF find_nearest → OpenAddresses find_nearest → suburb-modal lookup.

6. Compose Address with country-specific format rules (format_rules):
     US/AU/NZ etc.: "<house> <street>, <city>, <state> <postcode>, <country>"
     DE/FR etc.:    "<street> <house>, <postcode> <city>, <country>"

7. (query_with_lang only) If `lang=` was passed and i18n_names.bin is
     loaded, binary-search (entity_type, poly_id, lang_code) for each
     admin field that carries a poly_id. Override the default name with
     the OSM name:<lang> tag when present, then re-render display_name.

8. Stamp `confidence`: `exact` (addr_point hit), `interpolated` (via
     addr:interpolation way), or `fallback` (street centroid / admin-only).
```

### Forward geocoding: HTTP `/search` handler + `Forward::search_structured(q)`

```
0. (HTTP layer, FST fast-path) If the query is simple freeform
     (no street/city/state set, single country, query length ≥ 2)
     and the country's FST is loaded, probe it for an exact-key match.
     On hit: enrich via reverse geocode at the FST coord, return.
     Response carries `"source": "fst"`. Median latency: ~400 ns for
     the FST lookup + ~40 µs for the reverse-geocode enrichment.

1. Parse freeform `q` if present:
     extract house_number (leading digits),
     state abbreviation (NSW/VIC/...),
     postcode (trailing 4 digits),
     remaining tokens → bag for name/suburb matching.

2. Dispatch to a FieldedIndex:
     q.country_code set and per_country[cc] loaded → that index
     else default (monolithic) if loaded
     else no-op (501).

3. Compose tantivy BooleanQuery:
     Structured street/city/state tokens → Must on their field
     Freeform tokens → Must(Boolean Should across name(×3 boost) + suburb)
     country_code filter → Must (indexed field, always applied)
     kind filter → Must on fast field

4. Search top 3× limit, re-rank by BM25 × rank-prominence factor
     (rank 16 place ~1.4×, rank 19 suburb ~1.15×, rank 26 street 1.0×).

5. Fallback ladder if 0 hits: drop country_code → state → city → kind,
     retry each drop. Re-dispatch in case the fallback crosses countries.

6. Last-resort fuzzy fallback: retry with FuzzyTermQuery (Levenshtein
     distance 1) on the name field. Fires only if the strict ladder
     produced nothing — typical typo cost is ~150 µs vs ~40 µs strict.

7. For each top hit, if housenumber was parsed, refine via
     find_addr_point_in_country — routes through G-NAF first (AU),
     then OpenAddresses per-country, then OSM addr_points.

8. Each hit is enriched by calling Index::query at the resolved coord,
     giving the full normalised address object in the response.
```

### Response enrichment: H3 cells

Every endpoint that returns a coordinate accepts an optional `h3_res=`
query parameter — a comma-separated list of Uber H3 resolutions
(0–15, max 4). When present, the handler converts the response coord
into an `h3` map keyed by resolution, computed on the fly via `h3o`:

```
/reverse → Address.h3         (uses the caller's query coord)
/search  → hit.h3 per result  (uses the refined post-house-number coord)
/validate → response.h3       (uses final_lat/final_lng, post-refinement)
/autocomplete → hit.h3 per result
/geocode/ip → response.h3     (uses the MaxMind lookup coord)
```

Cells are 15-char lowercase hex strings (not u64 — JSON/JS can't carry
the full 64-bit value cleanly). No field is emitted when `h3_res` is
absent; the serde skip keeps the response byte-identical for callers
that don't opt in. The gRPC proto mirrors the REST shape via
`repeated uint32 h3_res` on requests and `map<uint32, string> h3` on
responses.

### Tokenization / normalisation (build-side + query-side, symmetric)

Both index-time and query-time text passes through the same pipeline:

1. **ASCII fold** — maps common Latin Extended characters to ASCII
   (`Zürich` ≡ `Zurich`, `São` ≡ `Sao`, `ñ → n`, etc.).
2. **Tantivy `SimpleTokenizer + AsciiFoldingFilter + LowerCaser`** at
   tantivy-level for the forward index.
3. **Abbreviation canonicalisation** — a conservative table of
   unambiguous street-type abbreviations (`Hwy→Highway`, `Tce→Terrace`,
   `Pde→Parade`, etc.). Applied to both index and query so "Pacific Hwy"
   matches "Pacific Highway". `St` and `Ct` deliberately skipped (St =
   Saint vs Street is ambiguous).

## Build pipeline

```
planet.osm.pbf
    │   (~75 GB for planet, ~1 GB for AU)
    │
    │   osmium apply-changes with daily diffs (update-index.sh)
    │
    ▼
build-index /data/index /data/pbf/*.osm.pbf
    │   ~45 min for planet, ~3 min for AU
    │   Produces the 17 OSM-derived .bin files (~20 GB planet)
    ▼

Optional enrichment (AU-specific):
    G-NAF PSV (1.7 GB zip from data.gov.au)
        │
        ▼
    build-gnaf-index → gnaf_*.bin (488 MB for AU)
        │
        │   Also feeds:
        ▼
    build-postcode-lookup → postcode_lookup.bin (244 KB)

Optional enrichment (worldwide):
    OpenAddresses batch (~66 GB compressed from openaddresses.io S3)
        │
        ▼
    build-openaddresses-index [--country us,fr,de] [--skip au]
        → oa_<cc>_*.bin per country

    Who's on First admin SQLite (~8.6 GB planet, or per-country extracts)
        │
        ▼
    wof-importer (tools/) → wof_countries*.bin
        Country-level polygon fallback for extracts missing
        OSM admin_level=2 (Great Britain, USA, per-country Geofabrik).

Forward index:
    build-forward-index /data/index [--partition-by-country]
        → tantivy/ (monolithic) or tantivy_<cc>/ per country

Autocomplete FST:
    build-autocomplete-fst /data/index
        → fst_<cc>.fst + fst_<cc>.bin + fst_<cc>_strings.bin per country,
          plus fst_unified.* when built with --layout both.
        Served by /autocomplete and used as the /search fast-path.
```

### What the C++ indexer includes and excludes

Filters applied during `build-index`'s OSM pass. Values are currently
hard-coded in `builder/src/build_index.cpp`; see "Customising the
indexer" below for the rationale and the pattern we'd use if we made
them config-driven.

**Streets** — `highway=*` ways with an explicit `name=` tag.

| Category | Included | Excluded |
|---|---|---|
| Motor roads | `motorway`, `trunk`, `primary`, `secondary`, `tertiary`, `unclassified`, `residential`, `living_street`, `motorway_link`/`trunk_link`/etc. | — |
| Named pedestrian ways | `pedestrian` (plazas, shopping streets — named only) | — |
| Trails & service | — | `footway`, `path`, `track`, `steps`, `cycleway`, `bridleway`, `service` |
| Transient | — | `construction` |

Unnamed ways are rejected universally regardless of type (the indexer
gates on `if (name)` before classification). `highway=pedestrian`
was moved from the exclusion list to the inclusion list in April 2026
so that named plazas like Sydney's Martin Place, Melbourne's Bourke
Street Mall, and most European old-town lanes become addressable.

**Places** — `place=*` nodes or named areas with `place=*` on a closed
way. Mapped to a Nominatim-compatible `rank` and included only if the
tag is one of:

| `place=` | Rank | Intent |
|---|---|---|
| `city` | 16 | Major population centre |
| `town` | 16 | |
| `village` | 16 | |
| `suburb` | 19 | Named neighbourhoods inside a city |
| `hamlet` | 20 | Small settlements |

All other `place=*` values (`farm`, `island`, `isolated_dwelling`,
`locality`, `quarter`, etc.) are dropped. They carry too much noise
for typeahead and address display — e.g. `place=farm` nodes often
share names with roads nearby, and `locality` is used inconsistently
across mappers.

**Admin polygons** — `boundary=administrative` at
`admin_level=2..=10`, plus `boundary=postal_code` treated as
`admin_level=11` so a postcode polygon can back-fill the postcode
field when no OSM postal boundary is available.

| `admin_level` | Typical use |
|---|---|
| 2 | Country |
| 3 | State grouping (rarely used) |
| 4 | State / province |
| 6 | County / local government area |
| 8 | City / town boundary |
| 9 | Suburb (AU) — honoured because OSM's AU taxonomy uses 9 not 10 for suburbs |
| 10 | Suburb (most other countries) |

Levels outside 2–10 are skipped because they carry almost no
geocoding signal and inflate the polygon index.

**Addresses** — every node with `addr:housenumber` + `addr:street`,
plus buildings (closed ways) carrying the same tags; centroids are
emitted for closed-way buildings. No filter by type; we take what
OSM has.

**Interpolation** — `addr:interpolation` ways with resolvable
endpoints (`addr:housenumber` tagged on the end-nodes). Unresolvable
interpolations are silently dropped.

**i18n names** — every admin polygon and named place also emits
`name:<lang>` pairs (e.g. `name:ja`, `name:zh`) into `i18n_names.bin`.
No filter; all languages OSM provides are kept.

### Customising the indexer

Today the filters above are hard-coded in C++. That's intentional: the
defaults are a considered baseline ("Nominatim minus the obviously
unhelpful") and most deployments never need to deviate.

The pragmatic extension point when someone actually needs custom
filters is a JSON config file next to `admin-mapping.json`, consumed
by both the C++ builder and the runtime. Shape we'd land on:

```json
{
  "included_highways": ["motorway", "trunk", "primary", "pedestrian", ...],
  "excluded_highways": ["footway", "path", ...],
  "place_ranks": { "city": 16, "suburb": 19, "custom_type": 18 },
  "admin_levels": [2, 4, 6, 8, 9, 10]
}
```

Not shipped because no caller has asked for it yet. The trigger to
revisit: concrete failures in the regression corpora we're bringing
online (Pelias acceptance-tests, translated Nominatim BDD scenarios,
OpenAddresses round-trip) that point to tag filters as the root cause —
e.g. a Pelias case expects `place=locality` to resolve in a region
where it's the local convention for suburb, or an OpenAddresses
round-trip misses a country because we drop `highway=service` that
carries the address there.

When that happens, a small PR adds a config parser, hands the
resulting arrays to the existing `is_included_highway` / `place_rank` /
admin filter call sites, and ships an `indexer-config.json` alongside
the PBF. Until then the hard-coded defaults are simpler to reason
about and produce byte-identical indexes across rebuilds.

## Update model

Zero-downtime index reloads via `ArcSwap<Arc<Index>>` + a marker file:

```
update-index.sh (cron):
  1. osmium apply-changes planet.osm.pbf diffs.osc → planet-new.osm.pbf
  2. build-index /data/index-next /data/planet-new.osm.pbf
  3. [optionally rebuild forward / G-NAF / OA indexes]
  4. mv /data/index /data/index-old && mv /data/index-next /data/index
  5. touch /data/index/.reload

query-server watches the marker, re-mmaps on mtime change.
```

Concurrent queries keep holding the old Arc until finished; new queries
see the new Index. Dropped refcount frees the old mmaps.

## Deployment model

### Single-country deployment (e.g. AU)

- **Instance**: `t4g.medium` or similar (4 GB RAM, 1-2 vCPU)
- **Storage**: 50 GB gp3 (AU index is ~1.4 GB with G-NAF + postcode +
  FST + tantivy; the rest is headroom for diffs / next build)
- **Cost**: ~$20/month AWS
- **Handles**: 5-20K QPS per core, far more than any single fleet

### Worldwide deployment

- **Instance**: `r6i.xlarge` or `r6id.xlarge` (32 GB RAM; NVMe
  preferred). Per-country indexes keep it manageable.
- **Storage**: 100 GB gp3 or instance-store NVMe. Planet index total
  (OSM reverse + per-country tantivy + FST + OpenAddresses + WoF
  countries + i18n) runs ~28–33 GB; source data during the build is
  far larger and belongs on scratch disk.
- **Cost**: ~$200/month AWS at single-instance steady state
- **Handles**: 5-10K QPS per instance; scale out behind a load balancer

### Updates

- **AU-only / small-region**: rebuild weekly, OS cron → `update-index.sh`
- **Worldwide**: rebuild on an ingest instance, upload to S3,
  query instances sync via `aws s3 sync` on a marker, ArcSwap picks up

## How this compares to Radar's HorizonDB

Radar published two posts
([1](https://radar.com/blog/high-performance-geocoding-in-rust),
[2](https://radar.com/blog/building-horizondb-in-production)) describing
their in-house geocoder that powers 1B+ API calls/day. Their
architecture is the closest analog to ours in public; this section
compares shape-by-shape.

### Shared choices (where they validate our approach)

| | Ours | Radar HorizonDB |
|---|---|---|
| Language | Rust | Rust |
| Process model | Single binary, multi-threaded | Single binary, multi-threaded |
| Spatial index | Google S2 (Rust bindings for s2 crate) | Google S2 (Rust bindings, plan to open-source) |
| Forward text search | Tantivy | Tantivy |
| Storage | mmap'd flat binaries | RocksDB (LSM) + mmap |
| Country partitioning | Per-country `tantivy_<cc>/` + `oa_<cc>_*.bin` + `fst_<cc>.*` | ISO-2 prefix in FST, ~250 country partitions |
| FST fast-path for common queries | `/autocomplete` + `/search` exact-match, 400 ns | "Serves 80% of traffic, order-of-magnitude faster than tantivy" |
| Fuzzy fallback | FuzzyTermQuery (Levenshtein-1) on zero hits | FastText n-gram embeddings + Levenshtein |
| Open-source intent | Yes | "Plan to open source" their S2 bindings |

Both architectures rejected the same class of alternatives: no
Elasticsearch, no MongoDB, no PostGIS, no distributed search. Single
process, multi-threaded, S2 + Tantivy is apparently the right answer.

### Where Radar does more

| Thing | Radar | Ours |
|---|---|---|
| **ML-driven query understanding** | FastText n-gram embeddings for typo tolerance; LightGBM classifier routes queries by intent. | FuzzyTermQuery (Levenshtein 1) as last-resort fallback. No ML intent classification. |
| **Data pipeline** | Apache Spark, versioned S3 assets, ingest+eval new sources "within a day". | Per-dataset CLI builders + shell scripts. |
| **Separate stores per domain** | Separate tantivy indexes + RocksDB stores for **Addresses / Regions / Places**. | Single tantivy with a `kind` fast field. Address-point, admin, and place indexes are already separate bin files, but forward search is one index. |
| **RocksDB-backed KV** | Point lookups over RocksDB for record retrieval. | mmap'd fixed-record arrays indexed by S2 cells. |
| **Custom fst::Automaton for country prefix pruning** | One FST per data type, keys prefixed with 2-byte ISO code. | Per-country FST files — equivalent behaviour, different file layout. Unified FST is a low-priority structural cleanup. |
| **u64-bitmap fast fields for dense numerics** | Custom Collector intersects query bitmask with per-hit bitmap pre-BM25 for street numbers. | Not yet — we dedup housenumber strings in tantivy, which is less efficient but works. |
| **Production battle-testing** | 1B+ calls/day, 1K QPS/core measured. | 20K QPS/core measured on AU synthetic load, not yet run at production scale globally. |
| **api-diff regression harness** | Open-sourced `@radarlabs/api-diff` — CSV-driven regression tool used to shadow HorizonDB traffic for a year. | Not built. |
| **Kinesis → S3 + Athena partition-projection** | Telemetry pipeline with nightly Airflow repartitioning. >1000× bytes-read reduction. | Not built — no query-log pipeline yet. |
| **CDKTF-driven blue-green deployment** | Each index release is a versioned S3 asset; new ASG reads it, ALB weighted-shifts traffic. | Our ArcSwap + marker-file pattern covers single-instance hot-reload; multi-instance deployment pattern not documented. |

### Where we do more (or differently)

| Thing | Ours | Radar |
|---|---|---|
| **Multi-source address ladder** | G-NAF (AU direct) → OpenAddresses per-country → OSM addr:housenumber. Explicit source priority per country. | Single "addresses" index; aggregation is in their Spark preprocessing. |
| **Config-driven admin mapping** | `admin-mapping.json` inspired by Nominatim's `address-levels.json`. Per-country overrides (AU level 9 → city, NZ level 6 → city) with area caps. | Not described; probably table-driven internally but not detailed. |
| **Nominatim-JSON-compatible output** | `/reverse` and `/search` return the Nominatim response shape. Drop-in replacement for Nominatim clients. | Their own API shape. |
| **Zero-downtime reload pattern** | `ArcSwap<Arc<Index>>` + marker file polling. Index rebuild → atomic swap. | Not explicitly described; they mention "gradual migration" over a year for their own system cut-over. |
| **Simpler operational surface** | Single binary, no RocksDB tuning, no Spark cluster. Just mmap. | Single binary but RocksDB + Spark ingestion to operate. |
| **Built-in i18n** | `/reverse?lang=zh` returns localised admin names from OSM `name:<lang>` tags via `i18n_names.bin`. | Radar's public docs don't specify an i18n mechanism for returned names. |
| **gRPC API with full feature parity** | `geocoder.proto` mirrors every REST endpoint, including H3 enrichment. Typed clients, lower serialisation cost. | REST only. |
| **H3 cell enrichment** | Opt-in `h3_res=` parameter stamps Uber H3 cell IDs on any returned coord, up to 4 resolutions in one call. Computed query-time via `h3o`; no on-disk footprint. Lets Kepler.gl / DuckDB / Databricks / Snowflake consumers join directly against H3-indexed data. | Not publicly exposed on Radar's API. |
| **WoF country fallback** | `wof_countries.bin` fills in `country_code` when an OSM extract is missing the `admin_level=2` relation (per-country Geofabrik). | Uses reverse-geocodable polygons end-to-end; not documented as a separate fallback path. |
| **Every enrichment optional** | Missing `gnaf_*.bin` / `fst_*.fst` / `i18n_names.bin` / `postcode_lookup.bin` / `wof_countries.bin` → server starts, the corresponding feature returns 501 or degrades silently. Per-country deployments only mount what they need. | Not documented. |

### Performance: apples vs oranges

Their numbers and ours aren't directly comparable because they're
solving different problems:

| | Ours (measured, AU) | Radar (published) |
|---|---|---|
| Reverse p50 | **20–60 µs** | <1 ms |
| Reverse with `lang=` | 80–100 µs | — |
| Forward p50, FST fast-path hit | **0.4 µs FST + ~40 µs enrich** | — |
| Forward p50, tantivy | **19–70 µs** (structured) | 50 ms (freeform, fuzzy, ML-disambiguated) |
| Forward, fuzzy fallback | ~150 µs | included in their 50 ms |
| Autocomplete p50 | **~7 µs** | included in forward figures |
| QPS/core | **~20 K** (structured) | **2 K** (full-pipeline, global — they publish "2000 rps/core" as a headline number) |
| Scale tested | AU only, ~16 M addresses | Global, 1B+ calls/day |

- Our latency is dominated by S2 cell lookups + polygon tests. We don't
  pay for fuzzy tokenisation, ML inference, or RocksDB round-trips.
- Radar's 50 ms forward is the full stack: typo tolerance, semantic
  understanding, query disambiguation, fuzzy matching. For a
  "user-typing-Elizabeth-St" search that's the right latency; for
  Traccar's known-country dispatch input it's overkill.
- Radar's 1K QPS/core is with 1B calls/day of truly diverse queries;
  our 20K QPS/core is uniform AU reverse — we haven't load-tested
  global freeform.

For the per-commit detail of how these numbers got there — the
specific allocation removals, double-call dedupes, and
cache-locality wins, with criterion and k6 before/after for each
— see [`docs/performance/`](docs/performance/). The tip-of-tree
read-path snapshot is
[`readpath-optimisations-2026-04-25.md`](docs/performance/readpath-optimisations-2026-04-25.md).

### What we'd still adopt if the scope grew

1. **Unified FST with country-prefix automaton** — Radar's documented
   trick: one FST with keys prefixed by 2-byte ISO code, custom
   `fst::Automaton` peels the prefix. Cleaner file layout than our
   per-country FSTs, same pruning behaviour. Low priority.
2. **Tantivy u64-bitmap fast fields + custom `Collector`** for
   housenumber filtering — pre-BM25 bitmap intersection instead of
   string-term matching. Better when house-number ranges matter.
3. **Per-domain isolation** (separate tantivy indexes for places vs
   streets vs addresses vs regions). Our single tantivy + `kind` fast
   field works; per-domain would give tighter BM25 IDF. Structural.
4. **ML intent classification** (LightGBM). Radar uses it to route
   query shapes to the right index; we use a heuristic classifier
   (is-this-query-structured?) which is sufficient for dispatch
   workloads.
5. **Apache Spark DAG ingestion**. Our per-source CLIs don't compose.
   An Airflow/Spark DAG would let us "ingest and evaluate a new data
   source within a day" the way Radar claims. Pay-off scales with
   number of data sources.
6. **api-diff regression harness** (they open-sourced
   `@radarlabs/api-diff`). Would gate ranking changes without shipping
   regressions. Easy to port; not yet built.
7. **Kinesis → S3 + Athena partition-projection telemetry**. Once we
   have query traffic worth sampling, this pattern keeps analytics
   costs near-zero.
8. **DuckDB as index-debug workbench**. Dump the binary indexes as
   Parquet for ad-hoc investigation ("why did this address rank
   there?").
9. **CDKTF + blue-green deployment** with versioned S3 index artifacts.
   Our ArcSwap reload handles single-instance; fleet-level rollouts
   want the Radar pattern.

### Areas Radar hasn't publicly documented

Worth flagging as both our risk areas and opportunities to differentiate
through our own documentation:

- **Observability / SLOs** — we don't expose per-endpoint metrics yet;
  they don't publish what they measure either.
- **Polygon simplification strategies** — our Douglas-Peucker at 500
  vertices works; they haven't documented their approach.
- **Timezone lookups** — classic "given a coord, what's the TZ"
  service. Often co-located with geocoding. Not shipped either side.
- **Address deduplication algorithms** — Radar mentions it matters;
  neither of us has published a technique.
- **Bloom filter / column-family tuning** for RocksDB (theirs) —
  irrelevant to us because we don't use RocksDB.

## Summary

Rust + S2 + tantivy + mmap has turned out to be the right answer for
geocoding infrastructure on both sides of this comparison — neither
system needed PostGIS, Elasticsearch, or a distributed database to
get there. Our measured latency sits 100–1000× below Radar's published
numbers for structured-dispatch workloads; the freeform / ML-ranked
case is where they still do more, and that backlog is catalogued above.
Beyond the Radar playbook we ship H3 cell enrichment, a gRPC mirror of
every REST endpoint, drop-in Nominatim JSON compatibility, and
optional-enrichment semantics that let operators pick their exact
cost / coverage tradeoff per country.
