# Nominatim BDD scenario coverage

Mapping of every scenario in `osm-search/Nominatim`'s
`test/bdd/features/` tree to either a translated case in our
`tests/regression/corpora/nominatim-bdd-au.json` or a documented
skip reason. 207 API-level scenarios + ~50 db-level scenarios in
the source tree; we translate the subset that exercises behaviour
our API can actually be tested against.

The translations all use AU inputs we index — Sydney, Melbourne,
Baulkham Hills, etc. — rather than Nominatim's original
Liechtenstein/Vaduz test data. Each translated case carries a
`notes` field citing its source feature file + scenario name.

## Skip categories

Scenarios are skipped when they test something our server doesn't
support. Organised by category:

| Skip reason | Count (approx) | Why not translated |
|---|---:|---|
| Response format-specific (XML, geojson, geocodejson, jsonv2) | ~40 | We emit one JSON shape; assertions against Nominatim's XML/geojson/geocodejson element names don't map. |
| `/details` endpoint | 26 | We don't expose a per-OSM-entity details endpoint. |
| `/lookup` endpoint | 6 | We don't expose lookup-by-OSM-id. |
| `viewbox` + `bounded` params | ~15 | Not implemented in our API. |
| `exclude_place_ids` param | ~8 | Not implemented. |
| `layer` / `layers` param | ~10 | Not implemented (both Nominatim and Pelias have this). |
| `zoom` param for reverse | ~8 | Our reverse doesn't take a zoom level. |
| `debug` param | 2 | No debug-HTML output. |
| `json_callback` (JSONP) | ~6 | Not supported. |
| Class-type `[amenity=xyz]` syntax | ~10 | Our `/search` doesn't parse OSM class-type filters from free text. |
| Viewbox-restricted POI search | 2 | Requires viewbox, not implemented. |
| Coordinate-in-query syntax (`"restaurant near 47.16,9.51"`) | ~4 | Our `/search` doesn't parse lat/lon from free text; `/reverse` is the right path. |
| Category-type narrow scenarios (`bars in X`) | ~5 | Requires class-type inference which we don't do. |
| TIGER housenumbers | 2 | US Census TIGER data not ingested. |
| Country-grid fallback (Nominatim-specific lookup table) | 1 | Their fallback uses an internal country grid; we use WoF polygons instead — separately tested. |
| Nominatim `osm_type` / `osm_id` assertions | ~15 | We don't expose OSM object IDs in our response. |
| Nominatim `place_id`, `licence`, `boundingbox` metadata | ~10 | Our response doesn't carry these fields. |
| Nominatim-specific error shapes (`error+code`, `error+message`) | ~8 | Our error responses use HTTP status + plain-text body; we don't emit structured error objects. |

## Translated coverage

We translated **~45 scenarios** covering:

- **Search — queries.feature** (8): natural-object search, housenumber
  resolution, missing-housenumber fallback, numeric housenumber,
  country-filtered search, whitespace tokenisation, addressdetails
  schema, complete-match ranking.
- **Search — structured.feature** (5): country-only, street+city,
  bad-postcode tolerance, surrounding quotes, rank restriction.
- **Search — postcode.feature** (3): address+postcode combo,
  postcode+country_code filter, US 5+4 ZIP shortening (smoke
  test — we don't have US data loaded by default).
- **Search — language.feature** (3): default language, unknown
  lang fallback, accept-language-like behaviour (smoke test since
  our /search doesn't honour lang= yet; /reverse does).
- **Search — simple.feature** (6): garbage queries across ASCII,
  special characters, Unicode/CJK, long strings, IP-like input,
  punctuation-only. Our tokeniser must not crash on any of these.
- **Reverse — queries.feature** (5): unknown-country fallback,
  numeric housenumber, low-zoom country level, non-alpha
  housenumber, on-street coord.
- **Reverse — language.feature** (3): default lang, `lang=ja`
  param, unknown lang fallback. Honours our `lang=` query param.
- **DB — housenumbers.feature** (3): ASCII digit in both orders,
  missing housenumber returns street.
- **DB — normalization.feature** (3): case-insensitivity,
  diacritic folding, punctuation tolerance.
- **DB — search_simple.feature** (2): place-by-name, state
  disambiguation.
- **DB — reverse.feature** (1): nearest feature returned.
- **DB — japanese.feature** (1): i18n probe.
- **DB — postcodes.feature** (2): reverse returns postcode,
  postcode-in-freeform-query.
- **Status** (1): our `/healthz` equivalent.
- **`/healthz/indexes`** (1): our own endpoint, not in Nominatim,
  but semantically analogous to their `/status` JSON variant.

## What's ignored entirely

The following feature files have **zero** translatable scenarios —
either 100 % format-specific or testing endpoints we don't have:

- `api/details/*.feature` (26 scenarios) — `/details` endpoint.
- `api/lookup/*.feature` (6 scenarios) — `/lookup` endpoint.
- `api/status/*.feature` v1_* scenarios — same as above for JSON
  status metadata.
- `api/reverse/v1_xml.feature`, `v1_json.feature`, `v1_geojson.feature`,
  `v1_geocodejson.feature`, `v1_params.feature` (~50 scenarios) —
  test Nominatim response-format specifics that our API doesn't
  emit.
- `api/search/v1_geocodejson.feature` (~4 scenarios) — same.
- `api/reverse/layers.feature` (6 scenarios) — `layer` param.
- `api/search/params.feature` (~34 scenarios) — mostly
  Nominatim-specific params (`dedupe`, `format`, `email`,
  `featuretype`, `polygon_*`, `extratags`, `namedetails`).
- `osm2pgsql/**/*.feature` — they test Nominatim's PostgreSQL
  import pipeline, not the query API.
- `db/update/**/*.feature` — live-update semantics, requires the
  running PostgreSQL instance to modify the data mid-test. Not
  applicable to our mmap'd format.
- `db/import/**/*.feature` — import-time behaviour, our C++
  builder has its own coverage in `server/tests/struct_layout.rs`
  etc.

## Maintenance

When Nominatim adds new BDD scenarios, the process is:

1. `cd test-data/nominatim && git pull` (or re-run
   `./scripts/fetch-test-data.sh --all-corpora` with cache
   cleared).
2. Diff `test/bdd/features/api/search/queries.feature` et al.
   against the last translation snapshot.
3. For each new scenario: decide translatable vs. skip, add a
   case to `nominatim-bdd-au.json` or a row to the skip-reason
   table above.
4. `make regression-nominatim-au` must pass.

The alternative — maintaining a full Gherkin parser + runner
against our API — is a larger lift we haven't taken.
