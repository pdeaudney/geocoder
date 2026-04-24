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

Supported region presets: `oceania` (default), `australia`, `new-zealand`, `africa`, `antarctica`, `asia`, `europe`, `north-america`, `south-america`, `central-america`, `russia`, `usa`, `planet`.

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

Custom region? Use the download helper directly:

```bash
./scripts/download-region.sh europe        # all of Europe
./scripts/download-region.sh north-america ./my-pbf-dir
```

### Build from source

Prerequisites: a C++17 compiler + CMake for the builder, Rust stable for the server, `protoc` for gRPC. On macOS:

```bash
brew install cmake libosmium protozero s2geometry protobuf
```

On Debian/Ubuntu:

```bash
apt-get install cmake libosmium2-dev libprotozero-dev libs2-dev \
                zlib1g-dev libbz2-dev libexpat1-dev liblz4-dev \
                protobuf-compiler
```

Then:

```bash
# Indexer
mkdir build && cd build && cmake ../builder && make && cd ..

# Server + all build tools
cargo build --release --manifest-path server/Cargo.toml

# Index an OSM PBF
./build/build-index data/index data/pbf/*.osm.pbf

# (Optional) forward search — tantivy per-country
./server/target/release/build-forward-index data/index --partition-by-country

# (Optional) autocomplete FST
./server/target/release/build-autocomplete-fst data/index

# Serve
./server/target/release/query-server data/index
```

The server starts on `0.0.0.0:3000` (REST) and `0.0.0.0:3001` (gRPC) by default.

### Storage sizing

| Deployment | OSM index | Tantivy | FST | G-NAF (AU) | Total |
|---|---:|---:|---:|---:|---:|
| Single country (AU) | 620 MB | 42 MB | 16 MB | 488 MB | **~1.2 GB** |
| EU-only (10 countries) | ~6 GB | ~400 MB | ~100 MB | n/a | **~6.5 GB** |
| Planet | ~20 GB | ~2 GB | ~600 MB | n/a | **~22 GB** |

The full planet index wants ≥16 GB of RAM or fast NVMe. Single-country deployments fit a `t4g.medium` class instance fine.

## HTTP API

All endpoints require an API key. Authentication is managed via a web dashboard served at the root URL — create an admin account on first launch, generate keys, set per-user rate limits.

### GET /reverse

Coordinate → address.

```
GET /reverse?lat=-33.8688&lon=151.2093&key=YOUR_KEY
GET /reverse?lat=-33.8688&lon=151.2093&lang=zh&key=YOUR_KEY
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
| `key` | yes | API key |

Typical p50 latency: **20–60 µs**.

### GET /search

Text → ranked coordinate candidates.

```bash
# Freeform
GET /search?q=10%20alysse%20close%20baulkham%20hills%20nsw&key=KEY

# Structured (takes precedence over q when both present)
GET /search?street=Alysse%20Close&housenumber=10&city=Baulkham%20Hills&country_code=AU&key=KEY

# Multi-country
GET /search?q=Elizabeth%20Street&country_code=US,CA,AU&key=KEY
```

Parameters:

| Param | Description |
|---|---|
| `q` | Freeform text. Parsed for house number (leading digits), state abbreviation, postcode, country hints |
| `street`, `housenumber`, `city`, `state`, `country_code` | Structured fields; take precedence over `q` |
| `kind` | `place` or `street` (filter) |
| `limit` | 1–50 (default 10) |

Response includes each hit's `confidence` label (`exact`, `interpolated`, `fallback`) and a `source` field when served from the FST fast-path.

Features:

- **Token canonicalisation** — `Hwy`/`Tce`/`Pde`/`Cres`/`Blvd`/`Ln`/`Ave`/`Rd`/`Dr`/`Ct`/`Cl`/`Pl` expand symmetrically at index + query time.
- **Diacritic folding** — `Zürich` ≡ `Zurich`, `Café` ≡ `Cafe`.
- **Rank-based ranking** — cities outrank streets of the same name.
- **Fallback ladder** — strict → drop country → state → city → kind → fuzzy (Levenshtein 1) on name.
- **House-number refinement** — if a number is parsed, the coord is refined via G-NAF / OpenAddresses / OSM addr_point lookup.
- **FST fast-path** — exact-key queries bypass tantivy entirely, returning in ~400 ns.

Typical latency: **~400 ns** (FST fast-path) / **20–70 µs** (tantivy) / **~150 µs** (fuzzy fallback).

### GET /autocomplete

Prefix typeahead.

```
GET /autocomplete?q=alys&country_code=AU&limit=5&key=KEY
```

Built per country from the OSM + place index as `fst_<cc>.fst` files (~16 MB for AU). Typical latency: **~7 µs** per query.

### GET /validate

Structured address validation.

```
GET /validate?street=Alysse%20Close&housenumber=10&city=Baulkham%20Hills&country_code=AU&key=KEY
```

Returns `verified: true/false`, a canonical normalised address, confidence level, and coordinate. Use for ingest-side address cleaning.

### GET /geocode/ip

IP → coordinate + full address via MaxMind GeoLite2.

```
GET /geocode/ip?key=KEY                          # uses requester IP
GET /geocode/ip?ip=8.8.8.8&key=KEY               # explicit override
```

Requires `GeoLite2-City.mmdb` in the data directory (free signup at [maxmind.com](https://www.maxmind.com/en/geolite2/signup)) or the `GEOLITE2_DB` env var pointing at one. Returns `503 Service Unavailable` when the DB isn't loaded.

### H3 cell enrichment

Any endpoint that returns a coordinate accepts an optional `h3_res` parameter — a comma-separated list of [Uber H3](https://h3geo.org/) resolutions (0–15, up to 4 values). The response gets an extra `h3` map keyed by resolution so downstream tools (Kepler.gl, DuckDB, Databricks, Snowflake) can do direct H3 joins without a per-row conversion step. Absent the parameter, no field is added — zero overhead for callers that don't ask.

```
GET /reverse?lat=-33.87&lon=151.21&h3_res=9&key=KEY
GET /search?q=Sydney&country_code=au&h3_res=7,9,12&key=KEY
```

```json
{
  "address": { ... },
  "h3": { "7": "872830828ffffff", "9": "8928308280fffff", "12": "8c28308280c01ff" }
}
```

Values are the standard 15-char lowercase hex cell IDs. Cells are computed at query time — nothing new is stored on disk. The same parameter and response field work over gRPC (`repeated uint32 h3_res` on requests, `map<uint32, string> h3` on responses).

## gRPC

A typed mirror of every REST endpoint. Service definition: [`server/proto/geocoder.proto`](server/proto/geocoder.proto).

Default bind: `0.0.0.0:3001`. Override with `--grpc-addr` or `GEOCODER_GRPC_ADDR`.

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
