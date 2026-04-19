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
              ┌──────┐  ┌────────┐ ┌──────────────┐
              │tantivy│ │ gnaf_* │ │ oa_<cc>_*    │
              │  /    │ │ (AU)   │ │ (~60 ctries) │
              │tantivy│ └────────┘ └──────────────┘
              │_<cc>/ │
              └──────┘

                     ┌─────────────────────┐
                     │  query-server       │   Rust + axum + tantivy
                     │  (single binary)    │
                     └───────┬─────────────┘
                             │  mmap all files, serve HTTP on :3000
                             ▼
                   /reverse, /search, auth routes
```

Everything on the left of `query-server` is offline build work. The
server itself is stateless — give it a directory of binaries, it serves
queries.

## On-disk format

All data lives as memory-mapped flat binary files. We deliberately avoid
RocksDB / LMDB / SQLite / any embedded DB — every query is a pointer
arithmetic operation over pre-sorted arrays indexed by S2 cell.

### Files produced by the C++ `build-index`

| File | Record | Bytes / record | Purpose |
|---|---|---:|---|
| `addr_points.bin` | `AddrPoint{f32 lat, f32 lng, u32 housenumber_id, u32 street_id}` | 16 | OSM addr:housenumber points |
| `addr_entries.bin` | `u16 count, u32 ids...` | variable | Per-cell list of addr_point IDs |
| `street_ways.bin` | `WayHeader{u32 node_offset, u8 node_count, u32 name_id}` | 12 | OSM highways with names |
| `street_nodes.bin` | `NodeCoord{f32 lat, f32 lng}` | 8 | Street polyline nodes |
| `street_entries.bin` | `u16 count, u32 ids...` | variable | Per-cell street way IDs |
| `geo_cells.bin` | `u64 cell_id, u32 street_off, u32 addr_off, u32 interp_off` | 20 | Sorted merged S2 cell index for streets/addrs/interps |
| `admin_polygons.bin` | `AdminPolygon{vertex_offset, vertex_count, name_id, admin_level, area, country_code}` | 24 | `boundary=administrative`/`postal_code` polygons |
| `admin_vertices.bin` | `NodeCoord` | 8 | Polygon vertex pool (Douglas-Peucker simplified, max 500 verts) |
| `admin_cells.bin`, `admin_entries.bin` | S2 index | variable | Admin polygon cell lookup with INTERIOR_FLAG short-circuit |
| `place_points.bin` | `PlacePoint{lat, lng, name_id, rank}` | 16 | `place=city/town/village/suburb/hamlet` points |
| `place_cells.bin`, `place_entries.bin` | S2 index | variable | Place point cell lookup |
| `interp_ways.bin`, `interp_nodes.bin`, `interp_entries.bin` | Interpolation | variable | `addr:interpolation` ways |
| `strings.bin` | NUL-terminated UTF-8 | variable | Deduplicated string pool for every `*_id` above |

### Files produced by Rust builders

| File | Built by | Purpose |
|---|---|---|
| `tantivy/` or `tantivy_<cc>/` | `build-forward-index` | Forward search via tantivy (monolithic or per-country) |
| `gnaf_points.bin`, `gnaf_cells.bin`, `gnaf_entries.bin`, `gnaf_strings.bin` | `build-gnaf-index` | G-NAF-derived AU address points (16.4M records) |
| `oa_<cc>_*.bin` | `build-openaddresses-index` | Per-country OpenAddresses address points |
| `postcode_lookup.bin`, `postcode_lookup_strings.bin` | `build-postcode-lookup` | Suburb-modal postcode lookup (AU G-NAF) |

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
}

Forward {
  default: Option<FieldedIndex>                // Monolithic `tantivy/`
  per_country: HashMap<[u8;2], FieldedIndex>   // `tantivy_<cc>/`
}
```

### Reverse geocoding: `Index::query(lat, lng)`

```
1. find_admin(lat, lng)
     Walk 9 admin cells at admin_cell_level.
     For each candidate polygon: check INTERIOR_FLAG, else PIP in f64.
     Track best-by-admin-level by smallest area, respecting per-country
     max_area caps (catches AU pastoral stations tagged admin_level=9).

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
```

### Forward geocoding: `Forward::search_structured(q)`

```
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

6. For each top hit, if housenumber was parsed, refine via
     find_addr_point_in_country — routes through G-NAF first (AU),
     then OpenAddresses per-country, then OSM addr_points.

7. Each hit is enriched by calling Index::query at the resolved coord,
     giving the full normalised address object in the response.
```

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
    OpenAddresses batch (~30 GB compressed ZIP from openaddresses.io)
        │
        ▼
    build-openaddresses-index [--country us,fr,de] [--skip au]
        → oa_<cc>_*.bin per country

Forward index:
    build-forward-index /data/index [--partition-by-country]
        → tantivy/ (monolithic) or tantivy_<cc>/ per country
```

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
- **Storage**: 50 GB gp3 (AU index is ~1.2 GB with all enrichment;
  the rest is headroom for diffs / next build)
- **Cost**: ~$20/month AWS
- **Handles**: 5-20K QPS per core, far more than any single fleet

### Worldwide deployment

- **Instance**: `r6i.xlarge` or `r6id.xlarge` (32 GB RAM; NVMe
  preferred). Per-country indexes keep it manageable.
- **Storage**: 100 GB gp3 or instance-store NVMe. Planet OSM reverse
  + tantivy + OA is ~50-60 GB.
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
| Country partitioning | Per-country `tantivy_<cc>/` + `oa_<cc>_*.bin` | ISO-2 prefix in FST, ~250 country partitions |
| Open-source intent | Yes | "Plan to open source" their S2 bindings |

Both architectures rejected the same class of alternatives: no
Elasticsearch, no MongoDB, no PostGIS, no distributed search. Single
process, multi-threaded, S2 + Tantivy is apparently the right answer.

### Where Radar does more

| Thing | Radar | Ours |
|---|---|---|
| **FST fast-path** | Tiny in-memory FST caches "millions of happy paths in MBs", returns "order of magnitude faster than a Tantivy query". Serves 80% of their traffic. | No FST cache — every forward query goes through tantivy. |
| **ML-driven query understanding** | FastText for typo-tolerant n-gram embedding; LightGBM classifier routes queries by intent. | Heuristic AU-state-abbreviation parser + token canonicalisation. |
| **Two-tier query architecture** | "Fast tier" for speed/precision, "deep search tier" for recall. | Single Fallback ladder within one tantivy index. |
| **Data pipeline** | Apache Spark, versioned S3 assets, ingest+eval new sources "within a day". | Per-dataset CLI builders + shell scripts. |
| **Separate stores per domain** | Separate tantivy indexes + RocksDB stores for **Addresses / Regions / Places** (three independent data clients). | Single `strings.bin` pool; separate `*_points.bin` files but not separate tantivys. |
| **RocksDB-backed KV** | Point lookups over RocksDB for record retrieval. | mmap'd fixed-record arrays indexed by S2 cells. |
| **Production battle-testing** | 1B+ calls/day, 1K QPS/core measured. | 20K QPS/core measured on AU synthetic load, not yet run at production scale globally. |
| **Cost savings documented** | Replaced Mongo + Elasticsearch clusters, saved "high five-figures/month". | Greenfield — no legacy to replace. |

### Where we do more (or differently)

| Thing | Ours | Radar |
|---|---|---|
| **Multi-source address ladder** | G-NAF (AU direct) → OpenAddresses per-country → OSM addr:housenumber. Explicit source priority per country. | Single "addresses" index; aggregation is in their Spark preprocessing. |
| **Config-driven admin mapping** | `admin-mapping.json` inspired by Nominatim's `address-levels.json`. Per-country overrides (AU level 9 → city, NZ level 6 → city) with area caps. | Not described; probably table-driven internally but not detailed. |
| **Nominatim-JSON-compatible output** | `/reverse` and `/search` return the Nominatim response shape. Drop-in replacement for Nominatim clients. | Their own API shape. |
| **Zero-downtime reload pattern** | `ArcSwap<Arc<Index>>` + marker file polling. Index rebuild → atomic swap. | Not explicitly described; they mention "gradual migration" over a year for their own system cut-over. |
| **Simpler operational surface** | Single binary, no RocksDB tuning, no Spark cluster. Just mmap. | Single binary but RocksDB + Spark ingestion to operate. |

### Performance: apples vs oranges

Their numbers and ours aren't directly comparable because they're
solving different problems:

| | Ours (measured, AU) | Radar (published) |
|---|---|---|
| Reverse p50 | **20–60 µs** | <1 ms |
| Forward p50 | **19–70 µs** (structured, exact) | 50 ms (freeform, fuzzy, ML-disambiguated) |
| QPS/core | **~20 K** (structured) | ~1 K (full-pipeline, global) |
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

### What we'd adopt from their playbook

If this geocoder starts carrying production global traffic, the highest-
leverage borrowings from Radar:

1. **FST fast-path for top queries.** A tiny in-memory trie keyed on
   normalised-query-string → result-set. Serves the 80% "well-formed"
   queries without touching tantivy at all. ~1 day of work, major
   latency/throughput win on repeated queries.

2. **Per-domain isolation** (separate tantivy indexes for places vs.
   streets vs. addresses vs. regions). Smaller per-domain BM25 tables,
   cleaner scoring. Our single tantivy mixes place/street docs already
   with a `kind` fast field — separating would mean one more tantivy
   per country but better scoring.

3. **Spark-like batch pipeline**. Our per-builder tools work but don't
   compose. A DAG ingestion layer would let us "ingest and evaluate a
   new data source within a day" as they claim. Low priority until we
   have multiple data sources in flight.

4. **ML for query understanding**. Only pays off at global scale with
   truly freeform input. Not needed for Traccar's dispatch workload.

### What we already do better (or more explicitly)

1. **Simpler storage.** No RocksDB to tune. Our mmap'd flat format is
   smaller, faster for point lookups, and has no LSM compaction
   overhead. Tradeoff: we can't do range scans, prefix lookups on
   arbitrary text, or update-in-place. Fine for our workload.

2. **Explicit country-source priority.** G-NAF for AU, OA for other
   covered countries, OSM as floor. Each source is visible in the
   `find_addr_point` ladder. Their preprocessing collapses this into
   one opaque "addresses" store.

3. **Drop-in Nominatim compatibility.** Existing clients of
   nominatim.openstreetmap.org can point at us without changing
   their parsers. Radar's API is their own shape.

4. **Every enrichment is optional.** Missing `gnaf_*.bin`? Server
   starts. Missing `tantivy_<cc>/`? Search falls back. Missing
   `postcode_lookup.bin`? Postcode fields are just null. Lets operators
   pick their exact cost/coverage tradeoff per country.

## Summary

We've built a geocoder architecturally congruent with Radar's
production HorizonDB at ~100× their measured per-query latency for our
narrower problem (structured country-aware dispatch vs. full freeform
global search). We don't yet do ML-driven query understanding, FST
fast-paths, or million-QPS global scale — those are natural next
increments if the project grows that direction.

Both systems validate that Rust + S2 + tantivy + mmap is the current
best-in-class answer for geocoding infrastructure, and neither needed
PostGIS, Elasticsearch, or a distributed database to get there.
