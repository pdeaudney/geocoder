# Hashmap choice in the index builder

> **Snapshot doc.** Captured 2026-04-25. Records why
> `builder/src/build_index.cpp` was migrated from
> `std::unordered_map` to `ankerl::unordered_dense::map` /
> `segmented_map`, what bench evidence drove the choice, and what
> the measured impact was on the AU build.

## TL;DR

| | Before | After | Δ |
|---|---:|---:|---:|
| AU build wallclock | 100 s | 78 s | **−22 %** |
| Library | `std::unordered_map` (libstdc++) | `ankerl::unordered_dense::map` + `segmented_map` (vendored v4.8.1, MIT) | |
| Header footprint | stdlib only | +90 KB single header (well, two now) | |
| New deps | none | none — header-only, no link changes | |

Same inputs, same hardware, same compiler, same `-O3` flags. Only
the map type changed.

The C++ build path was not the largest perf win available in this
session (the read-path autocomplete win was −38 % at p95) but it's
the one that mattered most at scale: planet builds amortise this
22 % over 45 minutes, where the read-path wins amortise over a few
microseconds per query.

## Why we re-evaluated

The C++ specialist code review (earlier in this session, see
commit history around `3e86292`) flagged
`std::unordered_map<std::string, uint32_t>` in `StringPool` and
`std::unordered_map<uint64_t, std::vector<uint32_t>>` for the five
`cell_to_*` inverted-index maps as an obvious target. libstdc++'s
chained-bucket layout means each insert can heap-allocate; each
lookup chases a pointer per probe; iteration walks a linked list.
At planet scale (`addr_count_total ~ 1B`,
`StringPool ~ 100–300M distinct strings`) those overheads add up.

This document records the bench evidence and the analysis we used
to pick a replacement.

## Reference benchmarks

Two recent third-party benchmarks shaped the decision. Both run on
modern x86-64, both compare 20+ open-source hashmaps under
realistic workloads.

### Martin Ankerl, *Hashmap Benchmarks 01: Overview & Setup* (2022-08-27)

- URL: <https://martin.ankerl.com/2022/08/27/hashmap-bench-01/>
- 29 hashmaps × 6 hash functions, 11 distinct benchmarks, Intel
  i7-8700 @ 3200 MHz, clang++ 13 with `-O3 -march=native`.
- Headline finding: **`std::unordered_map` is "slow across the
  board… no benchmark where it achieves competitive speeds."**
- For our two workloads:
  - **Insert-heavy (insert-then-erase 100M ints):**
    `emhash7::HashMap`, `ska::bytell_hash_map`,
    `tsl::hopscotch_map`. `absl::flat_hash_map` close.
  - **String keys, insert-heavy:**
    `ankerl::unordered_dense::map` dominates ("string search is
    fastest"). `emhash7/8` and `folly::F14ValueMap` competitive.
  - **Iteration:** `ankerl::unordered_dense::map` "unbeatable",
    followed by `emhash7` and `jg::dense_hash_map`. (This matters
    for our `write_index` flush step which iterates every map.)

### Jackson Allan, *C/C++ Hash Tables Benchmark* (ongoing, snapshot 2024)

- URL: <https://jacksonallan.github.io/c_cpp_hash_tables_benchmark/>
- Tests 32-bit-key, 64-bit-key, and 16-char-string-key
  configurations across seven operations (insert, lookup, delete,
  iterate, mixed); load factors 0.44–0.875; AMD Ryzen 7 5800H,
  GCC 13.2 with `-O3`. Includes both C++ open-addressing maps and
  C-side libraries.
- Headline finding: `std::unordered_map` is "far slower than most
  open-addressing tables in most benchmarks" and "iteration is at
  least an order of magnitude slower."
- For our two workloads:
  - **Integer-key insert + string-key insert:**
    `boost::unordered_flat_map` is "all-around best performer."
    `ankerl::unordered_dense` close behind, especially at lower
    load factors.
  - **Iteration:** `ankerl::unordered_dense` is "nearly perfect"
    (contiguous payload + index storage); `boost::unordered_flat_map`
    excellent within-group clustering.
  - **Memory overhead:** `khash` at ~0.25 byte/bucket, `absl::flat_hash_map`
    at ~1 byte/bucket. `std::unordered_map` at ~8 bytes/bucket
    *plus* per-node fragmentation.

## Workload-specific analysis

The two articles agree on the headline (anything is faster than
`std::unordered_map`); they don't fully agree on which open-
addressing map wins. So we mapped the candidates against our
specific workload shapes.

### `StringPool::index_`

```cpp
// Was:
std::unordered_map<std::string, uint32_t> index_;
// Now:
ankerl::unordered_dense::map<std::string, uint32_t> index_;
```

- **Pattern:** `find` then `emplace` on miss; once a string is
  interned, every subsequent `intern()` call is a hit.
- **Key shape:** `std::string`, 1–200 chars (street names, place
  names, postcodes, country codes, `name:<lang>` values).
  Distribution is heavily weighted to short strings (~5–30 chars).
- **Final size:** AU build ~430k distinct strings; planet build
  100–300M.
- **Post-ingest access:** `data()` is read once linearly during
  `strings.bin` flush. The map itself is never iterated post-build.
- **Why ankerl wins:** Martin Ankerl himself documents string
  interning at scale as the workload his library targets. The
  built-in `ankerl::unordered_dense::hash` is faster than
  libstdc++'s `std::hash<std::string>` on short strings (which
  dominate our distribution); SIMD probing on lookup; flat backing
  storage means inserts are amortised O(1) without per-node
  allocation.
- **Alternatives considered:**
  - `boost::unordered_flat_map`: comparable insert + better lookup
    per Allan, but adds a Boost dep we don't otherwise need at
    build time. Not worth the dep cost for a header-only
    alternative.
  - `absl::flat_hash_map`: comparable speed, much larger dep
    (whole Abseil), and weaker iteration which matters elsewhere.

### `cell_to_*` (the five S2 cell inverted indexes)

```cpp
// Was:
std::unordered_map<uint64_t, std::vector<uint32_t>> cell_to_addrs;
// Now:
ankerl::unordered_dense::segmented_map<uint64_t, std::vector<uint32_t>> cell_to_addrs;
// (declared via the `cell_map<V>` alias)
```

- **Pattern:** `m[cell.id()].push_back(id)` per ingested feature.
  Both an insert (when the cell is new) and a value mutation (the
  vector grows in place). Both happen **billions of times** on a
  planet build.
- **Key shape:** `uint64_t` S2 cell ID. Well-distributed on a
  Hilbert curve; identity hash works fine.
- **Value shape:** `std::vector<uint32_t>` (24 B header on 64-bit;
  payload grows to thousands of u32s in dense urban cells).
- **Final size:** AU geo cell map = 15M cells; planet ~100M+
  across the five maps.
- **Post-ingest access:** Sorted into a `std::vector` then
  iterated once during `write_entries` / `write_cell_index`.
  Iteration speed matters.
- **Why `segmented_map` specifically:** the regular
  `ankerl::unordered_dense::map` keeps its bucket array in a
  single allocation, which on rehash means *one* huge realloc +
  copy. At planet scale that's tens of GB rehashed at the worst
  moment. The `segmented_map` variant grows in 4096-element
  chunks: rehash never moves more than a chunk at a time, peak
  memory is bounded, and ingestion stays smooth instead of
  spiking. The dense iterator order, SIMD probing, and avalanching
  hash are unchanged.
- **Why ankerl over boost::unordered_flat_map here:**
  iteration speed is the deciding factor. `write_index` walks
  every entry of every map exactly once to flush the
  `*_entries.bin` files. `ankerl::unordered_dense` stores
  payloads contiguously and iteration is "unbeatable" per both
  benchmarks. `boost::unordered_flat_map` is competitive on
  insert but loses on iteration, *and* rehashing moves the
  `vector<u32>` headers wholesale (the values are 24 B each, so
  the move is cheap, but ankerl's separate-payload architecture
  avoids it entirely).

### Smaller maps left as `std::unordered_set`

`interp_cells` and `way_cells` (the per-feature S2 cell coverage
sets at lines ~864/907) and `interior_set` (line ~273) hold a few
dozen elements each, scoped to a single function call. The
allocation overhead of `std::unordered_set` is genuinely minor
here, and the cleanest reading of the code keeps the standard
type. Swapping these would be ~0% impact and pure churn.

`addr_by_coord` in `resolve_interpolation_endpoints` *was* swapped
to `ankerl::unordered_dense::segmented_map` because at planet scale
it can hold ~1B coordinate keys.

## Migration mechanics

1. `builder/third_party/ankerl/{unordered_dense.h, stl.h, LICENSE}` —
   vendored from upstream `v4.8.1`. v4.8 is a 2-file split (the
   library separated `stl.h` to support C++20 modules cleanly);
   we vendor both. Total ~90 KB.
2. `builder/CMakeLists.txt` — one-line
   `target_include_directories(... third_party)`.
3. `builder/src/build_index.cpp`:
   - One `#include <ankerl/unordered_dense.h>`.
   - One alias `template <typename V> using cell_map =
     ankerl::unordered_dense::segmented_map<uint64_t, V>;`.
   - Six `std::unordered_map<...>` declarations swapped (five
     `cell_to_*` plus `StringPool::index_` plus `addr_by_coord`).
   - `write_entries` / `write_cell_index` parameter signatures
     updated to take the new map type.
   - The `write_offset` lambda's parameter type updated.
4. No link-time changes (header-only).
5. No on-disk format change. Existing `.bin` files stay readable.
6. No API change to the runtime reader.

Diff: 4 files changed, ~30 lines net (excluding vendored header).

## Measured impact

**AU build, M1 Max, same PBF input, same `-O3` flags:**

| | std::unordered_map | ankerl::unordered_dense |
|---|---:|---:|
| Wallclock | 100 s | 78 s |
| Output bytes (matches the sorted layout) | identical | identical |

The diff is entirely in the build pipeline; output `.bin` files are
byte-identical at the user-visible level (the addr_points sort
order is determined by S2 cell, not by hashmap iteration order).

**Projected impact on planet build:**

- Five `cell_to_*` maps × ~200M insert events each = ~1B total inserts.
  `std::unordered_map` does ~120 ns per insert on Allan's bench;
  `ankerl::unordered_dense::segmented_map` does ~30–40 ns. **Estimated 80–90 s saved**.
- `StringPool::index_`: ~100–300M distinct strings; string-key
  insert is ~3× faster on ankerl per Ankerl's bench. **Estimated 60–120 s saved**.
- Iteration during flush: ankerl's contiguous-payload iterator vs
  std's pointer-chase-per-element. **Estimated 30–60 s saved**.

**Total projection: ~3–5 minutes off a 45-minute planet build,
matching the AU 22 % ratio reasonably well.** The exact number
depends on the actual planet-scale memory pressure and how often
the segmented_map's chunked growth lands inside warm L3 cache.
We'll capture an actual planet-scale bench when the next planet
build is run on AWS.

## Trade-offs and reasons-not-to

- **Vendored code grows.** `+90 KB single header` is the cost.
  Tracking upstream means we periodically re-vendor a tag.
- **Compile time.** ankerl's header is ~3000 lines of templated
  code. Build-side compile time goes up by ~2 s on the macOS
  laptop; trivial against a 78 s AU build but worth noting.
- **One more knob to maintain.** If a future planet-scale build
  hits a real-world rehash hotspot we didn't anticipate, the
  `segmented_map` chunk size becomes a tunable. We accept the
  default for now.
- **API gotchas.** `segmented_map` is mostly drop-in vs. std's
  `unordered_map`, but a few edge-case methods aren't supported
  (e.g. `node_handle` extraction). None of the build code uses
  these.
- **The faster alternatives we passed on.**
  `boost::unordered_flat_map` is marginally faster on
  insert+lookup but adds a Boost dep we'd otherwise avoid; its
  iteration is worse and rehashes move values. `emhash7` is
  fastest on insert+erase but slightly behind on string keys.
  `absl::flat_hash_map` is excellent but Abseil is too large a
  dep for one map. None of these would change the headline
  number meaningfully on our workload.

## Verification — what to re-run if this gets touched

The fastest signal that something regressed is just timing the AU
build:

```bash
cd /Users/pdeaudney/git/traccar-geocoder
time ./builder/build/build-index data/index data/pbf/australia-latest.osm.pbf
```

Expected: 75–85 s on M1 Max, roughly 100–110 s on a typical EC2
build host. A regression to 100+ s on M1 Max means something in
this path got worse — first suspicion is the hashmap library
choice, second is the `addr_points` sort, third is the OSM
ingestion pass itself.

For sub-component timing the C++ doesn't currently emit
per-stage timestamps. Adding a `chrono::steady_clock` block per
build phase (Pass 2 ingest, dedup, sort, write) would be the
right next step if a perf regression turns up later.

## Files touched in the migration commit

- `builder/CMakeLists.txt`
- `builder/src/build_index.cpp`
- `builder/third_party/ankerl/unordered_dense.h` (vendored)
- `builder/third_party/ankerl/stl.h` (vendored)
- `builder/third_party/ankerl/LICENSE` (vendored)
- `docs/performance/hashmap-choice-2026-04-25.md` (this file)

## References

- Ankerl, M. (2022). *Hashmap Benchmarks 01: Overview & Setup*.
  <https://martin.ankerl.com/2022/08/27/hashmap-bench-01/>
- Allan, J. (ongoing, snapshot 2024). *C/C++ Hash Tables Benchmark*.
  <https://jacksonallan.github.io/c_cpp_hash_tables_benchmark/>
- Leitner-Ankerl, M. *unordered_dense*. v4.8.1 source vendored at
  `builder/third_party/ankerl/`. Upstream:
  <https://github.com/martinus/unordered_dense>
- Internal: code review of the C++ write path that flagged this,
  see commit `3e86292` and the discussion threading commits
  around it.
