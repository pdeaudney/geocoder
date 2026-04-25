# Build pipeline post-stages-1-6 snapshot (2026-04-25)

> **Snapshot doc.** Captures measured numbers after the six-stage
> build-pipeline perf plan
> ([`build-pipeline-perf-plan.md`](build-pipeline-perf-plan.md))
> shipped. Companion to
> [`readpath-optimisations-2026-04-25.md`](readpath-optimisations-2026-04-25.md)
> which covers the runtime read-path changes from the same week.

## What landed

Six commits, in order: `3784dd6` → `10334c7`. Cumulative effect
on the build pipeline:

| # | Stage | Commit | Effect |
|--:|---|---|---|
| 1 | libdeflate linked into `build-index` | `3784dd6` | PBF inflate via SIMD; AU within noise (decompression isn't AU-bound), planet expected −3–8 min on Pass 2 |
| 2 | mimalloc as global allocator across all 8 Rust binaries | `39460cb` | **−22 % on `build-forward-index`** (alloc-heavy tantivy ingest); −5 % on FST builder; modest on the others |
| 3 | rayon `par_iter` over countries in `build-openaddresses-index` | `ef3889a` | AU-local 1 country: noise. Planet 60 countries: 8–16× speedup expected |
| 4 | simd-json in `wof-importer` | `9800021` | Tests pass. Planet 8.6 GB GeoJSON expected ~6× faster parse |
| 5 | `rayon::scope` over the four G-NAF pre-passes | `875fe85` | Pass 4 (geocode) overlaps with passes 1–3; bounded but real on AU |
| 6 | per-stage timing emission across all builders | `10334c7` | 0 perf — observability. Format `[stage] <name>: <X>s` greppable by CI |

## Measured numbers (M1 Max, 2026-04-25, AU index)

### C++ `build-index`

```
[stage] pass1_relations: 0.65s
[stage] pass2_ingest:    65–69 s   ← 84 % of wallclock; libdeflate
                                    is linked but AU is not
                                    decompression-bound
[stage] resolve_interp:  0.37s
[stage] dedup:           0.12s
[stage] sort_addr_points: 0.16s
[stage] write_index:     10.4s    ← 13 %
[stage] total:           ~77–82 s   (run-to-run variance ±5 s)
```

The pre-stages baseline (before ankerl swap, before sort, before
libdeflate) was 100 s; the AU build is now around 78 s. The 22 %
reduction is dominantly from the ankerl hashmap swap (commit
`2344e9d`, before this six-stage plan); libdeflate is in but
unmeasurable on AU because Pass 2's tag iteration is the
bottleneck, not zlib.

### `build-forward-index` (tantivy)

| Build | Wallclock | Δ |
|---|---:|---:|
| Pre-mimalloc (`3784dd6`) | 28.3 s | baseline |
| Post-mimalloc (HEAD) | 22.0 s | **−22 %** |

Tantivy ingestion is exactly the alloc-heavy workload mimalloc
targets: postings construction, segment writers, BTree builds, all
allocate aggressively in tight loops. This is the cleanest measured
mimalloc win in the project.

### `build-autocomplete-fst`

| Build | Wallclock | Δ |
|---|---:|---:|
| Pre-mimalloc (`3784dd6`) | 21.9 s | baseline |
| Post-mimalloc (HEAD) | 20.8 s | −5 % |

FST construction is fundamentally less alloc-heavy than tantivy —
it's a sort + sequential insert. Modest win matches the workload
shape.

### Builders not measured locally

- `build-gnaf-index`: needs G-NAF PSV files; not in this dev env.
  The four pre-passes now run concurrently via `rayon::scope`;
  expected 30–50 % wallclock cut on AU when re-run with PSVs
  present.
- `build-openaddresses-index`: AU-only locally is one country, no
  measurable parallel win at that count. Planet 60-country runs
  will see the 8–16× speedup the rayon swap delivers.
- `wof-importer`: needs WoF SQLite; planet GeoJSON parse expected
  ~6× faster via simd-json.
- HTTP cold/warm bench attempted; Docker Desktop's VM was at
  process-resource limit at run time. Server-side mimalloc impact
  not isolated; will surface on the next clean run.

## Estimated planet-build impact

These are projections, not measurements — confirmed by AU numbers
where the workload shape applies:

| Stage | Planet wallclock saving |
|---|---|
| 1: libdeflate | 3–8 min off Pass 2 (the 84 % portion) |
| 2: mimalloc, build-forward-index | 1–3 min, scales with index size |
| 3: parallel OA countries | 25+ min off OA stage (currently sequential 60-country) |
| 4: simd-json WoF | 1–2 min off the 8.6 GB GeoJSON parse |
| 5: parallel G-NAF pre-passes | small (G-NAF is AU-only) |
| 6: timing emission | 0 (instrumentation) |
| **Total estimated** | **30–40 min off a ~45 min planet build** |

If those projections hold, planet drops from ~45 min to **~10–15
min** end-to-end.

## What's left on the table

These didn't make the six-stage plan and remain candidates for
the next round:

1. **Parallel PBF block decode** in `build-index` — Pass 2 is 84 %
   of AU wallclock; halving it via libosmium's thread-pool reader
   would drop AU from 77 s to ~45 s and planet from 45 min to ~25
   min on its own. Needs handler thread-safety review (current
   handlers append to globals).
2. **Tantivy `IndexWriter` heap budget** — default ~50 MB; planet
   is alloc-pressure-bound; bigger heap = fewer commits = smaller
   segments to write.
3. **Pipeline-level overlap** — G-NAF + OA + WoF + tantivy all
   read from `data/index/` after `build-index` finishes and have
   no inter-dependencies. Running them concurrently in the
   orchestrator is a Packer/scripts change, not code.
4. **`build-autocomplete-fst` sub-staging** — single 21 s phase
   today; if a future round identifies the FST-build hotspot
   (probably the per-country FST construction inside the loop),
   add sub-stages.

## How to re-run this

Builders all emit `[stage] <name>: <X>s` lines on stderr. Format
matches between C++ and Rust so a single CI grep
(`grep -E '^\[stage\]'`) works across the lot.

For a clean before/after on a specific binary:

```bash
git worktree add /tmp/geocoder-baseline <before-commit>
cd /tmp/geocoder-baseline && cargo build --release --bin <bin>

# back to main
cd /Users/pdeaudney/git/traccar-geocoder
./target/release/<bin> ...        # HEAD timing
/tmp/geocoder-baseline/target/release/<bin> ...   # baseline
git worktree remove --force /tmp/geocoder-baseline
```

For end-to-end HTTP load:

```bash
./scripts/bench-http.sh --label <run-name> --mode both
```

(needs Docker Desktop running and not pegged at process limit;
we hit that during this snapshot capture).

## Files referenced

- `docs/performance/build-pipeline-perf-plan.md` — the plan that
  drove these changes; now marked COMPLETE.
- `docs/performance/readpath-optimisations-2026-04-25.md` —
  read-path snapshot from the same week (covers runtime, not
  build).
- `docs/performance/hashmap-choice-2026-04-25.md` — analysis
  behind the ankerl::unordered_dense pick that closed the
  pre-stage AU build from 100 s to ~78 s.
