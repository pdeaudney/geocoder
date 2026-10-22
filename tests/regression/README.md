# Regression suite

Black-box regression tests for the geocoder. Spins up a live server
against a pre-built index, fires a corpus of JSON-declared cases, and
asserts structured expectations.

## Running

```bash
# Full suite (assumes ./data/index is populated)
make regression-au

# Debug profile — rebuilds faster during iteration
make regression-au-debug

# Skip the cargo build (useful after a manual `cargo build`)
./scripts/run-regression.sh --skip-build

# Filter to a subset
./scripts/run-regression.sh --skip-build -- --filter search-
```

The runner writes a JSON report to
`tests/regression/reports/au-<UTC>.json` for each run
(gitignored; committed corpora are the source of truth).

## House-number retrieval benchmark

Run against an existing per-country forward index:

```bash
make bench-house-numbers INDEX=./data/index SAMPLE=1000
# Small smoke run:
make bench-house-numbers INDEX=./data/index SAMPLE=10
```

This samples 1,000 unique indexed address identities per country by default,
using fixed seeds and random document positions across each shard. It reserves
up to 10% each for units, suffixes, and numeric ranges, filling any shortfall
from the available addresses. Actual strata counts are in the report; this is
not an independently verified or uniformly urban/rural sample.

Every address is searched with structured fields and freeform text. Up to 100
abbreviation variants and 100 single-character deletion typos per country are
also tried. Country is supplied; coordinate bias is never supplied. Range
records are searched as recorded, without inferring individual house locations
from their endpoints.

Each query checks the first result and top five at 25, 50, and 100 metres. The
same result must match house number, normalized street, country, recorded unit
when requested, and postcode when the reference has one. Field-only results
are reported separately. Street normalization uses the existing Pelias field
normalizers; postcodes use the server's display formatting. Missing responses
and HTTP errors count as failures.

Reports go into `tests/regression/reports/house-numbers-<UTC>/`:

- `addresses.json`: sampled references, including coordinates and strata.
- `corpus.json`: requests and ordered checks, runnable with `regression-runner`.
- `results.json`: every case's ordered `check_results` and first result.
- `summary.json`: metrics by country, query variant, and stratum, plus index and
  binary identity. `number_returned` is diagnostic; it does not establish that
  the returned property is correct.
- `failures.json`: all queries that fail the first-result 50-metre check.
- `server.log` and `runner.log`: startup and per-case failures.

This is **retrieval from imported data**, not independent address coverage.
The current `/search` house-number path returns stored address points or street
fallbacks; it does not interpolate house locations. A returned street without a
matched house number never passes an address check.

Controlled suffix, range, unit, and nonexistent-number cases are in
`server/tests/oa_street_search.rs`. The runner's own tests reject mixed-result
field/coordinate matches, wrong house suffixes, missing or wrong units, and
invalid coordinates. Run the benchmark generator/report checks with:

```bash
python3 -m unittest discover -s scripts -p test_bench_house_numbers.py
```

## Combined field and coordinate checks

For optional `result_matches_within.coordinates`, use
`{"lat": -33.73698, "lon": 150.9796, "max_m": 50}`. Existing field-only
checks are unchanged. The runner's aggregate pass count requires every check;
use the benchmark summary for each rank and distance threshold.

## Corpus schema

One file per corpus under `corpora/`:

```json
{
  "name": "au-regression",
  "description": "...",
  "cases": [
    {
      "id": "search-sydney-city",
      "tags": ["search", "place"],
      "request": {
        "method": "GET",
        "path": "/search",
        "query": { "q": "Sydney", "country_code": "au" }
      },
      "expect": {
        "status": 200,
        "checks": [
          { "kind": "json_path_min_count", "path": "results", "min": 1 },
          { "kind": "json_path_contains_ci", "path": "results.0.name", "value": "sydney" },
          {
            "kind": "coord_within_m",
            "lat_path": "results.0.lat",
            "lng_path": "results.0.lon",
            "expected_lat": -33.87,
            "expected_lng": 151.21,
            "max_m": 10000
          }
        ]
      }
    }
  ]
}
```

### Check kinds

| `kind`                    | Asserts                                                             |
|---------------------------|---------------------------------------------------------------------|
| `json_path_equals`        | Value at dot-path equals `value` (deep equality).                   |
| `json_path_contains_ci`   | String at path contains `value` (case-insensitive).                 |
| `json_path_exists`        | Path resolves to a non-null value.                                  |
| `json_path_absent`        | Path missing or null.                                               |
| `json_path_min_count`     | Array at path has ≥ `min` elements.                                 |
| `result_matches_within`   | One result in the first `limit` entries matches every mapped field. |
| `result_absent`           | No result matches every mapped field.                                 |
| `result_coord_within`     | A result in the first `limit` entries is within `max_m` metres.       |
| `result_count`            | Result count satisfies an expression such as `>= 5`.                |
| `coord_within_m`          | Haversine distance between (`lat_path`, `lng_path`) and the given expected lat/lng is ≤ `max_m` metres. |

Pelias converted cases use `result_matches_within` to preserve each case's `priorityThresh`.
The runner counts fully comparable, partial, and unsupported cases separately.
Unsupported requests are skipped; partial cases run only checks for response fields our API exposes.

`corpora/forward-quality-targets.json` keeps the original queries from the
US/CA failure triage but checks real coordinates and stable address fields.
It does not replace the literal Pelias corpus: display spelling differences
remain visible in the Pelias report, while this corpus measures whether the
service found the intended location.

`corpora/reverse-oa-targets.json` checks that an exact OpenAddresses point
beats a farther OSM house for reverse geocoding.

### Path syntax

Dot-notation over JSON. Numeric segments index arrays; strings index
objects. `results.0.address.city` → `root["results"][0]["address"]["city"]`.

## Adding cases

1. Pick a stable, known-good input. Prefer inputs that exercise a
   specific code path (autocomplete fast-path, G-NAF resolution,
   country filter, etc.).
2. Run the query against your local server and copy the real response
   into the expectation. Loosen the tolerances deliberately — e.g.
   coord tolerance in the 1–10 km range for city-centre queries
   (centroid drift between OSM and G-NAF is normal) and sub-km for
   specific address points.
3. Re-run `make regression-au` to confirm green.

If a case fails because the real-world data changed, update the
expectation, don't relax the check kind — preserving the "what is this
case proving" signal is more valuable than a permanently-green test.

## Planned phases

- **MVP (AU)** — this file. One corpus, hand-curated.
- **Pelias acceptance-tests adapter** — schema converter so we can pull
  in Pelias's ~thousand cases per country.
- **OpenAddresses round-trip** — generate cases from OA published data;
  assert geocoder returns coords within tolerance of ground truth.
- **Nominatim BDD translation** — hand-translate Gherkin feature files
  into our schema for edge-case coverage.
- **Worldwide** — more corpora per region, larger fetch script.
