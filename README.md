# Geocoder

A self-hosted, single-binary geocoding service in Rust. Reverse geocoding, structured + freeform forward search, typeahead autocomplete, address validation, and IP geolocation — all over HTTP/REST and gRPC, with sub-millisecond latency on commodity hardware.

Indexes OpenStreetMap data alongside authoritative per-country sources (G-NAF for Australia, OpenAddresses.io for ~60 other countries) into mmap-friendly binary files. No database, no search cluster, no queue — the server is one process, one directory of `.bin` files, one port.

> **Fork notice.** This project was forked from [traccar/traccar-geocoder](https://github.com/traccar/traccar-geocoder) and has diverged significantly in scope and capability. Bug fixes from this fork are periodically contributed back upstream; scope additions (forward geocoding, G-NAF ingestion, FST fast-path, per-country partitioning, gRPC, i18n, etc.) stay here.

## What it gives you

| | |
|---|---|
| **Reverse geocode** | coordinate → full address with country-aware admin mapping |
| **Forward search** | text or structured fields → ranked candidate list, Nominatim-compatible JSON |
| **Autocomplete** | FST-backed prefix typeahead, ~400 ns per exact key match |
| **Address validation** | structured fields → verified status + canonical normalised address |
| **IP geocode** | requester IP → coordinate (optional MaxMind GeoLite2) |
| **Multi-language** | OSM `name:<lang>` translations honoured via `lang=` parameter |
| **H3 cell enrichment** | opt-in `h3_res=` stamps [Uber H3](https://h3geo.org/) cell IDs on any returned coord, up to 4 resolutions per call |
| **Authoritative country data** | G-NAF (AU) and OpenAddresses.io (~60 countries) drop in as optional enrichment |
| **Hot reload** | index rebuilds swap atomically via `ArcSwap`; queries don't drop |
| **Zero external deps at runtime** | one binary, one data directory, optional MaxMind file |

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full technical reference: on-disk format, query paths, data pipeline, deployment model, and a detailed comparison to Radar's public HorizonDB architecture.

## Quick start

### Docker

```bash
# All-in-one: download, build index, and serve
docker run -e REGION=oceania \
  -v geocoder-data:/data -p 3000:3000 geocoder:latest
```

The `auto` mode (default) downloads the PBF for a named region, builds the reverse + forward indexes, and starts serving.

Supported region presets: `oceania` (default; full Australia/Oceania continent — AU, NZ, Fiji, PNG, Vanuatu, Solomon Is, New Caledonia, Cook Is, Samoa, Tonga, Kiribati, etc.), `australia` and `new-zealand` (sub-region extracts), `africa`, `antarctica`, `asia`, `europe`, `north-america`, `south-america`, `central-america`, `russia`, `usa`, `planet`.

```yaml
# docker-compose.yml
services:
  geocoder:
    image: geocoder:latest
    environment:
      - REGION=australia
    ports:
      - "3000:3000"   # REST
      - "3001:3001"   # gRPC
    volumes:
      - geocoder-data:/data

volumes:
  geocoder-data:
```

`PBF_URLS="https://... https://..."` as an alternative to `REGION`; pass any PBF URL(s) and the builder will use them instead.

Custom region? Build the `fetch-data` binary and call it directly:

```bash
cargo build --release --manifest-path server/Cargo.toml --bin fetch-data
./server/target/release/fetch-data --region europe --data-dir ./data
./server/target/release/fetch-data --region north-america --data-dir ./my-data
```

### Build from source

Prerequisites: a C++17 compiler + CMake for the builder, Rust stable for the server, `protoc` for gRPC. The C++ builder also benefits from `libdeflate` for fast PBF inflate (libosmium picks it up automatically when present; falls back to zlib otherwise — see `docs/performance/build-pipeline-perf-plan.md`).

On macOS:

```bash
brew install cmake libosmium protozero s2geometry protobuf libdeflate lbzip2
```

On Debian/Ubuntu:

```bash
apt-get install cmake libosmium2-dev libprotozero-dev libs2-dev \
                zlib1g-dev libbz2-dev libexpat1-dev liblz4-dev \
                libdeflate-dev \
                protobuf-compiler \
                lbzip2
```

`lbzip2` is the parallel bzip2 decoder `fetch-data` shells out to when
unpacking the WhosOnFirst SQLite archive — 3–5× faster than stock
`bzip2`. Optional but cuts ~5 minutes off a planet build's fetch step.
The binary falls back to `pbzip2` then `bzip2` if `lbzip2` isn't
installed.

Fetch the source data with the `fetch-data` binary
(`server/src/bin/fetch_data.rs`):

```bash
# Build the binary once.
cargo build --release --manifest-path server/Cargo.toml --bin fetch-data

# All-in-one fetch — OSM PBF + WhosOnFirst (+ optional OpenAddresses, MaxMind, G-NAF).
# Defaults output to ./data/. Conditional GET + resumable downloads:
# re-runs are bandwidth-cheap (304 short-circuit) and a killed run resumes
# from the .partial sidecar on the next invocation.
./server/target/release/fetch-data --region au --wof          # AU-only
./server/target/release/fetch-data --region oceania --wof     # full Australia/Oceania
./server/target/release/fetch-data --region europe --wof      # EU
./server/target/release/fetch-data --region planet --wof      # planet ~85 GB
./server/target/release/fetch-data --region all-continents --wof  # planet via 9 parallel continent extracts (recommended)
```

Each PBF lands at `data/pbf/<region>-latest.osm.pbf` with three
sidecars next to it:

  - `<file>.etag` — captured from the response's `ETag` header so the
    next run can send `If-None-Match` and short-circuit on 304.
  - `<file>.partial` — in-flight download (atomically renamed to the
    final filename on success; survives a killed process for resume).
  - `<file>.state.txt` — Osmosis-format replication state
    (`timestamp=…` / `sequenceNumber=…`) that the rest of the OSM
    ecosystem (`pyosmium-get-changes`, `osmupdate`, our own
    `update-index.sh`) consumes.

Mix and match sources by combining flags:

```bash
# Add OpenAddresses (Requester-Pays, needs AWS creds) and MaxMind (license-key) and G-NAF.
export MAXMIND_LICENSE_KEY=...
export GNAF_ARCHIVE_URL=https://...
./server/target/release/fetch-data --region au --wof --openaddresses au --maxmind --gnaf

# WoF only, scoped to specific countries.
./server/target/release/fetch-data --wof --wof-countries "au gb us" --data-dir ./data
```

License-gated sources are skipped with a structured warning when their
env var isn't set, so the binary degrades gracefully:

```bash
# MaxMind GeoLite2-City — free signup at maxmind.com/en/geolite2/signup.
# Required only for the /geocode/ip endpoint.
export MAXMIND_LICENSE_KEY=...

# G-NAF — Australian Geocoded National Address File. Manual license
# acceptance at https://geoscape.com.au/data/g-naf/. Paste the accepted
# URL into GNAF_ARCHIVE_URL.
export GNAF_ARCHIVE_URL=https://...
```

#### A note on OpenAddresses + AWS

OpenAddresses retired its free HTTPS bulk mirror; the only
programmatic path to the processed data is the Requester-Pays S3
bucket `s3://v2.openaddresses.io`. A free-tier AWS account is enough
— the requester-pays charge is single-digit dollars for the planet,
cents for a single country. If you can't use AWS at all:

- **AU-only:** omit `--openaddresses`. G-NAF is the better
  address-points dataset for AU anyway.
- **Other regions:** omit `--openaddresses`. OSM alone covers most
  `/reverse` queries; the address-point refinement on `/search` and
  `/validate` won't be available, but the service still works.
- **Specific countries:** the per-source `data` URLs in the
  [`openaddresses/openaddresses` GitHub repo's `sources/`](https://github.com/openaddresses/openaddresses/tree/master/sources)
  point at upstream open-data portals. No AWS needed, but each is in
  the upstream's native format (Shapefile / GeoJSON / WMS / KML) and
  requires a per-source format adapter.

Build the indexes:

```bash
# Indexer
mkdir build && cd build && cmake ../builder && make && cd ..

# Server + all build tools
cargo build --release --manifest-path server/Cargo.toml

# Index an OSM PBF (mandatory — the rest are additive)
./build/build-index data/index data/pbf/*.osm.pbf

# (Optional) forward search — tantivy per-country
./server/target/release/build-forward-index data/index --partition-by-country

# (Optional) autocomplete FST
./server/target/release/build-autocomplete-fst data/index

# (Optional, AU only) postcode lookup + G-NAF address points
./server/target/release/build-postcode-lookup data/gnaf/psv data/index
./server/target/release/build-gnaf-index data/gnaf/psv data/index

# (Optional, worldwide) OpenAddresses address points
./server/target/release/build-openaddresses-index data/openaddresses data/index

# Serve
./server/target/release/query-server data/index
```

The server starts on `0.0.0.0:3000` (REST) and `0.0.0.0:3001` (gRPC) by default.

### Storage sizing

Each data source drops files in the same index directory and is loaded independently at startup — leave any component out and the server degrades gracefully.

| Component | AU (single country) | Planet |
|---|---:|---:|
| OSM reverse index (geo, addr, admin, place, street, interp, strings, i18n) | ~620 MB | ~20 GB |
| Tantivy forward index (per-country + unified) | ~80 MB | ~2–3 GB |
| FST autocomplete (per-country + unified) | ~25 MB | ~600 MB |
| G-NAF address points (AU only) | ~490 MB | — |
| OpenAddresses per-country (~60 countries; AU skipped when G-NAF present) | — | ~4–8 GB |
| Who's on First admin fallback (per-country or planet) | ~50 MB | ~500 MB |
| Postcode lookup (AU only) | <1 MB | <1 MB |
| **Built index total** | **~1.4 GB** | **~28–33 GB** |

Source data needed during the build is substantially larger — the raw PBF, OpenAddresses global batch (~66 GB), and WoF planet SQLite (~8.6 GB) all sit on scratch disk until ingestion finishes. Point the build at local NVMe (the `r8gd.*` packer default) if you're running worldwide.

RAM guidance: AU-only fits a `t4g.medium` class instance; planet wants ≥16 GB at query time for a warm mmap working set, and ≥256 GB during **build** because libosmium's single-threaded pass holds the node cache in memory.

## HTTP API

The server is unauthenticated — every endpoint is open to any caller that can reach the port. Deploy behind a network boundary (VPC, service mesh, localhost bind, reverse proxy) to control access.

### GET /reverse

Coordinate → address.

```
GET /reverse?lat=-33.8688&lon=151.2093
GET /reverse?lat=-33.8688&lon=151.2093&lang=zh
```

Response follows [Nominatim's format](https://nominatim.org/release-docs/latest/api/Reverse/):

```json
{
  "display_name": "Avenue de la Costa 42, 98000 Monaco, Monaco",
  "address": {
    "house_number": "42",
    "road": "Avenue de la Costa",
    "city": "Monaco",
    "state": "Monaco",
    "county": "Monaco",
    "postcode": "98000",
    "country": "Monaco",
    "country_code": "MC"
  },
  "confidence": "exact"
}
```

Parameters:

| Param | Required | Description |
|---|---|---|
| `lat`, `lon` | yes | WGS84 coordinate |
| `lang` | no | ISO 639-1 language code; returns OSM `name:<lang>` tag for admin fields when available |
| `h3_res` | no | Comma-separated H3 resolutions (0–15, max 4); returns an `h3` map on the response. See the H3 section below. |

Typical p50 latency: **20–60 µs**.

### GET /search

Text → ranked coordinate candidates.

```bash
# Freeform
GET /search?q=10%20alysse%20close%20baulkham%20hills%20nsw

# Structured (takes precedence over q when both present)
GET /search?street=Alysse%20Close&housenumber=10&city=Baulkham%20Hills&country_code=AU

# Multi-country
GET /search?q=Elizabeth%20Street&country_code=US,CA,AU
```

Parameters:

| Param | Description |
|---|---|
| `q` | Freeform text. Parsed for house number (leading digits), state abbreviation, postcode, country hints |
| `street`, `housenumber`, `city`, `state` | Structured fields; take precedence over `q` |
| `country_code` | Single ISO 3166-1 alpha-2, or a comma-separated list (e.g. `US,CA,MX`). No cap on list length, but each code spawns one per-country search — keep it short (≤5) for sensible latency. |
| `kind` | `place` or `street` (filter) |
| `limit` | Integer 1–50 (default 10). Out-of-range values are silently clamped into this window. |
| `h3_res` | Comma-separated H3 resolutions (0–15, max 4); returns an `h3` map per hit. |

Response includes each hit's `confidence` label (`exact`, `interpolated`, `fallback`) and a `source` field when served from the FST fast-path.

Features:

- **Token canonicalisation** — `Hwy`/`Tce`/`Pde`/`Cres`/`Blvd`/`Ln`/`Ave`/`Rd`/`Dr`/`Ct`/`Cl`/`Pl` expand symmetrically at index + query time.
- **Diacritic folding** — `Zürich` ≡ `Zurich`, `Café` ≡ `Cafe`.
- **Rank-based ranking** — cities outrank streets of the same name.
- **Fallback ladder** — strict → drop country → state → city → kind → fuzzy. Fuzzy is **pinned to Levenshtein edit distance 1** (not tunable at runtime); queries more than one character off the target name will miss.
- **House-number refinement** — if a number is parsed, the coord is refined via G-NAF / OpenAddresses / OSM addr_point lookup.
- **FST fast-path** — exact-key queries bypass tantivy entirely, returning in ~400 ns.

Typical latency: **~400 ns** (FST fast-path) / **20–70 µs** (tantivy) / **~150 µs** (fuzzy fallback).

### GET /autocomplete

Prefix typeahead.

```
GET /autocomplete?q=alys&country_code=AU&limit=5
```

| Param | Description |
|---|---|
| `q` | Prefix to match against the FST. Required. |
| `country_code` | Single ISO 3166-1 alpha-2 to restrict to one country's FST. Omit to search all loaded countries. |
| `limit` | Integer 1–50 (default 10). Out-of-range values are silently clamped. |
| `h3_res` | Comma-separated H3 resolutions (0–15, max 4); returns an `h3` map per hit. |

Built per country from the OSM + place index as `fst_<cc>.fst` files (~16 MB for AU). Typical latency: **~7 µs** per query.

### GET /validate

Structured address validation.

```
GET /validate?street=Alysse%20Close&housenumber=10&city=Baulkham%20Hills&country_code=AU
```

Returns `verified: true/false`, a canonical normalised address, confidence level, and coordinate. Use for ingest-side address cleaning.

### GET /geocode/ip

IP → coordinate + full address via MaxMind GeoLite2.

```
GET /geocode/ip                                  # uses requester IP
GET /geocode/ip?ip=8.8.8.8                       # explicit override
```

Requires `GeoLite2-City.mmdb` in the data directory (free signup at [maxmind.com](https://www.maxmind.com/en/geolite2/signup)) or the `GEOLITE2_DB` env var pointing at one. Returns `503 Service Unavailable` when the DB isn't loaded.

### H3 cell enrichment

Any endpoint that returns a coordinate accepts an optional `h3_res` parameter — a comma-separated list of [Uber H3](https://h3geo.org/) resolutions (0–15, up to 4 values). The response gets an extra `h3` map keyed by resolution so downstream tools (Kepler.gl, DuckDB, Databricks, Snowflake) can do direct H3 joins without a per-row conversion step. Absent the parameter, no field is added — zero overhead for callers that don't ask.

```
GET /reverse?lat=-33.87&lon=151.21&h3_res=9
GET /search?q=Sydney&country_code=au&h3_res=7,9,12
```

```json
{
  "address": { ... },
  "h3": { "7": "872830828ffffff", "9": "8928308280fffff", "12": "8c28308280c01ff" }
}
```

Values are the standard 15-char lowercase hex cell IDs. Cells are computed at query time — nothing new is stored on disk. The same parameter and response field work over gRPC (`repeated uint32 h3_res` on requests, `map<uint32, string> h3` on responses).

### GET /h3

Pure `(lat, lon)` → H3 cell-map computation. Skips reverse-geocoding entirely — no mmap reads, no admin lookup, microsecond-scale per request. Use this when a client only needs spatial-join keys and would otherwise waste a `/reverse` round-trip per coord.

```
GET /h3?lat=-33.8568&lon=151.2153&h3_res=9
GET /h3?lat=-33.8568&lon=151.2153&h3_res=7,9,12
```

```json
{
  "lat": -33.8568,
  "lon": 151.2153,
  "h3": { "7": "87be0e35cffffff", "9": "89be0e35c0bffff", "12": "8cbe0e35c0943ff" }
}
```

`h3_res` is required here (a missing/empty value returns 400 — the call has no other purpose). Same 0–15 range, same 4-resolution cap, same wire-format conventions as the enrichment field on the other endpoints. Identical surface over gRPC: `Geocoder.H3(H3Request) → H3Response`.

## gRPC

A typed mirror of every REST endpoint. Service definition: [`server/proto/geocoder.proto`](server/proto/geocoder.proto).

Default bind: `0.0.0.0:3001`. Override with `--grpc-addr` or `GEOCODER_GRPC_ADDR`. Like the REST side, the gRPC surface is unauthenticated — gate it at the network layer.

Shared limits: `SearchRequest.limit` and `AutocompleteRequest.limit` are silently clamped into 1–50 (same behaviour as REST). `h3_res` accepts up to 4 resolutions; >4 returns `InvalidArgument`.

```
rpc Reverse(ReverseRequest) returns (AddressResponse);
rpc Search(SearchRequest) returns (SearchResponse);
rpc Validate(ValidateRequest) returns (ValidateResponse);
rpc Autocomplete(AutocompleteRequest) returns (AutocompleteResponse);
rpc IpGeocode(IpGeocodeRequest) returns (IpGeocodeResponse);
```

Disable with `--no-default-features --features forward` at build time.

## Data sources

The server loads whatever is present in the data directory; any missing source degrades gracefully to a simpler response.

### OpenStreetMap (always; primary)

Address points, street centrelines, admin polygons, `place=*` nodes, postcode boundaries. Built by the C++ `build-index` from any `.osm.pbf` file. See [ARCHITECTURE.md § "What the C++ indexer includes and excludes"](ARCHITECTURE.md#what-the-c-indexer-includes-and-excludes) for the exact tag filters (which `highway=*` types, which `place=*` ranks, which `admin_level`s). For multi-country/worldwide deployments including measured download/build times and RAM envelopes, see [`docs/worldwide-build.md`](docs/worldwide-build.md).

### G-NAF (Australia)

Authoritative AU addresses from [data.gov.au](https://data.gov.au/dataset/ds-dga-19432f89-dc3a-4ef3-b943-5326ef1dbecc). Two import paths:

- **Postcode lookup** (`build-postcode-lookup`, ~30 s, ~240 KB): suburb-modal postcode table that fills in `postcode` for reverse queries where OSM lacks `boundary=postal_code` (OSM covers <5% of AU postcodes).
- **Full address-point index** (`build-gnaf-index`, ~3 min, ~488 MB): 16.4 M AU addresses with exact geocodes and per-address postcodes. Routes `find_addr_point` through G-NAF first for AU queries — `10 Alysse Close` returns the real G-NAF coord, not the street centroid.

```bash
# After downloading the G-NAF ZIP from data.gov.au:
unzip -j g-naf_*_allstates_gda2020_psv_*.zip \
    '*_LOCALITY_psv.psv' '*_STATE_psv.psv' '*_ADDRESS_DETAIL_psv.psv' \
    '*_ADDRESS_DEFAULT_GEOCODE_psv.psv' '*_STREET_LOCALITY_psv.psv' \
    -d data/gnaf/psv

build-postcode-lookup data/gnaf/psv data/index
build-gnaf-index data/gnaf/psv data/index
```

**Attribution required** (CC-BY 4.0): this distribution incorporates data from G-NAF © Commonwealth of Australia (Geoscape Australia).

### OpenAddresses.io (worldwide)

Authoritative addresses from ~60 countries (US, FR, DE, NL, ES, BE, CH, PL, DK, CA, and more). Per-country binary files so you only mount the countries you serve.

```bash
# After extracting an OpenAddresses batch under data/openaddresses/:
build-openaddresses-index data/openaddresses data/index \
    --country us,fr,de \
    --skip au           # use G-NAF direct for AU instead
```

### MaxMind GeoLite2 (IP geocoding)

Optional. Drop `GeoLite2-City.mmdb` into the data directory to enable `/geocode/ip`.

## Updates & hot reload

`scripts/update-index.sh` automates a full zero-downtime refresh:

1. `pyosmium-get-changes` pulls OSM diffs since the local PBF's timestamp.
2. `osmium apply-changes` updates the PBF.
3. `build-index` rewrites the binary index into a new directory.
4. Atomic `mv` swaps directories.
5. Touching the reload marker prompts the server to re-mmap within 5 s.

In-flight queries keep the old `Arc<Index>` until they return; new queries see the new one. No dropped requests.

```bash
# Nightly cron
0 3 * * * DATA_DIR=/data \
          REPLICATION_URL=https://download.geofabrik.de/australia-oceania-updates \
          /path/to/scripts/update-index.sh
```

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `DATA_DIR` | `/data` | Data directory (PBFs under `pbf/`, indexes under `index/`) |
| `BIND_ADDR` | `0.0.0.0:3000` | REST bind address |
| `GEOCODER_GRPC_ADDR` | `0.0.0.0:3001` | gRPC bind address |
| `DOMAIN` | (off) | Domain name for automatic HTTPS via Let's Encrypt |
| `CACHE_DIR` | `acme-cache` | ACME certificate cache |
| `PBF_URLS` | — | Space-separated list of PBF download URLs (required for `auto`/`build` unless `REGION` is set) |
| `REGION` | — | Named Geofabrik region preset (e.g. `oceania`) |
| `FORWARD_INDEX` | `1` | Build the tantivy forward index in `auto`/`build` modes (set to `0` to skip) |
| `GEOCODER_RELOAD_MARKER` | `$DATA_DIR/index/.reload` | Path to the hot-reload marker file |
| `GEOCODER_RELOAD_INTERVAL_SEC` | `5` | Reload marker poll interval |
| `GEOCODER_ADMIN_CONFIG` | (embedded) | Path to a JSON file overriding the `admin_level` → output-field mapping |
| `GEOLITE2_DB` | `$DATA_DIR/GeoLite2-City.mmdb` | MaxMind GeoLite2 path for IP geocoding |

## Tooling

| Binary | Purpose |
|---|---|
| `build-index` (C++) | Parse OSM PBF → OSM binary index |
| `build-forward-index` | Tantivy index for `/search`. `--partition-by-country` emits per-country indexes |
| `build-autocomplete-fst` | FST prefix index for `/autocomplete` + `/search` fast-path |
| `build-postcode-lookup` | G-NAF suburb-modal postcode table |
| `build-gnaf-index` | Full G-NAF address-point index |
| `build-openaddresses-index` | Per-country OpenAddresses address-point index |
| `query-server` | The HTTP + gRPC server |

All Rust binaries take `--help`.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for:

- Complete data-flow diagram (PBF → binaries → query server)
- Every binary file's record format
- Reverse + forward query paths as numbered flows
- Deployment sizing recommendations for AWS (EC2/EBS/NVMe)
- Comparison to Radar's HorizonDB architecture

## License

Apache License, Version 2.0. Original copyright © Traccar (upstream project); additions copyright © this project's contributors.

Data licences travel through the index:

- **OpenStreetMap**: ODbL 1.0. Attribute OSM and its contributors when using the output.
- **G-NAF**: CC-BY 4.0. Attribute "G-NAF © Commonwealth of Australia (Geoscape Australia)" when redistributing.
- **OpenAddresses.io**: per-source, mostly CC-BY / CC0 / ODbL. Carry through attributions from the included `CREDITS.md` in your distribution.
- **MaxMind GeoLite2**: CC BY-SA 4.0. Attribute MaxMind when exposing IP-geocoding results.
