# Upstream relationship

This project was forked from [traccar/traccar-geocoder](https://github.com/traccar/traccar-geocoder). The upstream repository provides a focused reverse-geocoding service for the Traccar GPS tracking platform; this fork has significantly broadened scope. This document tracks the divergence, what has been contributed back, and how future pulls from upstream should be handled.

## Divergence point

Forked from upstream `main` at commit `46a8d7c` (*"Skip index if exists"*, the latest upstream commit at the time of forking).

All commits after the fork point are local to this project unless explicitly noted as cherry-picked from upstream.

## What's been added to this fork (not in upstream)

### Scope additions — stays here
These are architectural expansions that don't match upstream's mandate as a Traccar component:

- **Forward geocoding** via embedded Tantivy, including per-country indexes, fallback ladder, fuzzy-term fallback, and FST exact-match fast-path.
- **Autocomplete** via per-country FST (`/autocomplete`).
- **Address validation** endpoint (`/validate`).
- **IP geocoding** via MaxMind GeoLite2 (`/geocode/ip`).
- **G-NAF** (Australia) ingestion — both postcode lookup and full address-point index.
- **OpenAddresses.io** per-country ingestion.
- **Multi-language** reverse geocoding via OSM `name:<lang>` tags.
- **Country-aware admin-level mapping** (Nominatim-style `address-levels.json`).
- **gRPC API** mirroring every REST endpoint.
- **Confidence field** (`exact` / `interpolated` / `fallback`) on all responses.
- **Hot reload** via `ArcSwap` + marker file.
- **Per-country partitioning** for Tantivy, FST, and address-point indexes.
- **Build tooling** — `build-forward-index`, `build-autocomplete-fst`, `build-gnaf-index`, `build-openaddresses-index`, `build-postcode-lookup`.
- **Architecture documentation** — [ARCHITECTURE.md](ARCHITECTURE.md).

### Bug fixes and quality improvements — candidates for upstream PRs
These came out of a focused code review early in the fork's history. They improve correctness, performance, or safety of the existing reverse-geocoding code and are good candidates for upstream contribution:

| Fix | Status | Notes |
|---|---|---|
| Interpolation math used squared distances as linear segment lengths | local | `project_point_on_polyline`; test reproduces the bug explicitly. Fixed with a single-pass linear-length computation. |
| `format_address` allocated 8 `String`s per call via `Vec<String>` + `.join()` | local | Rewrote to a single pre-sized `String` with `write!`. One allocation per call. |
| `seen_streets` dedup was a 64-slot direct-mapped collision table, not a real set | local | Replaced with bounded linear scan in an `ArrayVec`-sized buffer. |
| `point_in_polygon` ran in `f32` — boundary precision issues | local | Inner math now `f64`, storage stays `f32`. f32/f64 reference parity for AU admin polygons. |
| Mmap slice reinterpretation scattered across query methods | local | Consolidated into `as_typed_slice`. |
| Struct layouts between Rust and C++ weren't pinned | local | `struct_layout` tests assert record sizes match the C++ `sizeof`. |

When contributing back upstream, these are discrete changes with accompanying tests — each should be its own PR.

### Things upstream might want to know about
Architectural observations that may inform their roadmap even if they don't take the code:

- `find_admin` is 20–50× more expensive than `query_geo` for urban queries; the cost is O(vertices × candidate polygons). Bounding-box prefilter per polygon + sorting entries by area would cut this substantially.
- `query_geo`'s `seen_streets` dedup materially matters at urban density (our tests measured 1.8× redundant street scans without proper dedup).
- `format_address` allocations show up in p99 end-to-end latency under high QPS.

## Pulling in upstream changes

Upstream `main` is occasionally a source of improvements — dependency bumps, Dockerfile tweaks, new PBF-region support — worth pulling in.

Suggested workflow:

```bash
git remote add upstream https://github.com/traccar/traccar-geocoder
git fetch upstream
git log upstream/main --not main --oneline     # see what's new upstream
git cherry-pick <commit>                       # or merge, depending on scope
```

Merge conflicts will usually land in:

- `entrypoint.sh` — both sides add cases for new indexes
- `Dockerfile` — both sides add builders/binaries
- `builder/src/build_index.cpp` — we've extended tag collection, they may have unrelated changes
- `README.md` — we rewrote structure; theirs will be different

Prefer targeted cherry-picks of upstream bug fixes over large merges to keep the fork's history coherent.

## Contributing bug fixes back

When raising an upstream PR from this fork:

1. Branch off `upstream/main`, not this repo's `main`.
2. Cherry-pick only the relevant commits; strip out scope-addition context.
3. Keep test coverage minimal — the fix + one regression test. Don't drag the full test harness upstream.
4. Reference this fork in the PR body so upstream maintainers can see the broader context if they want.

## License and attribution

The fork remains under Apache License 2.0. Upstream copyright line stays in `LICENSE.txt`; new contributors add their own lines (don't replace). Data-source licences (OSM ODbL, G-NAF CC-BY, OpenAddresses per-source, MaxMind CC-BY-SA) carry through the index to downstream consumers — see the Licence section in [README.md](README.md#license).
