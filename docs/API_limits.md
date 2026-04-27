# API limits

This document lists the per-field length caps and global request-body limit enforced by every public endpoint on both the HTTP/REST and gRPC surfaces.

## Why limits exist

The geocoder is unauthenticated — `README.md` calls this out and recommends deploying behind a network boundary. The caps below provide a second layer of resource protection: a misbehaving (or malicious) caller that gets past the network boundary can't pin a request thread by sending megabytes of text in a query parameter.

Without the caps, a 10 MB `q=` would tokenize linearly through `tokenize_user_input` and produce a Tantivy `BooleanQuery` with thousands of clauses — the request still completes, but compute cost scales with input size. The caps below are sized so legitimate inputs always fit and pathological inputs are rejected with a 400 (REST) or `InvalidArgument` (gRPC) before any expensive work runs.

## Per-field caps

Both the REST and gRPC handlers read these constants from `server/src/limits.rs`. Drift between the two surfaces is impossible — they share the same source. Limits are measured in **bytes** (UTF-8): a multi-byte character counts as more than one. The unit matches what the downstream tokenisers see.

| Field | Cap (bytes) | Endpoints | Justification |
|---|--:|---|---|
| `q` (search) | 512 | `/search`, gRPC `Search` | Pelias caps at ~256; we double for freeform queries with POI hints / multi-locality context. |
| `q` (autocomplete) | 128 | `/autocomplete`, gRPC `Autocomplete` | Typeahead — real users rarely type more than ~50 chars before pressing Enter. |
| `street`, `city`, `state` | 256 each | `/search`, `/validate`, gRPC equivalents | OSM canonical names are <100 chars even for the longest cities/streets. |
| `housenumber` | 32 | `/search`, `/validate` | Most are 1–10 chars; suffixed/fractional forms (`123A`, `12-1/2`) up to ~25. |
| `postcode` | 16 | `/validate` | Longest real systems (GB `SW1A 1AA`, BR with hyphen) are ≤10. |
| `country_code` (single) | 2 | `/autocomplete` | Hard ISO-3166-1 alpha-2 constraint. |
| `country_code` (multi, comma-separated) | 64 | `/search`, `/validate` | Radar-style multi-country filter (`US,CA,MX,…`); 64 fits ~21 codes. |
| `lang` | 16 | `/reverse` | BCP-47 with subtags reaches `zh-Hant-TW` (10); 16 covers everything we'd see. |
| `ip` | 64 | `/geocode/ip` | IPv6 with zone id (`fe80::1%eth0`) is the longest realistic form. |

## Global request body limit

| Layer | Limit | Notes |
|---|--:|---|
| REST (axum) | **64 KiB** | `axum::extract::DefaultBodyLimit::max(64 * 1024)`. Geocoder endpoints are GET-only today (URL-bounded by upstream LBs to ~8 KB) — this is belt-and-braces against any future POST endpoint. |
| gRPC (tonic) | 4 MiB | tonic's default `max_decoding_message_size`. Per-field caps render this redundant; the inner messages always fit within the per-field limits well below 4 MiB. |

## Error responses

### REST

A request that exceeds any cap returns:

```
HTTP/1.1 400 Bad Request
Content-Type: text/plain; charset=utf-8

q: input is 1024 bytes; max allowed is 512
```

The response body names the offending field and reports the actual byte length so the caller can see exactly what to trim.

### gRPC

The handler returns `tonic::Status` with `Code::InvalidArgument` and the same human-readable message:

```
status: 3, message: "q: input is 1024 bytes; max allowed is 512"
```

(`Code::InvalidArgument` = 3 in the gRPC code numbering.)

## Inputs at exactly the cap

The check is `value.len() > max`, so values of exactly the cap pass. The integration test `search_at_cap_boundary_passes_length_check` (in `server/tests/length_caps.rs`) pins this so the boundary can't drift to off-by-one.

## How the caps are tested

- **Unit:** `server/src/limits.rs` has 3 tests covering boundary, error message format, and UTF-8 byte semantics.
- **gRPC integration:** `server/tests/length_caps.rs` has 19 tests covering every (handler, field) wiring point — `Search` × {q, street, city, state, housenumber, country_code}, `Validate` × {street, city, state, housenumber, postcode, country_code}, `Autocomplete` × {q, country_code}, `Reverse.lang`, `IpGeocode.ip`, plus boundary tests asserting inputs of exactly the cap pass. Skipped automatically when `GEOCODER_INDEX_DIR` is not set (the service constructor needs a real index).
- **REST shape:** the helper (`check_text` in `server/src/main.rs`) is one line that wraps `query_server::limits::check`. It's exercised end-to-end at deploy via the existing smoke tests and the regression suite.
- **Constants floor:** `cap_constants_above_realistic_lower_bounds` in `length_caps.rs` asserts each constant stays above a realistic minimum (e.g. `SEARCH_Q ≥ 256`, `IP ≥ 45`, `POSTCODE ≥ 10`). Independent of any service state — runs on every CI build to catch a future tightening that would break legitimate clients.

## Adjusting the limits

If a deployment hits a cap legitimately, edit the constant in `server/src/limits.rs` and re-deploy. There is no runtime knob — these are compile-time constants by design, since:

1. Operators tend to want the same cap on every node.
2. A runtime knob would require either a config-file watcher (more moving parts) or a restart (no benefit over a recompile).
3. The caps are part of the API contract; advertising them as compile-time pins them to the binary version.

For a per-deployment override, fork or vendor the crate. We will accept upstream PRs that justify a higher (or lower) default cap with a real-world example — see the `STRUCTURED_FIELD ≥ 100` comment in `length_caps.rs` for the kind of justification the floor test wants.
