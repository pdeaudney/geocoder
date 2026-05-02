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
| `coord_within_m`          | Haversine distance between (`lat_path`, `lng_path`) and the given expected lat/lng is ≤ `max_m` metres. |

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
