# Autocomplete FST build parallelization — measured 2026-04-27

> **Snapshot doc.** Sister to
> [`forward-index-parallelization-2026-04-27.md`](forward-index-parallelization-2026-04-27.md):
> the same `find_admin` + per-country-emit pattern applied to the
> autocomplete FST builder. Captured against the same GB10 planet
> index. Reproducible via the recipe at the bottom.

## TL;DR

`build-autocomplete-fst` on planet (211 country FSTs + 1 unified
FST, ~50.6 M raw candidates → 11.46 M unified entries) dropped from
**14 minutes 20 seconds to 1 minute 27 seconds — a 9.91× wall-clock
speedup** at default rayon thread count on the GB10's 20-core Grace
CPU.

Pattern matches the forward-index parallelization shipped earlier
the same day. The hot loop is the same `idx.find_admin(lat, lng)`
per-doc walk; the per-country emit is the same embarrassingly-parallel
file-write fan-out. The diff applied the same refactor: explicit
phases, parallel where each phase's data dependencies allow.

Commit: `5bbd436`.

## Measured numbers

GB10 host (Grace + Blackwell, 20-core CPU, 122 GiB visible RAM).
Same planet index that produced the 14:20 baseline timing:

| Phase | Time | % of total | Notes |
|---|---:|---:|---|
| `autocomplete_classify` | 56.69 s | 65 % | par_iter places + ways with `find_admin` |
| `autocomplete_dedup` | 10.40 s | 12 % | Sequential bucket-by-country with street dedup. Surprise: bigger than expected (predicted 1–2 s) |
| `autocomplete_intern` | 4.57 s | 5 % | par_iter over country buckets, each builds PerCountry |
| `autocomplete_per_country_emit` | 1.21 s | 1 % | par_iter writes 211 × (.fst / .bin / _strings.bin) triples |
| `autocomplete_unified_emit` | 12.10 s | 14 % | Sequential merged-FST emit (one shared intern pool + one BTreeMap) |
| **total** | **86.81 s** | | |
| **wall** | 1 m 26.9 s | | 19 m 39 s user / 2.9 s sys = **13.5× CPU** (~13.5 of 20 cores active avg) |

Baseline (pre-parallelisation): 860.232 s wall, 99.7 % CPU = 1.0 cores
average. Speedup = 9.91×.

## What's parallel and what's not

```
Phase                          Parallel?  Bound by
---------------------------------------------------------------
classify (find_admin)          rayon par_iter ✓
dedup (street dedup + bucket)  sequential   sequential HashSet+String::clone
intern (per-country PerCountry)rayon par_iter ✓
per_country_emit               rayon par_iter ✓
unified_emit                   sequential   merged BTreeMap insert
```

Two phases are deliberately sequential and now show up as the
post-parallelisation long poles:

  - **`dedup` (10.4 s)** — surprise. The street dedup loop walks
    47 M candidates, computing `(name_id, suburb_string.clone(),
    cc)` per row and hashing into a `HashSet`. The `String::clone`
    on the suburb key dominates. Could be replaced with a
    `Vec<u8>`-backed dedup or a `(name_id, suburb_offset_in_intern,
    cc)` tuple if we paid the intern cost upfront, or parallelised
    via `DashSet`. ~10 s saving available, ~10 % of total wall.
  - **`unified_emit` (12.1 s)** — by design. The unified FST has
    one shared string pool and one global BTreeMap of
    `<cc[0]><cc[1]><normalised_name>` → entry id. Parallelising
    needs a concurrent re-intern (a `DashMap<&str, u32>` plus
    deterministic-id assignment) and a shardable BTreeMap. Real
    work to land, ~10 s saving available, ~12 % of total wall.

Together they're the only meaningful chunk left. After phase 1a
parallelisation, the next ~22 s lives in these two sequential
phases. Diminishing returns past here without bigger refactors.

## Output is byte-identical

Determinism preserved:

  - **Place candidates** materialised via rayon's
    `IndexedParallelIterator::collect`, which preserves source-slice
    order. The bucket-by-country pass in phase 1b walks the result
    sequentially, so per-country candidate ordering matches the old
    serial behaviour.
  - **Street candidates** same shape; the sequential dedup uses the
    same `(name_id, suburb_key, cc)` key as before, with the same
    "first occurrence wins" semantics.
  - **Per-country intern** runs in parallel across countries, but
    each country's intern is sequential within its rayon worker —
    same intern offset assignment as the serial path.
  - **`ccs.sort()`** before per-country emit and unified emit gives
    deterministic country iteration, so unified entry IDs are
    stable across rebuilds with identical input.

Verified locally on AU: same entry counts, same key counts, same
strings.bin sizes as the pre-parallelisation binary.

## Tuning knobs

Same as the forward index:

| Var | Default | Effect |
|---|---|---|
| `RAYON_NUM_THREADS` | `num_cpus` (20 on GB10) | Caps parallelism across all parallel phases. Lower for memory-tight hosts. |

The thread-count diagnostic prints at phase-1a start so production
log analysis can correlate wall-time anomalies with the configured
concurrency without re-running.

## Memory profile

Peak working memory during a planet build at default thread count:

| Component | Peak |
|---|---:|
| Loaded `Index` (mmap'd .bin files) | ~1 GiB resident |
| Phase 1a candidates (`Vec<Candidate<'_>>` × 50.6 M) | ~5–6 GiB (borrows `&str`, so the candidate body is small but the Vec itself is large) |
| Phase 1b dedup HashSet (`HashSet<(u32, String, [u8;2])>` × ~11 M unique) | ~1–2 GiB |
| Phase 1c per-country PerCountry instances | ~1 GiB total across 211 countries |
| **Headline peak** | **~7–9 GiB** plus loaded index |

Comfortable on the GB10's 122 GiB. Substantially smaller working
set than the tantivy build (which had 20 × 512 MiB writers
concurrent).

## Speedup composition

Same overall pattern as the forward-index split: classify dominates
the residual time after parallelisation, secondary phases plateau
on sequential floors.

```
                  baseline       parallel        speedup
classify          ~700 s         56.7 s          12.4×
dedup             ~3 s           10.4 s          (was bundled; sequential)
intern            ~30 s          4.6 s           ~6.5×
per_country_emit  ~70 s          1.2 s           ~58×
unified_emit      ~50 s          12.1 s          ~4×
total             860 s          86.8 s          9.91×
```

(Baseline columns are derived from the original 99.7 %-CPU run's
total minus what we now know each phase costs after refactor;
treat them as estimates, not direct measurements.)

## Reproduction recipe

```bash
cd /path/to/traccar-geocoder
git pull origin main
cargo build --release --manifest-path server/Cargo.toml --bin build-autocomplete-fst

# Run with default thread count + log capture
time ./target/release/build-autocomplete-fst data/index --layout both 2>&1 | tee /tmp/fst-build.log
grep "\[stage\]" /tmp/fst-build.log
```

For thread-scaling experiments (mirrors the forward-index doc):

```bash
for n in 1 8 20; do
    rm -f data/index/fst_*.fst data/index/fst_*.bin data/index/fst_*_strings.bin
    rm -f data/index/fst_unified.* data/index/fst_unified_strings.bin
    RAYON_NUM_THREADS=$n \
        ./target/release/build-autocomplete-fst data/index --layout both \
        2>&1 | tee /tmp/fst-build-N${n}.log
    grep "\[stage\]" /tmp/fst-build-N${n}.log
done
```

## Limitations

- Single hardware data point (GB10, 20-core Grace + LPDDR5x). The
  92 %/63 % efficiency split observed in the forward-index doc at
  N=8 / N=20 should carry over to this binary's classify phase
  (same `find_admin` workload), but only the N=20 result was
  measured here.
- Single workload (planet build, 50.6 M candidates → 11.46 M
  unified entries across 211 countries). AU-only or single-region
  builds have one country and won't show phase-1c / phase-2
  parallelism wins.
- Sequential dedup phase (10.4 s) and sequential unified emit
  (12.1 s) are the next levers if anyone wants to push below
  ~80 s. Both require non-trivial refactors and would yield
  ~12 % savings each — captured here as known follow-ups, not
  blockers.

## Cross-reference

The combined post-`build-index` phase (formerly ~30 minutes) now
finishes in ~2.5 minutes:

| Step | Pre-parallelisation | Post-parallelisation | Doc |
|---|---:|---:|---|
| `build-forward-index` | ~15 min | 1 min 20 s | [`forward-index-parallelization-2026-04-27.md`](forward-index-parallelization-2026-04-27.md) |
| `build-autocomplete-fst` | 14 min 20 s | 1 min 27 s | this doc |
| Combined | ~29 min | ~2 min 47 s | — |

The C++ `build-index` phase (13.5 hours on this host) remains the
overall planet-build wall-time floor; see
[`planet-build-on-gb10-2026-04-27.md`](planet-build-on-gb10-2026-04-27.md)
for that profile.
