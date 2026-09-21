# Building a worldwide index

End-to-end walkthrough for producing a multi-country index from OSM
PBFs. Numbers below come from measured runs on the development
machine (M1 Max, 64 GB RAM, SSD) — your hardware will vary, but the
ratios hold roughly linearly with PBF size.

For single-region builds see [`README.md`](../README.md); for AWS
deployment see [`BUILD-DEPLOY.md`](../BUILD-DEPLOY.md). This doc
targets the operational sweet spot: one combined index spanning
multiple countries, served from a single `Index` handle.

## Picking a scope

OSM PBFs are distributed by Geofabrik at
[download.geofabrik.de](https://download.geofabrik.de/). The AU +
Oceania, Europe, and North America extracts dominate most practical
deployments.

### Download sizes (compressed, current)

| Region | File | Size |
|---|---|---:|
| Australia | `australia-latest.osm.pbf` | 890 MB |
| New Zealand | `new-zealand-latest.osm.pbf` | 377 MB |
| United Kingdom | `united-kingdom-latest.osm.pbf` | ~2 GB |
| Canada | `canada-latest.osm.pbf` | 5.9 GB |
| USA | `us-latest.osm.pbf` | 11 GB |
| **5-region sum** | | **~20 GB** |
| — | — | — |
| Europe continent | `europe-latest.osm.pbf` | ~30 GB |
| North America continent | `north-america-latest.osm.pbf` | ~15 GB |
| Planet | `planet-latest.osm.pbf` | ~75 GB |

Use the [United Kingdom extract](https://download.geofabrik.de/europe/united-kingdom.html) for UK coverage, including Northern Ireland.
The Great Britain extract omits Northern Ireland.

Download speed from Geofabrik fluctuates between 1 MB/s (throttled)
and 5 MB/s (healthy). Plan for ~1 hour to pull the 5-region set.

### Build sizes (measured)

| Scope | PBF in | Reverse index out | Build wall-time |
|---|---:|---:|---:|
| AU only | 890 MB | ~2 GB | 8 min |
| AU + NZ | 1.3 GB | 751 MB (reverse only) + ~300 MB (forward + FST) | 10 min |
| AU + NZ + GB + CA + US | ~20 GB | **6.7 GB** reverse only | **~3 h 15 min** (measured with the Great Britain extract on 2026-04-22; excludes Northern Ireland) |

Observed end-of-build totals for the 5-region combined index:

- 16.3 M street ways
- 52.2 M address points (30.3 M from buildings)
- 81 K admin/postcode boundaries (102 K polygon rings)
- 209 K place=\* points
- 174 K localized-name entries
- 55 MB string pool

The build time scales super-linearly with PBF size because of (a) the
single-threaded handler (see
[`research/libosmium-parallelisation.md`](research/libosmium-parallelisation.md))
and (b) SSD-bound node-location cache behaviour on machines where RAM
can't hold the full file-backed `SparseFileArray`. US alone accounted for
~60 % of the total wall-time despite being ~55 % of the PBF bytes.

The 751 MB "reverse only" includes `addr_points`, `street_*`,
`admin_*`, `place_*`, `i18n_names`, `strings`, `geo_cells`. The
forward (tantivy) index is a separate step; autocomplete FST is
another. Together they roughly double the reverse-only footprint.

### RAM peaks (observed)

Single-pass combined build processes each PBF sequentially but keeps
accumulating `addr_points`, `street_ways`, `admin_polygons`, etc.
in memory until the final write phase. As a rough guide:

| Scope | Peak RSS |
|---|---:|
| AU only | ~8 GB |
| AU + NZ | ~10 GB |
| 5-region worldwide | ~15 GB RSS + 23 GB file-backed node cache (observed) |
| Planet | ≥ 128 GB (extrapolated) |

On the 5-region build we observed peak process RSS around 15 GB with
an additional 23 GB of `node_locations.tmp` mmap file that the kernel
page-cached aggressively, squeezing free RAM to ~70 MB. The system
stayed responsive and the build completed cleanly, but on a smaller
box (≤ 32 GB) this is where the build would start thrashing.

64 GB RAM is comfortable for the 5-region set. Planet-scale builds
realistically need a larger box, or a split-and-merge approach we
haven't implemented.

### Disk envelope

- **Staging** (PBFs downloaded): ~20 GB for the 5-region set.
- **Build scratch**: the C++ indexer uses in-memory sort; on-disk
  scratch is negligible.
- **Final index**: see "Build sizes" above.
- **Forward index** (tantivy): grows roughly linearly with street+place
  counts. Order of ~300 MB for AU+NZ combined.
- **Autocomplete FST**: ~6 MB per country for the unified + per-country
  pair, dominated by the string pool.

For the 5-region set, plan for ~40–60 GB free on the build host
(PBFs + final indexes + tantivy + FST + headroom).

## Docker build

The repo's `Dockerfile` includes every index builder. The `build` entrypoint
fetches the selected sources, builds into `/data/index.next`, and publishes
`/data/index` only after the final autocomplete stage succeeds:

```bash
docker build -t geocoder:local .
export PBF_URLS='https://download.geofabrik.de/australia-oceania/australia-latest.osm.pbf https://download.geofabrik.de/australia-oceania/new-zealand-latest.osm.pbf https://download.geofabrik.de/europe/united-kingdom-latest.osm.pbf https://download.geofabrik.de/north-america/canada-latest.osm.pbf https://download.geofabrik.de/north-america/us-latest.osm.pbf'
docker run --rm -v "$PWD/data:/data" \
  -e PBF_URLS \
  -e WOF_COUNTRIES='au nz gb ca us' \
  -e OA_GEOJSON_SOURCES='us/ny/city_of_new_york us/ca/san_francisco us/va/statewide ca/on/city_of_toronto ca/ab/calgary' \
  geocoder:local build
```

The OpenAddresses list is a selected US/CA sample, not nationwide coverage;
review each source's licence before use. To add AU G-NAF, set a
license-accepted `GNAF_ARCHIVE_URL` on the host and pass
`-e GNAF_ARCHIVE_URL` to `docker run`. Prepared G-NAF PSV files, WoF SQLite,
or OpenAddresses CSVs under `/data` are also imported without a fetch flag.
Use `docker run -v "$PWD/data:/data" -p 3000:3000 geocoder:local serve`
after the build. An explicit `build` regenerates all stages; `auto` serves an
unchanged completed index and rebuilds when source files change.

## Download

Use the `fetch-data` binary (`server/src/bin/fetch_data.rs`). It
handles conditional GET, resumable downloads, and MD5 verification
in one tool. Example for the 5-region set:

```bash
for region in australia new-zealand united-kingdom canada usa; do
    ./target/release/fetch-data --data-dir ./data --region "$region"
done
```

Geofabrik publishes weekly updates and supports `*-updates/` endpoints
for incremental diffs — see [`scripts/update-index.sh`](../scripts/update-index.sh)
for the diff-then-rebuild pattern if you want to keep an index fresh
without re-downloading the whole PBF.

## Build (combined, single-pass)

The C++ indexer accepts multiple PBFs in one invocation. This is the
preferred shape — sharing the admin and street tables across regions
lets cross-border queries resolve correctly, and the single pass is
cheaper than N separate builds followed by a merge.

```bash
# Build once.
mkdir -p data/index-worldwide
./build/build-index data/index-worldwide \
    data/pbf/australia-latest.osm.pbf \
    data/pbf/new-zealand-latest.osm.pbf \
    data/pbf/united-kingdom-latest.osm.pbf \
    data/pbf/canada-latest.osm.pbf \
    data/pbf/us-latest.osm.pbf
```

### Forward index (tantivy)

```bash
./target/release/build-forward-index data/index-worldwide
```

Produces `data/index-worldwide/tantivy/` with a monolithic index by
default, or `data/index-worldwide/tantivy_<cc>/` per-country when
invoked with `--partition-by-country`. Per-country partitioning is
preferred for larger deployments because a tantivy-wide query with
`country_code` filter still scans the whole index; per-country cuts
that scan dramatically.

Forward search indexes address points and their exact postcodes in
each country shard. The autocomplete build also derives one feature
per observed postcode from address data and fills missing postcode
features from valid Who's On First postalcode centroids. Bare postcode search
uses that feature when available and falls back to the forward shards;
it does not claim an arbitrary house as the postcode's identity.
The same postcode can occur in multiple countries, so country filters
or a geographic bias resolve that ambiguity.
An explicit ISO alpha-2 `country_code` is the reliable way to resolve
an ambiguous code.
Country-name suffixes come from the loaded Who's on First country
polygons; add those polygons when onboarding another country.
Two-letter suffixes can also be region abbreviations (`CA` is
California or Canada), so callers should send `country_code` when
they mean an ISO code.
The parser recognises common four-to-ten-character postcode shapes;
callers should use the structured `postcode` parameter for shorter
codes or unusual local formats.

### Autocomplete FST

```bash
./target/release/build-autocomplete-fst data/index-worldwide --layout both
```

The `both` layout emits per-country FSTs (legacy, swappable one at a
time) and the unified FST (Radar-style, country-prefix automaton).
Server prefers unified at query time.
Postcodes are indexed with their usual display spacing and a compact
alias, so `EN5 2LP` and `EN52LP` find the same feature. Postcodes absent
from the source addresses remain absent from autocomplete and search.

### Who's on First country-polygon fallback

Geofabrik's country extracts (`united-kingdom-latest.osm.pbf`,
`us-latest.osm.pbf`) don't ship their own `admin_level=2` country
boundary relation — the relation references ways outside the extract
window, so it's omitted. Consequence: OSM-only admin lookups return
no `country_code` for GB or US coords, which cascades into the
autocomplete FST dropping those countries entirely and per-country
filtering in tantivy misfiring.

Fix (adopted from Pelias, scoped to country level only):

```bash
# 1. Fetch separate admin and postalcode SQLite snapshots for the five countries.
./target/release/fetch-data --data-dir ./test-data --wof --wof-postcodes \
    --wof-countries "au nz us ca gb"

# 2. Import country polygons and valid postcode centroids.
#    Produces wof_countries*.bin and wof_postcodes.tsv.
make wof-import WOF_INDEX_DIR=./data/index-worldwide
# or directly:
./target/release/wof-importer ./test-data ./data/index-worldwide

# 3. Rebuild the autocomplete FST used for postcode search.
./target/release/build-autocomplete-fst ./data/index-worldwide --layout both
```

For later postcode-only refreshes, run `make wof-postcodes-import`
followed by `build-autocomplete-fst`; this leaves live country polygon
files untouched.

The importer pulls only `placetype='country'` rows from WoF. After
Douglas-Peucker simplification, the on-disk footprint is ~30 MB for
every country globally. At query time `find_admin` first consults
the OSM admin hierarchy; when that returns no `country_code` the
WoF polygons are scanned (pre-computed sorted bboxes make this
~5–10 µs on a miss). Happy path — coords inside an OSM-indexed
country — incurs zero extra cost.

This is best done **before** rebuilding the forward + autocomplete
indexes, because both read the country_code through `find_admin` at
build time. If you skip this step the indexes will be missing
coverage for any country whose extract omitted the country-level
relation.

WoF postcodes live in separate databases from WoF admin data. The importer
skips deprecated records and invalid centroids, including 0,0. The NZ
snapshot currently has no usable coordinates, so NZ postcode search
continues to rely on OSM/OpenAddresses. Existing OSM, G-NAF, and
OpenAddresses postcode candidates take precedence; WoF fills missing
codes. The exported file starts with `#wof-postcodes-v1`; the FST builder
rejects other layouts. Attribution: [Who's On First data and source
licenses](https://whosonfirst.org/docs/licenses/).

### Optional: G-NAF (Australia only)

G-NAF is Australia's authoritative address dataset — swaps street
centroids for real housepoint coords on AU queries. Not applicable to
non-AU regions.

```bash
# After downloading the G-NAF ZIP from data.gov.au (license-accepted)
./target/release/build-gnaf-index /path/to/gnaf-unpacked data/index-worldwide
./target/release/build-postcode-lookup /path/to/gnaf-unpacked data/index-worldwide
```

### Optional: OpenAddresses (per-country for non-AU coverage)

```bash
# Public per-source GeoJSON converted to the CSV layout used by the builder.
python3 scripts/import-oa-geojson.py \
  us/ny/city_of_new_york us/ca/san_francisco us/va/statewide \
  ca/on/city_of_toronto ca/ab/calgary
./target/release/build-openaddresses-index data/openaddresses data/index-worldwide \
  --country us,ca
# Forward search and autocomplete must be refreshed after adding OA points.
./target/release/build-forward-index data/index-worldwide --partition-by-country
./target/release/build-autocomplete-fst data/index-worldwide --layout both
```

The five sources above are a **targeted US/CA sample**, not nationwide
OpenAddresses coverage. Their terms differ: [San Francisco uses PDDL](https://opendatacommons.org/licenses/pddl/1-0/),
[Virginia identifies its points as public domain](https://vgin.vdem.virginia.gov/datasets/virginia-address-points/about),
and [NYC Open Data allows unrestricted reuse](https://opendata.cityofnewyork.us/wp-content/uploads/NYC_OpenData_TechnicalStandardsManual.pdf).
The [Toronto](https://www.toronto.ca/city-government/data-research-maps/open-data/open-data-licence/)
and [Calgary](https://data.calgary.ca/stories/s/Open-Calgary-Terms-of-Use/u45n-7awa)
Open Government Licences require attribution. Check source terms before adding
more; the Bucks County source prohibits commercial use and is excluded.

For an already extracted OpenAddresses CSV batch, skip the conversion:

```bash
./target/release/build-openaddresses-index /path/to/openaddresses data/index-worldwide
```

Skips AU automatically (G-NAF is authoritative). Covers ~60 countries
with variable quality. See [`ARCHITECTURE.md`](../ARCHITECTURE.md) for
the licensing notes.

## Serve

```bash
./target/release/query-server data/index-worldwide 0.0.0.0:3000
```

Health check the load:

```bash
curl -s http://localhost:3000/healthz/indexes | jq
```

Confirm the `countries` array in `indexes.autocomplete.countries`
matches what you built. The health response is documented in
[`BUILD-DEPLOY.md § 5.4`](../BUILD-DEPLOY.md#54-target-group).

## Test against the Pelias gold set

Once the server is up, the worldwide regression runner pulls the
country-filtered subsets of the Pelias acceptance-tests corpus and
runs them against the live index:

```bash
make pelias-full-refresh    # regenerate per-country corpora (~1 s)
make regression-worldwide   # run all 5 country subsets
```

Reports land in `tests/regression/reports/pelias-<cc>-<UTC>.json`.
The runner checks that the forward index serves AU, NZ, US, CA, and GB before testing them.
Set `COUNTRY_CODES` to a space-separated list to run another scope.
Reports separate fully comparable cases, partial diagnostics, and unsupported cases.
The converter preserves Pelias's endpoint, request filters, and top-N threshold when our API supports them.
It never adds a country filter based on the expected answer.
Cases requiring Pelias-only filters or response fields do not count as fully comparable.
The comparable pass rate measures a narrow subset; partial diagnostics remain useful for tracking our own search quality.

## Updating incrementally

Two patterns, documented in [`BUILD-DEPLOY.md § "Updating the index"`](../BUILD-DEPLOY.md#updating-the-index):

1. **Immutable AMI per index version** — rebuild from scratch, bake a
   fresh AMI, roll instances. Canonical cloud-native flow.
2. **Hot reload** — replace `data/index-worldwide/` in place, `touch`
   the reload marker, the server atomically swaps via `ArcSwap` with
   no dropped connections.

For development iteration, pattern 2 is faster; for production, pattern 1
is safer.

## Troubleshooting

- **`build-index` is single-core bound**: expected. libosmium's Reader
  deliberately serialises handler dispatch; our `BuildHandler` runs on
  one thread no matter the box. See
  [`docs/research/libosmium-parallelisation.md`](research/libosmium-parallelisation.md)
  for the investigation and why tweaking thread counts doesn't help.
- **`build-index` OOM-killed mid-run**: increase RAM or split regions
  into smaller subsets. The indexer keeps the full `addr_points`,
  `street_*`, and `admin_*` tables in memory until the sort+write
  phase at the end.
- **`build-forward-index` slow**: the tantivy builder is single-threaded
  per segment; for very large indexes, consider the `--partition-by-country`
  flag so each country builds in parallel (we do this inside the binary
  when the flag is set).
- **Server starts but `/healthz/indexes` shows `"forward": { "default": false }`**:
  tantivy directory missing. Re-run `build-forward-index`.
- **Regression runner returns 0 results for every query**: check
  `/healthz/indexes` for `autocomplete.countries` and `forward.default`.
  An index built without the forward step serves only reverse.
