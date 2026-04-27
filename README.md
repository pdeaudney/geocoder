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
./target/release/fetch-data --region europe --data-dir ./data
./target/release/fetch-data --region north-america --data-dir ./my-data
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
# Build the binary once. The cargo workspace lives at the repo
# root, so the binary lands at ./target/release/fetch-data
# (NOT ./server/target/...).
cargo build --release --manifest-path server/Cargo.toml --bin fetch-data

# All-in-one fetch — OSM PBF + WhosOnFirst (+ optional OpenAddresses, MaxMind, G-NAF).
# Defaults output to ./data/. Conditional GET + resumable downloads:
# re-runs are bandwidth-cheap (304 short-circuit) and a killed run resumes
# from the .partial sidecar on the next invocation.

# Worldwide builds — RECOMMENDED.
# 9 Geofabrik continent extracts in parallel. Faster CDN throughput
# than planet.osm.org's single throttled stream, lower per-pass
# memory pressure (each continent's working set is a fraction of
# planet's), and per-continent resumability if one fails. Build
# pipeline's pass-4 dedup handles the ~5 % border overlap; the
# resulting merged index is functionally identical to a planet
# build for the geocoder query workload.
./target/release/fetch-data --region all-continents --wof

# Regional builds — cheap and fast for single-region serving.
./target/release/fetch-data --region au --wof          # AU-only
./target/release/fetch-data --region oceania --wof     # full Australia/Oceania
./target/release/fetch-data --region europe --wof      # EU

# Legacy / fallback: single 80 GB stream from planet.openstreetmap.org.
# Use only when Geofabrik is unreachable or you need a canonically
# complete planet (relevant for OSM analytics or compliance use cases
# that care about cross-continent multipolygon relations — not the
# typical geocoding workload).
./target/release/fetch-data --region planet --wof
```

Full CLI surface (`./target/release/fetch-data --help`):

```text
Acquire OSM PBF + WoF + OpenAddresses + MaxMind + G-NAF for the geocoder build pipeline.

Usage: fetch-data [OPTIONS]

Options:
      --data-dir <DATA_DIR>            Output root. Layout: <dir>/pbf/, <dir>/openaddresses/, etc
                                       [env: DATA_DIR=] [default: ./data]
      --region <REGION>                OSM region preset (see the region table below)
      --wof                            Fetch WhosOnFirst admin SQLite
      --wof-countries <WOF_COUNTRIES>  WoF scope: "planet" (default), "none", or
                                       space-separated alpha-2 codes
                                       [env: WOF_COUNTRIES=] [default: planet]
      --openaddresses                  Fetch OpenAddresses (requires AWS creds for
                                       s3://v2.openaddresses.io)
      --oa-sources <OA_SOURCES>        OA source filter — "all" (default) or
                                       space-/comma-separated alpha-2 codes
                                       [env: OA_SOURCES=] [default: all]
      --maxmind                        Fetch MaxMind GeoLite2-City (requires MAXMIND_LICENSE_KEY)
      --gnaf                           Fetch G-NAF (requires GNAF_ARCHIVE_URL)
      --parallel <PARALLEL>            Concurrent download streams (e.g. all-continents)
                                       [env: FETCH_PARALLEL=] [default: 4]
      --force                          Re-download even if local data appears fresh
      --no-verify-md5                  Skip MD5 verification against upstream sidecar
      --no-resume                      Disable resuming from <dest>.partial files
      --quiet                          No terminal progress bars (logs are unaffected)
  -h, --help                           Print help (see more with '--help')
  -V, --version                        Print version
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

Mix and match sources by combining flags. License-gated sources
(MaxMind, G-NAF) need an env var set; the binary skips them with a
structured warning otherwise.

```bash
# AU + OpenAddresses + MaxMind + G-NAF
export MAXMIND_LICENSE_KEY=...   # free signup: maxmind.com/en/geolite2/signup
export GNAF_ARCHIVE_URL=https://...  # license-accepted URL from data.gov.au
./target/release/fetch-data --region au --wof --openaddresses au --maxmind --gnaf

# WoF only, scoped to specific countries
./target/release/fetch-data --wof --wof-countries "au gb us"
```

OpenAddresses lives in the Requester-Pays S3 bucket
`s3://v2.openaddresses.io` (the free HTTPS mirror was retired). A
free-tier AWS account suffices — the egress charge is single-digit
dollars for the global scope, cents per country. The binary uses the
standard credential chain (env, `AWS_PROFILE`, EC2 IMDS, SSO,
`credential_process`) so any auth flow your existing tooling expects
will work. If AWS isn't an option, omit `--openaddresses`: OSM alone
covers most `/reverse` queries, and G-NAF is the better address-points
source for AU anyway.

Build the indexes:

```bash
# C++ indexer (out-of-tree under ./build/)
make builder

# Rust binaries (workspace target → ./target/release/)
cargo build --release --manifest-path server/Cargo.toml --bins
cargo build --release -p wof-importer

# Index an OSM PBF (mandatory — the rest are additive)
./build/build-index data/index data/pbf/*.osm.pbf

# (Recommended) WhosOnFirst country polygons — runtime fallback for
# country-code resolution when OSM's admin_level=2 boundary is
# missing from the input PBF. Geofabrik regional extracts like
# great-britain-latest and us-latest commonly drop the country
# relation, so without this step /reverse queries in those regions
# may return empty `country` fields. Reads the
# whosonfirst-data-admin-*.db SQLite that `fetch-data --wof` placed
# in ./data/ and writes wof_countries.bin into the index dir.
./target/release/wof-importer ./data ./data/index

# (Optional) forward search — tantivy per-country
./target/release/build-forward-index data/index --partition-by-country

# (Optional) autocomplete FST
./target/release/build-autocomplete-fst data/index

# (Optional, AU only) postcode lookup + G-NAF address points
./target/release/build-postcode-lookup data/gnaf/psv data/index
./target/release/build-gnaf-index data/gnaf/psv data/index

# (Optional, worldwide) OpenAddresses address points
./target/release/build-openaddresses-index data/openaddresses data/index

# Serve
./target/release/query-server data/index
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

## Observability

Production-ready instrumentation across three surfaces. Everything degrades gracefully when its env var isn't set, so a dev `cargo run` doesn't need any of this configured.

### Health probes

| Path | Status code | Purpose |
|---|---|---|
| `GET /healthz` | 200 always (process-alive only) | Backwards-compatible blanket healthcheck. |
| `GET /healthz/live` | 200 always | k8s liveness probe — distinct from ready. |
| `GET /healthz/ready` | 503 while loading; 200 once mmap'd | ALB / k8s readiness probe. Drains the host from rotation by flipping back to 503 on graceful shutdown. |
| `GET /healthz/indexes` | 200 with per-index summary JSON | Observability — what's loaded, file sizes, mtimes. |

The pre-deploy smoke test at `scripts/smoke-test.sh` exercises all four plus the query endpoints.

### Prometheus `/metrics`

Scrape `GET /metrics` for the canonical text-exposition format. Six metrics shipped today:

  - `geocoder_requests_total{endpoint,country}` — request counter with country code derived from the response.
  - `geocoder_request_duration_seconds{endpoint}` — histogram with explicit buckets matched to our SLO targets.
  - Plus four shadow-validation metrics (see below).

Default port: same as the REST surface (`/metrics` on `:3000`). Recording rules + multi-window burn-rate alerts are pre-built in [`docs/alerts/prometheus-alerts.yaml`](docs/alerts/prometheus-alerts.yaml). SLO definitions and runbook entries: [`docs/sli-slo.md`](docs/sli-slo.md), [`RUNBOOK.md`](RUNBOOK.md).

### OpenTelemetry export (traces + metrics)

OTLP gRPC or HTTP/protobuf, against any OTel-native backend (Honeycomb, Datadog APM, New Relic, Tempo, Mimir, etc.):

```bash
OTEL_TRACE_ENABLED=true \
OTEL_METRICS_ENABLED=true \
OTEL_EXPORTER_OTLP_ENDPOINT=https://otel-collector.your-domain:4317 \
OTEL_SERVICE_NAME=geocoder \
./target/release/query-server data/index
```

Per-request server spans (REST + gRPC) carry semconv attributes; a parent span wraps each handler call. Internal-log dedup (process-static, per-callsite) suppresses spammy collector-down warnings — see `GEOCODER_LOG_DEDUP_WINDOW_SEC`.

The Prometheus surface and the OTLP exporter are independent: enable either, both, or neither. Failures on one path never block the other or the request hot loop. Detailed deployment guidance + the K8s sidecar pattern: [`docs/kubernetes-deployment.md`](docs/kubernetes-deployment.md).

### Shadow validation against Google Geocoding

The server can fire a sampled async copy of every reverse / forward query at Google's Geocoding API and compare results — letting you measure accuracy drift over time without affecting request latency. The shadow worker is a fire-and-forget mpsc dispatcher; the request hot loop never awaits Google.

```bash
GOOGLE_GEOCODING_ENABLED=true \
GOOGLE_GEOCODING_API_KEY=AIza... \
GOOGLE_GEOCODING_SAMPLE_RATE=0.001 \
GOOGLE_GEOCODING_DAILY_CAP=1000 \
./target/release/query-server data/index
```

Defaults: `SAMPLE_RATE=0.001` (0.1 % of requests), `DAILY_CAP=1000` calls/day. The cap is hard-clamped to `MAX_DAILY_CAP=100_000` regardless of env value (process-wide constant in `server/src/shadow.rs`) so a misconfiguration can't drain a 7-figure quota overnight.

Four metrics surface the comparison:

  - `geocoder_shadow_outcomes_total{endpoint,outcome}` — per-call outcome (sent, queue_full, throttled, request_denied, etc.).
  - `geocoder_shadow_match_total{endpoint,axis,result}` — per-axis agreement (country / state / locality / street).
  - `geocoder_shadow_distance_meters{endpoint}` — histogram of the haversine distance between our coordinate and Google's.
  - `geocoder_shadow_queue_full_total` — back-pressure counter (the dispatcher has a bounded mpsc channel).

Sticky-disable behaviour: once Google returns `REQUEST_DENIED` or `OVER_QUERY_LIMIT`, the worker stops issuing new calls until the next UTC-midnight reset, so a billing accident can't drain your daily allowance.

Costs: at $5/1000 queries × 0.1 % default sample × 1 M req/day = **~$5/day**. Tune `GOOGLE_GEOCODING_SAMPLE_RATE` and `GOOGLE_GEOCODING_DAILY_CAP` for whatever budget you've signed off on.

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
| `MAXMIND_FALLBACK_TO_DBIP` | `true` | When `MAXMIND_LICENSE_KEY` is unset, `fetch-data --maxmind` falls back to DB-IP's free IP-to-City Lite dataset (CC-BY 4.0, no signup, MMDB-format-compatible). Set to `false` for strict MaxMind-only mode. The `<dest>.mmdb.source` sidecar records which dataset is currently installed. |
| `GEOCODER_LOG_FORMAT` | autopick: `pretty` if tty, `json` otherwise | Log encoder. `json` (one structured event per line), `pretty` (human-readable), or `compact`. |
| `GEOCODER_LOG_DEDUP_WINDOW_SEC` | `30` | Per-callsite throttle window for repeated log events (suppresses opentelemetry-exporter spam during a collector outage). `0` disables. |
| **OpenTelemetry traces + metrics** | | See the [Observability section](#observability) for end-to-end usage. |
| `OTEL_TRACE_ENABLED` | follows endpoint | Master switch for OTLP trace export (`true` / `false`). Unset → enabled iff `OTEL_EXPORTER_OTLP_ENDPOINT` is set. |
| `OTEL_METRICS_ENABLED` | follows endpoint | Master switch for OTLP metric export (independent of traces). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | (off) | Collector endpoint, e.g. `http://otel-collector:4317`. Shared by traces + metrics. |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` | `grpc` or `http/protobuf`. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | inherits | Per-signal override (rarely needed). |
| `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | inherits | Per-signal override. |
| `OTEL_METRIC_EXPORT_INTERVAL` | `30000` (ms) | Periodic metric export interval. Clamped to `[1000, 300_000]`. |
| `OTEL_SERVICE_NAME` | `geocoder` | Resource attribute for the service. |
| `OTEL_RESOURCE_ATTRIBUTES` | (empty) | Standard OTel `key=value,key2=value2` resource attributes (env, version, etc.). |
| **Shadow validation** (`/reverse`, `/search`) | | See the [Observability section](#observability) for metric semantics. |
| `GOOGLE_GEOCODING_ENABLED` | `false` | Master switch for the Google shadow worker. Independent from the API key so an operator can toggle the feature without rotating the key. |
| `GOOGLE_GEOCODING_API_KEY` | — | Google Maps Platform API key with the Geocoding API enabled. Required when shadow is on. |
| `GOOGLE_GEOCODING_SAMPLE_RATE` | `0.001` | Fraction of requests that trigger a shadow call (`0.0`–`1.0`). |
| `GOOGLE_GEOCODING_DAILY_CAP` | `1000` | Daily call ceiling. Hard-clamped to `MAX_DAILY_CAP=100_000`. Resets at UTC midnight. |

## Tooling

| Binary | Purpose |
|---|---|
| `build-index` (C++) | Parse OSM PBF → OSM binary index |
| `wof-importer` | WhosOnFirst SQLite → `wof_countries.bin` (runtime country-code fallback) |
| `build-forward-index` | Tantivy index for `/search`. `--partition-by-country` emits per-country indexes |
| `build-autocomplete-fst` | FST prefix index for `/autocomplete` + `/search` fast-path |
| `build-postcode-lookup` | G-NAF suburb-modal postcode table |
| `build-gnaf-index` | Full G-NAF address-point index |
| `build-openaddresses-index` | Per-country OpenAddresses address-point index |
| `fetch-data` | Acquire OSM PBF + WoF + OpenAddresses + MaxMind/DB-IP + G-NAF |
| `query-server` | The HTTP + gRPC server |

All Rust binaries take `--help`.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for:

- Complete data-flow diagram (PBF → binaries → query server)
- Every binary file's record format
- Reverse + forward query paths as numbered flows
- Deployment sizing recommendations for AWS (EC2/EBS/NVMe)
- Comparison to Radar's HorizonDB architecture

## Operations

| Topic | Doc |
|---|---|
| Per-alert response procedures + on-call playbook | [`RUNBOOK.md`](RUNBOOK.md) |
| SLO targets per endpoint + multi-window burn-rate alert pattern | [`docs/sli-slo.md`](docs/sli-slo.md) |
| Prometheus alert + recording-rule definitions | [`docs/alerts/`](docs/alerts/) |
| K8s deployment patterns (sidecar OTel, resource floors, HPA) | [`docs/kubernetes-deployment.md`](docs/kubernetes-deployment.md) |
| Capacity plan: per-instance resource floors, throughput methodology | [`docs/performance/capacity-plan.md`](docs/performance/capacity-plan.md) |
| Worldwide-build wall-time + memory envelopes | [`docs/worldwide-build.md`](docs/worldwide-build.md) |
| Performance snapshots (LTO config, hashmap choice, read-path optimisations) | [`docs/performance/`](docs/performance/) |

### Load testing

Two k6 workloads, both driven through `scripts/bench-http.sh` (which boots
the server, manages cold/warm OS-cache modes, runs k6 via Docker, merges
JSON reports, and diffs against prior runs):

```bash
# AU workload (default) — five hardcoded-fixture scenarios.
./scripts/bench-http.sh

# Planet workload — three multi-country scenarios driven by Geonames-
# derived fixtures across US/GB/FR/DE/NL/ES/AU/CA. Run the fixture
# build once before the first planet bench (~50 s, downloads ~106 MB
# from Geonames + a 1 MB Pelias clone).
./scripts/bench/build-fixtures.sh
./scripts/bench-http.sh --workload planet --index /data/index
```

The planet workload's three scenarios:

  - `reverse_planet` — 5,000 balanced (lat, lon, country) coords, p99 SLO 50 ms
  - `search_planet` — 2,000 freeform city queries, p99 SLO 100 ms
  - `autocomplete_typeahead` — 1,458 prefixes spanning 1–6 chars across the 8 countries, p99 SLO 30 ms

Reports land at `tests/regression/reports/http-bench-planet-<label>.json`
(separate from the AU stream so prior-run diffs match workload to workload).
Fixture build script + the JSONs themselves live under
`scripts/bench/fixtures/`; refresh with `./scripts/bench/build-fixtures.sh`
when Geonames publishes a new monthly snapshot.

The load test only checks status codes (`2xx`) — it doesn't validate
that responses are correct. For correctness against the same dataset,
run the **bench-accuracy** companion:

```bash
# Reuses the bench fixtures; samples 500 rows per scenario by default
# and exits non-zero if the overall pass rate < 95 %.
./scripts/run-bench-accuracy.sh --index /data/index

# Or via Makefile (defaults to ./data/index, override with INDEX=…)
make bench-accuracy INDEX=/data/index SAMPLE=2000
```

Three accuracy assertions, one per scenario:

  - **`reverse`** — `response.address.country_code` must match the
    fixture row's source country_code.
  - **`search`** — top result's country_code must match AND its
    coords must be within `--search-radius-km` (default 100 km) of
    the fixture's `lat_hint`/`lng_hint`.
  - **`autocomplete`** — at least one result; for prefixes ≥ 3 chars,
    at least one result's normalised name must start with the prefix.

Output: human-readable per-country pass rates + a sample of failures
(country, request URL, reason) for grep-friendly triage, plus a JSON
report at `tests/regression/reports/bench-accuracy-<label>.json` for
diff-vs-prior comparisons. This is complementary to the
hand-curated [Pelias regression suite](#operations) — that one tests
specific addresses with specific expected fields; this one sweeps
breadth across 8 countries to catch country-wide regressions.

## License

Apache License, Version 2.0. Original copyright © Traccar (upstream project); additions copyright © this project's contributors.

Data licences travel through the index:

- **OpenStreetMap**: ODbL 1.0. Attribute OSM and its contributors when using the output.
- **G-NAF**: CC-BY 4.0. Attribute "G-NAF © Commonwealth of Australia (Geoscape Australia)" when redistributing.
- **OpenAddresses.io**: per-source, mostly CC-BY / CC0 / ODbL. Carry through attributions from the included `CREDITS.md` in your distribution.
- **MaxMind GeoLite2**: CC BY-SA 4.0. Attribute MaxMind when exposing IP-geocoding results.
