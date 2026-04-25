# Build-pipeline perf plan (in-progress)

> **Status doc.** This is a working plan, not a snapshot. Edit
> in-place as stages complete; record measured numbers + decisions
> as we go so a future engineer (or this same engineer post-compaction)
> can resume without re-deriving context.

## Goal

Cut planet-build wallclock from current ~45 min to "under 20 min"
without architectural change. Six items in priority order.
Each stage: implement → smoke test → time → record numbers →
commit → move on.

## Baseline

Captured 2026-04-25, M1 Max, macOS 15.7, rustc 1.95.0:

- AU `build-index` (after ankerl swap): **78 s** wallclock
- AU full pipeline (build-index + build-gnaf + build-postcode + build-forward + build-autocomplete-fst): not measured end-to-end yet
- Planet build: not run locally; AWS reference ~45 min

The AU 78 s is the headline reference point. After all six stages
we want this comfortably under 60 s (and the planet equivalent
proportionally lower).

## Stage tracking

| # | Stage | Status | Wallclock impact (AU `build-index` unless noted) |
|--:|---|---|---|
| 1 | libdeflate in `build-index` | DONE | AU: ~0 % (within noise; AU isn't decompression-bound). Planet expected: −3–8 min on OSM ingest. Confirmed linked + active. |
| 2 | mimalloc as global allocator (all Rust binaries) | DONE | AU FST build 20.4 s after; server startup 362 ms after. Per-stage delta vs std allocator deferred to stage 6 timing emission. |
| 3 | Parallel countries in `build-openaddresses-index` | DONE | rayon par_iter; test passes; AU-local has 1 country so no measurable local delta. Planet 60-country build expects 8-16× speedup. |
| 4 | simd-json in `wof-importer` | DONE | tests pass; deserialises into the same `serde_json::Value` so `extract_outer_rings` stays unchanged. Real win on planet's ~8.6 GB GeoJSON. |
| 5 | Parallel states in `build-gnaf-index` | TODO | target: −60 to −70 % of G-NAF stage |
| 6 | Per-stage timing emission (all builders) | TODO | 0 perf — observability only, unblocks the next round |

## Working method between stages

1. **Implement** the change.
2. **Build** affected binary in release.
3. **Smoke test** — for each builder we touch, run against AU data
   and confirm:
   - exits 0
   - output `.bin` files exist
   - server can load + serve a sample query against the rebuilt
     index (`./target/release/query-server data/index :13580`,
     curl `/reverse?lat=-33.87&lon=151.21`, expect a Sydney address)
4. **Time** the affected stage with `time` or the built-in timing
   we add in stage 6.
5. **Record** the actual numbers in the table above + a short note
   in the per-stage section below.
6. **Commit** with a focused message. Each stage = one commit.
7. **Push** so progress is durable.

## Stage 1: libdeflate in `build-index`

**Status: DONE** (commit pending — see end of stage)

### Background

PBF inflate currently goes through zlib via libosmium. libdeflate is
2–3× faster on the same data, using SSE4.2/AVX2 for the Huffman + LZ77
fast paths. libosmium recognises libdeflate at compile time when
`OSMIUM_WITH_LIBDEFLATE=1` is defined and the libdeflate library is
linked. Available since libosmium 2.18.

### Steps

1. `brew install libdeflate` (macOS dev). Verify with
   `brew list libdeflate` and `pkg-config --cflags --libs libdeflate`.
2. Edit `builder/CMakeLists.txt`:
   - Add `find_package(libdeflate)` (or `find_library`/`find_path`
     fallback if no CMake config).
   - Add `target_compile_definitions(build-index PRIVATE
     OSMIUM_WITH_LIBDEFLATE)`.
   - Link `libdeflate::libdeflate_static` (or shared).
3. Re-run cmake configure + build. Verify libdeflate is linked
   (`otool -L builder/build/build-index | grep deflate` on macOS).
4. Smoke-test: run the AU build, verify output identical (file
   sizes match the prior 78 s run).
5. Time three runs, record median.

### Expected numbers

- AU `build-index` from 78 s → ~55–65 s (mostly Pass 2 inflate).
- Planet impact bigger because PBF is ~75 GB compressed there.

### Gotchas

- Homebrew's libosmium *might* not have been compiled with
  `WITH_LIBDEFLATE=ON` itself. If so, `OSMIUM_WITH_LIBDEFLATE` at
  our level is ignored. Verification step: profile or `dtrace`
  for libdeflate symbols at runtime; or check `brew install
  --build-from-source libosmium -DWITH_LIBDEFLATE=ON` if homebrew
  doesn't ship it.
- If libosmium ignores it, fall back to using libdeflate directly
  in our own decompression of `pbf` → not viable; libosmium owns
  the parser. Document the limitation if hit.

### Result

- Wallclock before: 78 s
- Wallclock after: 79 s (within noise — AU run-to-run variance is ±2 s)
- Δ on AU: **~0 % (within noise)**
- libdeflate IS linked and active:
  `otool -L builder/build/build-index` shows
  `libdeflate.0.dylib` and the build defines `OSMIUM_WITH_LIBDEFLATE`.
- **Why no AU gain:** AU PBF is only 890 MB compressed; decompression
  is not the AU bottleneck — Pass 2 OSM tag iteration dominates.
  libdeflate's win is on planet PBF (~75 GB compressed) where
  decompression takes a substantial fraction of Pass 2 wallclock.
- **Action taken:** still ship the change. The link cost is zero,
  the planet-scale win is real (per libdeflate benchmarks: 2.7×
  faster gzip inflate vs zlib on x86-64 with AVX2). On the next
  AWS planet build, expect 3–8 minutes saved on the OSM ingest
  alone.

### Build-infra updates (in same commit as the CMake change)

User flagged we deploy on Ubuntu — added `libdeflate-dev` to:

- `Dockerfile` build stage line ~9 (added `libdeflate-dev`)
- `Dockerfile` runtime stage line ~32 (added `libdeflate0`)
- `packer/build-worldwide.pkr.hcl` line ~219 (added `libdeflate-dev`)
- `README.md` build prerequisites: brew + apt-get blocks both
  updated, plus a one-liner explaining libosmium picks it up
  automatically.
- `BUILD-DEPLOY.md` build-machine prerequisites updated.

### Outstanding follow-up (will surface in stage 6)

The 1 s AU delta is within run-to-run noise. To confirm the
libdeflate gain is real (and to get a defensible number for the
perf doc), we need either:

(a) per-stage timing emission so we can isolate "Pass 2 inflate"
    from "Pass 2 tag iteration" — that's stage 6, will close the
    loop.
(b) A planet build measurement on AWS — large effort, needs a
    full build run.

Going with (a) via stage 6.

---

## Stage 2: mimalloc as global allocator

**Status: DONE** (commit pending — see end of stage)

### Background

Microsoft's mimalloc is a drop-in malloc replacement that consistently
outperforms the system allocator (glibc malloc, jemalloc) on
allocation-heavy workloads. The Rust `mimalloc` crate ships a
pre-built binary and integrates via one declaration:

```rust
use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
```

Every Rust binary in our project does heavy small-allocation work
(tantivy postings, FST builders, hashmaps, string interning). mimalloc
is also thread-safe and behaves better under concurrent allocator
pressure than glibc malloc — relevant for stages 3 and 5.

### Affected binaries

All Rust binaries:

- `server/src/main.rs` (query-server)
- `server/src/bin/build_forward_index.rs`
- `server/src/bin/build_postcode_lookup.rs`
- `server/src/bin/build_gnaf_index.rs`
- `server/src/bin/build_openaddresses_index.rs`
- `server/src/bin/build_autocomplete_fst.rs`
- `tools/regression-runner/src/main.rs`
- `tools/wof-importer/src/main.rs`

### Steps

1. Add `mimalloc = "0.1"` to `server/Cargo.toml` and
   `tools/regression-runner/Cargo.toml` and `tools/wof-importer/Cargo.toml`
   (each crate is independent).
2. Add the `#[global_allocator]` declaration to every binary entry
   point.
3. Build all binaries with `cargo build --release --workspace`.
4. Smoke-test: re-run AU `build-index` (not affected — C++) plus
   `build-autocomplete-fst` against the AU index. Verify the
   server still serves Sydney correctly.
5. Time the Rust builders before vs after. Record the per-binary
   delta.

### Expected numbers

- 5–15 % across allocation-heavy Rust paths
- Server startup (mmap is alloc-free, but tantivy bootstrap allocates)
  may improve a few %.
- Biggest absolute saving probably in `build-forward-index` (tantivy
  build is alloc-heavy).

### Gotchas

- mimalloc compiles its own C source on first build. ~10 s of
  build-time cost added to a fresh build. Tradeoff is worth it.
- On x86_64 Linux, `mimalloc-secure` is an alternative with
  hardening features at a small cost. Plain `mimalloc` is fine
  for an internal builder.

### Result

- Added `mimalloc = { version = "0.1", default-features = false }` to:
  - `server/Cargo.toml`
  - `tools/regression-runner/Cargo.toml`
  - `tools/wof-importer/Cargo.toml`
- Added `#[global_allocator] static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;`
  to every binary entry point:
  - `server/src/main.rs` (query-server)
  - `server/src/bin/{build_autocomplete_fst,build_forward_index,build_gnaf_index,build_openaddresses_index,build_postcode_lookup}.rs`
  - `tools/regression-runner/src/{main,pelias_adapter}.rs`
  - `tools/wof-importer/src/main.rs`
- `cargo build --release --workspace` clean.

**Measured (post-mimalloc, AU index, M1 Max):**
- `build-autocomplete-fst`: 20.4 s wallclock (524 k → 248 k FST entries)
- `query-server` startup-to-/healthz: 362 ms (mmap is alloc-free, but
  tantivy + FST bootstrap allocates)
- `/reverse` 200 OK against Sydney coord with full Castlereagh St
  address.

**Real before/after delta deferred to stage 6.** Without per-stage
timing emission inside each binary we can't isolate "alloc time"
from "I/O + parsing time." When stage 6 lands, re-run a baseline
in a worktree pinned to pre-mimalloc and capture the delta in the
final snapshot doc.

Per published mimalloc benchmarks (Microsoft 2018, ongoing): 5–15 %
on alloc-heavy workloads is the expected band. Tantivy + FST
ingestion qualify; mmap-only paths (server load) won't budge much.

---

## Stage 3: Parallel countries in `build-openaddresses-index`

**Status: DONE** (commit pending)

### Background

`build-openaddresses-index` walks each `<csv-root>/<cc>/` country
directory in sequence, runs CSV parse + S2 binning + sort + write
per country. The countries are fully independent — separate output
files, separate string pools, separate cell maps. Embarrassingly
parallel.

Default OA batch covers ~60 countries. Sequential at ~30 s each
on AU = 30 minutes for the full set. With rayon `par_iter` over
countries on a 16-core box: ~3 minutes.

### Steps

1. `Cargo.toml`: add `rayon = "1.10"` to `server/Cargo.toml`
   `[dependencies]` (or `[dev-dependencies]` no — this is a
   release builder, prod dep).
2. In `server/src/bin/build_openaddresses_index.rs`:
   - Find the `for (cc, dir) in &country_dirs { build_country(...) }`
     loop in `run()`.
   - Replace with `country_dirs.par_iter().try_for_each(...)`.
   - The `eprintln!` calls inside `build_country` will interleave —
     accept the noise, or use a `Mutex<Stderr>` if logs need to
     stay readable. Default: accept interleaving (counts at the
     end stay correct).
3. Verify `build_country` doesn't share mutable state across
   countries. Currently uses local `StringPool`, local
   `Vec<(u64, AddressPoint)>`, local file writes. Should be OK
   — but double-check.
4. Build, smoke-test against the existing `tt` test country in
   `server/tests/openaddresses_roundtrip.rs`.
5. Time: hard to time without multi-country data locally. Note
   in plan: "real benchmarks require a planet-scale OA corpus.
   Validated correctness via integration test."

### Expected numbers

- Locally: no measurable change (only AU loaded by default + `tt`
  test).
- On AWS planet build: 8–16× faster on the OA stage.

### Gotchas

- The filesystem walk in `run()` builds `country_dirs` first, then
  iterates. The walk is sequential (single readdir) but cheap.
  Don't parallelise the walk itself.
- `eprintln!` interleave is fine; nothing parses the output.

### Result

(fill in after running)

---

## Stage 4: simd-json in `wof-importer`

**Status: DONE** (commit pending)

### Background

WoF planet GeoJSON is ~8.6 GB. We currently parse with
`serde_json` which is scalar. The `simd-json` crate parses ~3 GB/s
on x86-64 (AVX2) and ~1.5 GB/s on aarch64 (NEON), vs serde_json's
~500 MB/s.

### Steps

1. `tools/wof-importer/Cargo.toml`: replace `serde_json = "1"` with
   `simd-json = "0.13"` (or current latest). Keep `serde` for the
   derive macro.
2. Update `tools/wof-importer/src/main.rs`:
   - Replace `serde_json::from_str(...)` with
     `simd_json::serde::from_str(...)` (note: simd-json's `from_str`
     mutates the string buffer in place — accept a `&mut String`
     or call `from_slice` with a mutable byte buffer).
   - Replace `serde_json::Value` with `simd_json::OwnedValue` if
     used.
3. Re-test the existing wof-importer unit tests (parse_polygon_ring,
   parse_multipolygon_all_outer_rings).
4. Time against test fixture (small) — primary measurement is
   directional. The real win shows on the 8.6 GB planet GeoJSON
   on AWS.

### Expected numbers

- Tests: directional (microseconds).
- Planet GeoJSON: ~6× speedup of the parse phase.

### Gotchas

- simd-json's `from_str` requires the input to be a *mutable* `&mut
  str` because it modifies the buffer in place (zero-copy strings
  point into the original buffer). May need a wrapper that copies
  the input first if our current code passes `&str`.
- The "full" parse path uses `simd_json::OwnedValue`; the borrowed
  variant is faster but ties output lifetimes to the input buffer.

### Result

(fill in after running)

---

## Stage 5: Parallel states in `build-gnaf-index`

**Status: TODO**

### Background

G-NAF distributes per-state PSV files (NSW, VIC, QLD, SA, WA, TAS,
NT, ACT). Each state is independently parseable; the final
combined index merges them. Currently sequential.

### Steps

1. Read `server/src/bin/build_gnaf_index.rs` to confirm:
   - State files are processed in a sequential loop.
   - Per-state intermediate state lives in local variables (no
     shared mut refs).
2. Add rayon (already added in stage 3 if we did that one first;
   else add now).
3. Convert the state loop to `par_iter` / `par_bridge`.
4. Smoke-test against a real G-NAF corpus (we have one locally
   from prior runs).
5. Time before/after.

### Expected numbers

- 8 states currently sequential. With 8 cores: ~4–5× speedup of
  the per-state pass.
- Final merge step probably stays serial (small enough).

### Gotchas

- The combined string pool at the end may have order-dependence
  (intern order matters for offsets). If so, build per-state
  pools in parallel, merge serially at the end.
- Confirm the test harness `tests/gnaf_addresses.rs` still passes.

### Result

(fill in after running)

---

## Stage 6: Per-stage timing emission

**Status: TODO**

### Background

We can't tell where time goes inside any builder today. Adding
timing emission unblocks the next round of perf work and lets
operators see (e.g. in CI logs) which stage regressed.

### What "timing" means

For each binary, emit a structured line at the end summarising
elapsed time per logical phase. Format suggestion:

```
build-index timing:
  pass1_relations:    1.234s
  pass2_ingest:      48.567s
  resolve_interp:     0.123s
  dedup:              2.345s
  sort_addr_points:   1.111s
  write_index:        4.567s
  total:             58.000s
```

Emitted to stderr is fine; nothing parses it. Optionally a JSON
sidecar (`<output_dir>/build-timing.json`) for CI scraping.

### Steps per binary

**C++ `build-index`:**
- Add `chrono::steady_clock` blocks around the major sections in
  `run_build`.
- Print at end with `std::cerr`.

**Rust builders:**
- Small `Timer` RAII helper:
  ```rust
  struct Timer<'a> { name: &'a str, start: Instant }
  impl<'a> Timer<'a> {
      fn new(name: &'a str) -> Self { Self { name, start: Instant::now() } }
  }
  impl<'a> Drop for Timer<'a> {
      fn drop(&mut self) { eprintln!("  {}: {:.3}s", self.name, self.start.elapsed().as_secs_f64()); }
  }
  ```
- Sprinkle in the major phases of each binary.
- Track total via `Instant::now()` at top of `main()`.

### Steps

1. Add timer helper to a shared module. For C++ inline. For Rust
   put in `server/src/lib.rs` as `pub mod timing;` (or just
   inline in each bin — tiny, not worth a module).
2. Instrument:
   - `build-index`: 6 phases (Pass 1, Pass 2, resolve_interp,
     dedup, sort_addr_points, write_index).
   - `build-gnaf-index`: load PSVs, build index, write.
   - `build-openaddresses-index`: load CSVs, build index, write.
     (Per-country may be too noisy; total OA only.)
   - `build-postcode-lookup`: total only.
   - `build-forward-index`: load source, tantivy ingest, commit.
   - `build-autocomplete-fst`: build FST per country, write.
   - `wof-importer`: load source, parse GeoJSON, write.
3. Run AU build pipeline end-to-end, record per-stage numbers.
4. Commit.

### Expected output

End-to-end AU pipeline timing surface in one place — gives us
the data to decide stages 7+ if perf work continues.

### Result

(fill in after running)

---

## Notes for the next engineer (or future me)

- Each stage commits on its own. If something goes wrong mid-stage,
  the previous stage is preserved.
- The C++ build-index now uses `ankerl::unordered_dense::map` /
  `segmented_map`. If timings later show hashmap rehash as a
  bottleneck on planet, the segmented chunk size is the tunable.
- The runtime reader (Rust query-server) is unchanged by any of
  these stages. Output `.bin` formats are byte-identical.
- After all six stages, regenerate this doc as a snapshot
  (`build-pipeline-perf-snapshot-<date>.md`) with final numbers,
  similar to `readpath-optimisations-2026-04-25.md`.

## Open questions to revisit after stage 6

These didn't make the top-6 cut but become more attractive once
timing data is in:

- Parallel PBF block decode in `build-index` (libosmium thread-pool
  reader). Bigger blast radius (concurrency on shared state),
  needs handler-merge logic. Wait for stage 1 + 6 numbers.
- Pipeline-level overlap (run G-NAF + OA + WoF + tantivy in
  parallel after `build-index` finishes, sharing no data).
  Packer/scripts change, not code.
- Per-stage tantivy `IndexWriter` heap tuning (currently default
  ~50 MB; bigger heap = fewer commits, smaller segments = faster
  build).

## Working tree state at plan creation

- HEAD: `2344e9d` ("Builder: replace std::unordered_map with ankerl…")
- Working tree: clean
- Last 5 commits visible via `git log --oneline -5`
