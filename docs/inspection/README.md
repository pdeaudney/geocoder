# Inspecting the index with DuckDB

The geocoder's mmap'd `.bin` files aren't human-readable, but each is a
fixed-size C struct array — trivially convertible to CSV. This page
walks through dumping the index and running ad-hoc inspection queries
with DuckDB.

## Why

For debugging data issues (why did this suburb resolve wrong? is admin
coverage sparse here? are we missing an OSM tag?), jumping into SQL
with a columnar engine beats reading Rust one record at a time. DuckDB
is zero-setup, reads CSV directly, and runs typical queries in a
fraction of a second over the 5 M AU rows.

This isn't meant to replace the runtime — it's an offline inspection
workflow for humans who want to cross-check index state against source
data or guide schema decisions.

## Prerequisites

- DuckDB CLI — `brew install duckdb` (macOS) or [install docs](https://duckdb.org/docs/installation/).
- A built index — run a `build-index` pass (see main README).

## Dump the index to CSV

```bash
# Build the dumper once.
cargo build --release -p index-dumper

# Dump — outputs to ./data/index/dump-csv/ by default.
./target/release/index-dumper ./data/index

# Or pick a different output directory.
./target/release/index-dumper ./data/index /tmp/index-dump
```

CSVs produced (AU-only index as a reference; sizes scale with region):

| File                  | Rows     | Bytes   |
|-----------------------|---------:|--------:|
| `streets.csv`         |    1.16 M | ~54 MB  |
| `place_points.csv`    |     12.5 K | 500 KB  |
| `admin_polygons.csv`  |     18.5 K | 600 KB  |
| `addr_points.csv`     |    4.17 M | ~218 MB |
| `i18n_names.csv`      |     15.6 K | 500 KB  |
| `summary.txt`         | — | — |

## Load + query

Run DuckDB inside the dump directory. **Pass `quote='"'` explicitly**
— the auto-sniff misses the quote character when the first 20 k
sampled rows happen to carry no quoted fields, and then rows with
embedded commas (e.g. `"3rd Cutting (vehicle access to beach, north only)"`)
fail the strict parser.

```bash
cd data/index/dump-csv
duckdb
```

Inside the DuckDB shell:

```sql
CREATE VIEW streets AS
  SELECT * FROM read_csv_auto('streets.csv', quote='"');
CREATE VIEW place_points AS
  SELECT * FROM read_csv_auto('place_points.csv', quote='"');
CREATE VIEW admin_polygons AS
  SELECT * FROM read_csv_auto('admin_polygons.csv', quote='"');
CREATE VIEW addr_points AS
  SELECT * FROM read_csv_auto('addr_points.csv', quote='"');
CREATE VIEW i18n_names AS
  SELECT * FROM read_csv_auto('i18n_names.csv', quote='"');
```

Or load the canned bootstrap:

```bash
duckdb -init docs/inspection/bootstrap.sql
```

## Query recipes

### Row counts

```sql
SELECT 'streets' AS tbl, count(*) FROM streets
UNION ALL SELECT 'place_points', count(*) FROM place_points
UNION ALL SELECT 'admin_polygons', count(*) FROM admin_polygons
UNION ALL SELECT 'addr_points', count(*) FROM addr_points
UNION ALL SELECT 'i18n_names', count(*) FROM i18n_names
ORDER BY 2 DESC;
```

### Admin coverage sanity

```sql
-- How many admin polygons per level?
SELECT admin_level, count(*) AS n
FROM admin_polygons
GROUP BY 1 ORDER BY 1;

-- Biggest polygons (likely countries / states).
SELECT name, admin_level, country_code, vertex_count, area_sq_deg
FROM admin_polygons
ORDER BY area_sq_deg DESC
LIMIT 10;

-- Did we drop pastoral-station-sized suburbs?
-- (Admin-level-9 features are AU-specific suburbs — see ARCHITECTURE.md;
-- a huge one means a pastoral station is still tagged as a suburb.)
SELECT name, area_sq_deg, vertex_count
FROM admin_polygons
WHERE admin_level = 9 AND area_sq_deg > 0.5
ORDER BY area_sq_deg DESC;
```

### Street coverage by region

```sql
-- Count streets by midpoint quadrant — cheap coverage histogram.
SELECT
    floor(midpoint_lat / 10) * 10 AS lat_bucket,
    floor(midpoint_lng / 10) * 10 AS lng_bucket,
    count(*) AS streets
FROM streets
WHERE midpoint_lat IS NOT NULL
GROUP BY 1, 2
ORDER BY streets DESC
LIMIT 10;

-- Top 10 most-common street names.
SELECT name, count(*) AS occurrences
FROM streets
GROUP BY 1
ORDER BY occurrences DESC
LIMIT 10;
```

### Address-point density

```sql
-- How many addresses per street, top 20?
SELECT street_name, count(*) AS addr_count
FROM addr_points
GROUP BY 1
ORDER BY addr_count DESC
LIMIT 20;

-- Duplicate housenumbers on the same street (data smell).
SELECT street_name, housenumber, count(*) AS dupes
FROM addr_points
GROUP BY 1, 2
HAVING dupes > 1
ORDER BY dupes DESC
LIMIT 20;
```

### Find the "Martin Place" entries

The forward-search test case in `tests/regression/corpora/au-regression.json`
depends on Sydney's Martin Place being indexed. Cross-check the raw data:

```sql
SELECT name, node_count, midpoint_lat, midpoint_lng
FROM streets
WHERE name ILIKE '%martin place%'
ORDER BY node_count DESC
LIMIT 10;
```

### i18n coverage

```sql
-- Languages most commonly tagged on OSM entities.
SELECT lang, count(*) AS entries
FROM i18n_names
GROUP BY 1
ORDER BY entries DESC
LIMIT 20;

-- Entities that have Japanese, Chinese, and Korean names (tourist markers).
SELECT entity_kind, entity_id, list(lang ORDER BY lang) AS langs
FROM i18n_names
WHERE lang IN ('ja', 'zh', 'ko')
GROUP BY 1, 2
HAVING length(langs) = 3
LIMIT 20;
```

## Exporting to Parquet

If you expect to run inspections repeatedly, convert once to Parquet
for ~10× faster loads:

```sql
COPY streets         TO 'streets.parquet'         (FORMAT PARQUET);
COPY place_points    TO 'place_points.parquet'    (FORMAT PARQUET);
COPY admin_polygons  TO 'admin_polygons.parquet'  (FORMAT PARQUET);
COPY addr_points     TO 'addr_points.parquet'     (FORMAT PARQUET);
COPY i18n_names      TO 'i18n_names.parquet'      (FORMAT PARQUET);
```

Then swap `read_csv_auto(...)` for `read_parquet(...)` — no `quote=`
shenanigans, tiny files, columnar speed.

## What's NOT in the dump

- **Postcode lookup** — the on-disk format stores a `u64` hash of
  `(state, locality)` rather than the strings, so the lookup isn't
  losslessly recoverable from the index. Inspect the G-NAF source
  PSV files directly (`*_LOCALITY_psv.psv`,
  `*_ADDRESS_DETAIL_psv.psv`) if you need that dimension.
- **G-NAF / OpenAddresses address points** — these live in separate
  mmap files (`gnaf_*.bin`, `oa_<cc>_*.bin`) with their own layouts.
  A later iteration of the dumper can add them; the current MVP
  covers the OSM-derived core.
- **FST autocomplete keys** — can be walked via the `fst` crate
  directly; not represented here because the key → payload mapping
  is more useful as a server query than a table.
- **Admin polygon vertices** — dumped as a count only. The full
  vertex list would be ~150 MB for AU and isn't obviously useful for
  SQL-shaped inspection; if you need polygon geometry, export to
  GeoJSON instead.
