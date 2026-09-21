# Binary file format reference

Canonical reference for the on-disk `.bin` files that make up a
geocoder index. Written for two audiences:

1. **Human engineers** adding new fields, debugging a stale index,
   or writing tooling that reads these files directly (`tools/index-dumper`
   is the existing reader; a future parquet exporter or DuckDB
   extension would look similar).
2. **AI agents** (including future Claude sessions) that need to
   reason about cross-file invariants without re-grep'ing the whole
   tree. If a format changes, update this doc in the same commit.

Companion docs:
- [`docs/worldwide-build.md`](worldwide-build.md) — which builder
  writes what and how long it takes.
- [`docs/inspection/README.md`](inspection/README.md) — DuckDB
  recipes that depend on the layouts documented below.

## Design principles

1. **Mmap-first, zero-parse.** Files are laid out so the runtime
   does `mmap → reinterpret bytes as &[SomeStruct]` with no
   deserialization cost. Every format is a dense packed array of
   fixed-size records (or a string pool), addressed by offset.
2. **`#[repr(C)]` structs, native-endian.** No serde, no bincode,
   no length prefixes. Struct layout is the C ABI.
3. **Little-endian only.** All modern production targets (x86_64,
   aarch64 / Apple Silicon / Graviton, modern Linux / macOS / Windows)
   are little-endian. Files ARE NOT portable to big-endian hosts.
4. **No `usize`/`isize` on disk.** Every field's width is explicitly
   declared (`u32`, `f32`, `u8`, etc.) so files are portable between
   32-bit and 64-bit hosts and between architectures of the same
   endianness.
5. **Optional files are optional.** Every loader that reads an
   "optional" file returns `Ok(None)` when it's absent; the server
   degrades gracefully. Rule of thumb: a deployment that only has
   `strings.bin` + `geo_cells.bin` + core OSM files should still
   start and serve reverse geocoding.

## Cross-arch compatibility

| Build host | Serve host | OK? |
|---|---|---|
| Apple Silicon (M1–M4) | AWS Graviton (r7g/r8g/c7g) | ✅ |
| AWS Graviton | AWS Intel/AMD (c6i/r6i/m6i) | ✅ |
| x86_64 Linux | x86_64 macOS | ✅ |
| Any little-endian | Any little-endian | ✅ |
| Little-endian | Big-endian (MIPS, PowerPC big-endian) | ❌ would read garbage |

You can build the index on an r8g.16xlarge (Graviton 4, aarch64)
and serve it from a c6i.large (Intel, x86_64) without any
repackaging. The struct-level analysis: see "Per-struct size and
alignment" below — every struct we write has max alignment 4
(no 64-bit scalars) and explicit integer widths.

## Shared conventions

### String pool (`strings.bin` and friends)

Strings are interned into a NUL-terminated pool: a single `.bin`
file where strings are packed end-to-end separated by `0x00`. Field
types that reference strings carry a `u32 name_id` / `housenumber_id`
/ etc., which is a byte offset into the pool. `offset 0` is
conventionally the empty string (a single NUL byte at the start of
every string pool).

**Readers**: `Index::get_string(offset: u32) -> &str` in `server/src/lib.rs`.
**Writers**: intern-and-return-offset helpers in every builder.

Each subsystem has its own pool to keep pool offsets bounded to
`u32` per subsystem:

- `strings.bin` — OSM-derived entities (streets, places, admin,
  addresses, i18n)
- `gnaf_strings.bin` — G-NAF (AU authoritative address) pool
- `oa_<cc>_strings.bin` — one OpenAddresses pool per country
- `postcode_lookup_strings.bin` — postcodes only
- `fst_<cc>_strings.bin` / `fst_unified_strings.bin` —
  autocomplete name/suburb pools
- `wof_countries_strings.bin` — WoF country names

### S2 cell indices (`*_cells.bin` + `*_entries.bin`)

Spatial lookup. The C++ builder covers each feature (street
midpoint, admin polygon centroid, place point, address point) with
an S2 cell at a configurable level (default street=17, admin=10).
At query time:

1. Project the query coord to the same S2 cell level.
2. Scan a 9-cell neighbourhood (query cell + 8 neighbours).
3. For each cell, find its offset/length in `*_cells.bin` and
   iterate the record IDs in `*_entries.bin[offset..offset+length]`.
4. Run the appropriate predicate (point-in-polygon, nearest-point,
   etc.) against those candidate records.

`*_cells.bin` format: sorted array of `(cell_id: u64, offset_in_entries: u32, length: u32)`
structs. Binary-searched at query time.

`*_entries.bin` format: flat `u32` array of record indices into
the corresponding records file (`street_ways.bin`, `addr_points.bin`,
`admin_polygons.bin`, etc.).

**Readers**: `Index::lookup_admin_cell`, `find_street`, `find_place`
in `server/src/lib.rs`.

### Country-code packing

ISO 3166-1 alpha-2 country codes are packed into a single `u16`
so they fit inside the `#[repr(C)]` admin-polygon record:

```
country_code: u16 = (upper(cc[0]) as u16) << 8 | (upper(cc[1]) as u16)
```

Example: `"AU"` → `0x4155` (0x41='A', 0x55='U'). Unpacking:
`[((cc>>8) as u8), ((cc & 0xFF) as u8)]` gives `[b'A', b'U']`.

**Writers**: C++ `build-index` (packs admin-level-2 polygons) +
Rust `wof-importer` (packs WoF country polygons). Both follow
the same convention.

**Readers**: everywhere that converts back to `&str` via
`std::str::from_utf8(&[b1, b2])`.

A packed value of `0` means "no country code set" — the field is
optional. Runtime treats `country_code == 0` as `None`.

## Per-struct size and alignment

Every struct we write to disk, with its `#[repr(C)]` computed
layout. Run `sizeof/alignof` in Rust to verify locally:

| Struct | Size | Align | File |
|---|---:|---:|---|
| `WayHeader` | 12 | 4 | `street_ways.bin`, `interp_ways.bin` |
| `AddrPoint` | 36 | 4 | `addr_points.bin` |
| `InterpWay` | 24 | 4 | `interp_ways.bin` |
| `AdminPolygon` | 24 | 4 | `admin_polygons.bin` |
| `NodeCoord` | 8 | 4 | `street_nodes.bin`, `admin_vertices.bin`, `interp_nodes.bin`, `wof_countries_vertices.bin` |
| `PlacePoint` | 16 | 4 | `place_points.bin` |
| `PoiPoint` | 24 | 4 | `poi_points.bin` |
| `I18nRecord` | 16 | 4 | `i18n_names.bin` |
| `WofCountry` | 24 | 4 | `wof_countries.bin` |
| `AutocompleteEntry` | 20 | 4 | `fst_<cc>.bin`, `fst_unified.bin` |
| `AddressPoint` (G-NAF / OA) | 28 | 4 | `gnaf_points.bin`, `oa_<cc>_points.bin` |
| `RawEntry` (postcode lookup) | 16 | 8 | `postcode_lookup.bin` |
| `GeoCell` (serialized fields) | 20 | n/a | `geo_cells.bin` |
| `CellOffset` (serialized fields) | 12 | n/a | `admin_cells.bin`, `place_cells.bin`, `poi_cells.bin`, G-NAF/OA cells |

All alignments ≤ 8 and every primitive field is a fixed-width
integer or float. No `usize`, no pointers. This is what makes
cross-architecture portability work.

`manifest_reverse.json` stores the C++ record sizes, field offsets,
byte order, and byte lengths of the reverse-index files.
Schema version 2 added `AddrPoint.postcode_id` at byte offset 28.
Schema version 3 reserves string-pool offset zero for the empty string;
the reader checks both the version and the first byte of `strings.bin`.
Both changes require a fresh reverse build.
The Rust reader compares them with its compiled layouts and current
file lengths before mapping records.
Each G-NAF and OpenAddresses shard has a matching
`<prefix>_points.schema.json` with the same checks for `AddressPoint`.
Indexes built before these schemas were added must be rebuilt; the
reader will reject them rather than guess at the byte layout.

## Core files (always present)

### `strings.bin`

NUL-terminated string pool, byte addressed. Offset 0 is the empty
string (single `0x00`).

```
[0x00, name0_bytes..., 0x00, name1_bytes..., 0x00, ...]
```

### `geo_cells.bin` + `street_entries.bin` + `street_ways.bin` + `street_nodes.bin`

Street index (S2-cell-partitioned over OSM `highway=*` ways with
explicit `name=` tags). `geo_cells.bin` is the cell → entries map
shared with addr points and interpolation (combined because they
all geocode at street-level cells).

**`WayHeader` — `street_ways.bin`**:
```rust
#[repr(C)]
pub struct WayHeader {
    pub node_offset: u32,   // index into street_nodes.bin
    pub node_count: u8,     // how many NodeCoord entries follow
    pub name_id: u32,       // offset into strings.bin
}
```

**`NodeCoord` — `street_nodes.bin`**:
```rust
#[repr(C)]
pub struct NodeCoord {
    pub lat: f32,   // WGS-84 latitude in degrees
    pub lng: f32,   // WGS-84 longitude in degrees
}
```

### `admin_cells.bin` + `admin_entries.bin` + `admin_polygons.bin` + `admin_vertices.bin`

Administrative boundary index.

**`AdminPolygon` — `admin_polygons.bin`**:
```rust
#[repr(C)]
pub struct AdminPolygon {
    pub vertex_offset: u32,   // into admin_vertices.bin
    pub vertex_count: u16,    // number of NodeCoords in the outer ring
    pub name_id: u32,         // into strings.bin
    pub admin_level: u8,      // OSM admin_level (2–10; 11 for postal_code boundaries)
    pub importance: u8,       // 0..255 prominence (population log + wikidata + wikipedia)
    pub area: f32,            // shoelace area in deg² (not geodesic)
    pub country_code: u16,    // packed ISO 3166-1 alpha-2, 0 if unset
}
```

`importance` mirrors the Nominatim-style score `PlacePoint` and
`PoiPoint` carry. The forward-search ranker uses it as an additive
bonus so a major city represented as an admin polygon (Arlington
County VA, Münster NRW) wins same-name disambiguation against tiny
`place=town` siblings — those previously beat it because admin docs
entered the ranker with `importance=0`. The byte was carved out of
the original 3-byte padding slot after `admin_level`; the on-disk
struct stays 24 bytes (binary-format-stable).

Vertices in `admin_vertices.bin` are densely packed `NodeCoord`
records; each polygon's ring is `&admin_vertices[vertex_offset..vertex_offset+vertex_count]`.

**Note**: `vertex_count` is `u16` (max 65 535). Very large country
polygons get Douglas-Peucker simplified in the C++ builder before
they're written. The cap is graduated by `admin_level` and (at
level 2) by approximate country area — small countries like
Belgium, Netherlands, Switzerland, Luxembourg get 32 000 vertices,
mid-size countries (Germany, Italy, GB, Japan, Poland) 16 000, and
the rest 8 000. Vertex density (vertices per km of border) is what
actually matters at international borders, and a flat 8 000 cap
shortchanges small countries with disproportionate per-km border
length. The WoF importer handles the same concern differently
(see `wof_countries.bin` below: `vertex_count` widened to `u32`).

### `addr_points.bin` + `addr_entries.bin`

OSM `addr:housenumber` index, also keyed off `geo_cells.bin` at
street-cell level.

**`AddrPoint` — `addr_points.bin`**:
```rust
#[repr(C)]
pub struct AddrPoint {
    pub lat: f32,
    pub lng: f32,
    pub housenumber_id: u32,   // into strings.bin
    pub street_or_place_id: u32, // street/place name in strings.bin
    pub unit_id: u32,          // optional unit in strings.bin
    pub floor_id: u32,         // optional floor in strings.bin
    pub parent_place_id: u32,  // optional tagged locality in strings.bin
    pub postcode_id: u32,      // optional addr:postcode in strings.bin
    pub flags: u8,             // addr:place / housename bits
    pub _pad: [u8; 3],
}
```

Address points store their own street or place name, house number,
optional locality, and postcode. Forward search indexes these fields
at the point's coordinates. The reverse reader checks the manifest's
record size, field offsets, and file length before mapping this file.

### `interp_ways.bin` + `interp_nodes.bin` + `interp_entries.bin`

Address interpolation (odd/even ranges along a way).

**`InterpWay` — `interp_ways.bin`**:
```rust
#[repr(C)]
pub struct InterpWay {
    pub node_offset: u32,    // into interp_nodes.bin
    pub node_count: u8,
    pub street_id: u32,      // join into street_ways.bin
    pub start_number: u32,   // first housenumber in the range
    pub end_number: u32,     // last housenumber
    pub interpolation: u8,   // 1 = odd, 2 = even, 3 = all
}
```

### `place_cells.bin` + `place_entries.bin` + `place_points.bin`

`place=*` nodes/areas (cities, towns, suburbs, hamlets). Optional
— absent in very old indexes.

**`PlacePoint` — `place_points.bin`**:
```rust
#[repr(C)]
pub struct PlacePoint {
    pub lat: f32,
    pub lng: f32,
    pub name_id: u32,     // into strings.bin
    pub rank: u8,         // Nominatim-style address rank
                          //   16 = city/town/village
                          //   19 = suburb
                          //   20 = hamlet
    _pad: [u8; 3],        // alignment pad
}
```

### `i18n_names.bin`

Localized names (`name:<lang>` OSM tags). Sorted by
`(entity_type, entity_id, lang_code)` for binary search.

**`I18nRecord` — `i18n_names.bin`**:
```rust
#[repr(C)]
pub struct I18nRecord {
    pub entity_type: u8,   // 0 = admin polygon, 1 = place point
    pub _pad0: u8,
    pub lang_code: u16,    // packed 2-char ASCII: 'a' | ('b' << 8)
    pub entity_id: u32,    // index into admin_polygons or place_points
    pub name_id: u32,      // into strings.bin
    pub _pad1: u32,
}
```

Lookup: `I18nNames::lookup(entity_type, entity_id, lang_code)`
binary-searches the sorted records and returns `Option<u32>` name_id.

## Optional enrichment files

### G-NAF (AU authoritative addresses)

`gnaf_points.bin` + `gnaf_cells.bin` + `gnaf_entries.bin` +
`gnaf_strings.bin`. Identical layout to the generic
`AddressPointIndex` template.

**`AddressPoint` — `gnaf_points.bin`** (same shape for `oa_<cc>_points.bin`):
```rust
#[repr(C)]
pub struct AddressPoint {
    pub lat: f32,
    pub lng: f32,
    pub housenumber_id: u32,
    pub street_id: u32,       // into gnaf_strings.bin (*not* street_ways)
    pub locality_id: u32,
    pub postcode_id: u32,
    pub unit_id: u32,       // unit / flat number, 0 when absent
}
```

**Reader**: `Gnaf::find_by_housenumber`, `Gnaf::find_nearest` in
`server/src/gnaf.rs`; the generic `AddressPointIndex` in
`server/src/address_points.rs`.

### OpenAddresses (per-country)

One set of files per ISO 3166-1 alpha-2 code. File prefix is
`oa_<cc>_`, and the struct is exactly `AddressPoint` (same as
G-NAF). The runtime treats OA as per-country; G-NAF is AU-only.

**Reader**: `OpenAddresses::find_by_housenumber` in
`server/src/openaddresses.rs`. Has its own `has_country` /
`countries` accessors for the `/healthz/indexes` endpoint.

### Postcode lookup

Hash-based lookup from `(state, locality)` to postcode. AU-specific
today (sourced from G-NAF) but the format is reusable.

**`RawEntry` — `postcode_lookup.bin`**:
```rust
#[repr(C)]
pub struct RawEntry {
    pub key_hash: u64,         // siphash of (normalized_state, normalized_locality)
    pub postcode_offset: u32,  // into postcode_lookup_strings.bin
    pub _pad: u32,
}
```

**Gotcha**: the original `(state, locality)` strings aren't stored —
only the `u64` hash. Lossy by design. The `tools/index-dumper`
deliberately skips this file because it can't reconstruct human-
readable rows. See `docs/inspection/README.md`.

### WoF country fallback

Plugs the hole where Geofabrik country extracts omit their own
`admin_level=2` relation (UK, US). Written by `tools/wof-importer`
which reads Who's on First admin SQLite files, extracts
`placetype='country'` polygons, Douglas-Peucker simplifies, dedupes.

Separate WoF postalcode SQLite files are exported as `wof_postcodes.tsv`,
an input to `build-autocomplete-fst`. Its first line is exactly
`#wof-postcodes-v1\tcountry\tpostcode\tlatitude\tlongitude`; the FST
builder refuses any other header. Rows have an ISO alpha-2 country code,
source postcode, and WGS84 centroid. Records with invalid coordinates
or (0,0) are omitted. The TSV is not opened by the server; the resulting
postcodes live in the existing FST files.

**`WofCountry` — `wof_countries.bin`**:
```rust
#[repr(C)]
pub struct WofCountry {
    pub vertex_offset: u32,
    pub vertex_count: u32,     // WIDENED to u32; UK mainland has ~297k vertices
    pub name_id: u32,          // into wof_countries_strings.bin
    pub admin_level: u8,       // always 2
    pub area: f32,
    pub country_code: u16,     // packed alpha-2
}
```

**Key divergence from `AdminPolygon`**: `vertex_count` is `u32` not
`u16`. Discovered the hard way: the UK mainland WoF polygon wraps
a u16, producing `34428 = 297572 mod 65536`. The server's loader
reads the file correctly but PIP was broken because the ring was
truncated to one quarter of its vertices. Kept as a separate file
(not merged into `admin_polygons.bin`) to avoid widening the OSM
admin struct just for this edge case.

**Reader**: `WofCountries::find_country(lat, lng)` in
`server/src/wof_countries.rs`. Pre-computed sorted bboxes per
polygon for fast no-PIP culling; a full-scan fallback when OSM
admin produced no country_code.

### Autocomplete FST (per-country + unified)

Two layouts, both present on modern builds. The server prefers
the unified FST when both are available.

**Per-country**: `fst_<cc>.fst` (serialized `fst::Map`) + `fst_<cc>.bin`
(parallel array of `AutocompleteEntry` records) + `fst_<cc>_strings.bin`.

**Unified**: `fst_unified.fst` (keys are `<cc[0]><cc[1]><name>`) +
`fst_unified.bin` (shared entries across all countries) +
`fst_unified_strings.bin`.

**`AutocompleteEntry` — `fst_<cc>.bin` / `fst_unified.bin`**:
```rust
#[repr(C)]
pub struct AutocompleteEntry {
    pub lat: f32,
    pub lng: f32,
    pub name_offset: u32,     // into fst_<cc>_strings.bin
    pub suburb_offset: u32,   // into same strings pool; 0 = no suburb
    pub kind: u8,             // 1 = place, 2 = street
    pub rank: u8,             // inherited from PlacePoint.rank or 26 for streets
    pub pad: [u8; 2],
}
```

The FST itself stores `name → u32` mappings where the value is an
index into the parallel `.bin` records. Country scoping on the
unified FST is done at query time via the custom
`CountryPrefixAutomaton` in `server/src/autocomplete.rs`.

### Forward (tantivy) index

`tantivy/` directory. **Not our format** — this is the standard
tantivy on-disk layout (segment files, posting lists, fast fields,
metadata). Not addressed by this doc; see the tantivy docs upstream
if you need to inspect the raw files.

Depending on `build-forward-index`'s flags, you either get one
flat `tantivy/` (monolithic) or `tantivy_<cc>/` per-country.

### MaxMind GeoLite2

`GeoLite2-City.mmdb`. MaxMind binary format, not ours. Loaded via
the `maxminddb` crate.

## Runtime-only enrichments (not on disk)

Some response fields are computed at query time and therefore have
no corresponding `.bin` file. They're called out here so readers
auditing "what's in the index directory?" don't go looking for a
file that doesn't exist.

### H3 cell IDs

When a request carries `h3_res=<comma-separated resolutions>` (0–15,
max 4), the server converts the response coord into an `h3` map
`{"<resolution>": "<15-char hex cell id>"}` via the `h3o` crate.
Each cell takes ~tens of nanoseconds to compute, so persisting them
on disk would cost more in complexity than it saves. See
`server/src/h3_cell.rs` for the helper module and the reasoning.

The same param + response shape is mirrored on the gRPC proto as
`repeated uint32 h3_res` on requests and `map<uint32, string> h3`
on responses.

## File relationships

```
strings.bin
    ▲  name_id / housenumber_id / suburb_offset / etc.
    │
    ├── admin_polygons.bin ──▶ admin_vertices.bin (vertex_offset + vertex_count)
    │         ▲
    │         │ (S2 spatial index)
    │         └── admin_cells.bin ──▶ admin_entries.bin
    │
    ├── street_ways.bin ──▶ street_nodes.bin (node_offset + node_count)
    │         ▲   ▲
    │         │   │ (S2 spatial index, shared with addr + interp)
    │         │   └── geo_cells.bin ──▶ street_entries.bin
    │         │
    │         └── addr_points.bin.street_id (join)
    │                    ▲
    │                    └── geo_cells.bin ──▶ addr_entries.bin
    │
    ├── place_points.bin
    │         ▲
    │         └── place_cells.bin ──▶ place_entries.bin
    │
    ├── interp_ways.bin ──▶ interp_nodes.bin
    │         │    ▲
    │         │    └── geo_cells.bin ──▶ interp_entries.bin
    │         └──▶ street_ways.bin (street_id join)
    │
    └── i18n_names.bin
              │
              ├──▶ admin_polygons.bin (entity_id when entity_type=0)
              └──▶ place_points.bin   (entity_id when entity_type=1)

gnaf_strings.bin ◀── gnaf_points.bin ──▶ gnaf_cells.bin ──▶ gnaf_entries.bin
oa_<cc>_strings.bin ◀── oa_<cc>_points.bin ──▶ oa_<cc>_cells.bin ──▶ oa_<cc>_entries.bin
postcode_lookup_strings.bin ◀── postcode_lookup.bin

wof_countries_strings.bin ◀── wof_countries.bin ──▶ wof_countries_vertices.bin

fst_<cc>_strings.bin ◀── fst_<cc>.bin ◀── fst_<cc>.fst
fst_unified_strings.bin ◀── fst_unified.bin ◀── fst_unified.fst
```

## Writers (who emits what)

| Files | Writer |
|---|---|
| `strings.bin`, `geo_cells.bin`, `street_*`, `addr_*`, `interp_*`, `admin_*`, `place_*`, `i18n_names.bin` | C++ `builder/src/build_index.cpp` |
| `gnaf_*.bin` | Rust `server/src/bin/build_gnaf_index.rs` |
| `oa_*.bin` | Rust `server/src/bin/build_openaddresses_index.rs` |
| `postcode_lookup{,_strings}.bin` | Rust `server/src/bin/build_postcode_lookup.rs` |
| `fst_<cc>*`, `fst_unified*` | Rust `server/src/bin/build_autocomplete_fst.rs` |
| `tantivy/` | Rust `server/src/bin/build_forward_index.rs` |
| `wof_countries*` | Rust `tools/wof-importer/src/main.rs` |

## Versioning and compatibility rules

The raw `.bin` files have no inline header. The reverse index requires
`manifest_reverse.json`, and each G-NAF/OpenAddresses address-point shard
requires its `*_points.schema.json` sidecar. The reader compares version,
field offsets, byte order, and file lengths before mapping those files.
These checks detect layout drift and missing or truncated files; they do not
verify the contents of same-length files. Compatibility rules:

**Stable (backwards-compatible to change)**:
- Adding optional new files: allowed. Readers use
  `Option::open() -> Ok(None)` when the file is absent.
- Adding new admin levels, new FST countries: allowed.

**Breaking (requires rebuild + coordinated deploy)**:
- Changing any on-disk struct field order, width, or padding.
- Changing which strings pool a `name_id` refers to.
- Changing the S2 cell level.
- Widening a `u16` field to `u32` (see the WoF `vertex_count`
  incident — we picked a separate file rather than break the
  `AdminPolygon` shape).

When making a breaking change:

1. **Match the C++ struct layout exactly** with the Rust
   `#[repr(C)]` declaration. Run `server/tests/struct_layout.rs` to
   assert sizes — this test exists specifically to catch
   accidental padding drift.
2. Document the change in git history with a clear "index format
   break" marker in the commit message so operators know rolling
   deploys won't work.
3. Consider whether to add a new optional file instead of widening
   an existing struct — optional files are a zero-friction
   evolution path.

## Inspection tooling

- **`tools/index-dumper`** (`make inspect-dump`) — dumps all
  OSM-derived tables to CSV alongside the index. DuckDB recipes in
  `docs/inspection/README.md`.
- **`server/tests/struct_layout.rs`** — pins the expected
  sizeof/alignof for every on-disk struct. Run `cargo test
  struct_layout` after any `#[repr(C)]` edit.
- **`server/tests/wof_probe.rs`** — sanity-checks WoF fallback
  resolution against the built index.
- **`tools/regression-runner`** (`make regression-au` etc.) —
  black-box HTTP assertions. Useful when a format change might
  have silently corrupted query results.

## Evolution history

Notable format changes since the project forked, newest first:

- **2026-04 (WoF fallback)** — added `wof_countries*.bin` as a
  side-channel admin fallback. `WofCountry.vertex_count` is
  `u32`, diverging from `AdminPolygon.vertex_count: u16`.
- **2026-04 (unified FST)** — added `fst_unified.{fst,bin,strings.bin}`
  alongside existing per-country files. Runtime prefers unified
  when present.
- **2026-03 (i18n)** — added `i18n_names.bin`.
- **2026-03 (G-NAF authoritative + OpenAddresses)** — added
  `gnaf_*`, `oa_*`, and `postcode_lookup*` optional files.
- **Earlier** — base OSM pipeline (streets, admin, places,
  interpolation, addresses).

When making format changes, prepend a new bullet here.
