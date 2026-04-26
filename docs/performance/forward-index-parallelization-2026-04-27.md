# Forward index build parallelization — measured 2026-04-27

> **Snapshot doc.** Captured against the planet index built on the
> NVIDIA GB10 host (see `planet-build-on-gb10-2026-04-27.md`). Records
> the speedup from parallelising `build-forward-index --partition-by-country`
> and the residual ceilings each phase hits. Not auto-generated;
> reproducible via the recipe at the bottom.

## TL;DR

`build-forward-index --partition-by-country` on planet (211 country
indexes, 21 M docs after filter+dedup) dropped from **15 minutes to
80 seconds — an 11.3× wall-clock speedup** at `RAYON_NUM_THREADS=20`
on the GB10's 20-core Grace CPU.

The change refactored the binary's hot loop from a single sequential
producer feeding tantivy's internal indexer threads into two
explicit phases, both rayon-parallel:

  - **Phase 1 (`forward_classify`)** — par_iter over places + ways,
    runs `idx.find_admin(lat, lng)` per doc, buckets by country.
  - **Phase 2 (`forward_build_parallel`)** — par_iter over the
    per-country buckets, each gets its own tantivy `IndexWriter`
    (single-threaded internally so we don't oversubscribe on top of
    rayon's pool).

Commits: `baf9cd3` (phase 2 parallel), `fec8b0b` (phase 1 parallel),
`809426c` (thread-count diagnostic at phase 1 start).

## Why it mattered

Post-`build-index`, the forward index build was the longest-running
non-data-fetch step. Empirically on the GB10 planet build:

  - `build-index` (C++): ~13.5 hours (I/O-bound; documented separately)
  - `build-forward-index --partition-by-country` (Rust): 15 minutes
  - everything else combined (autocomplete FST, postcode lookup,
    GNAF index, OA index, smoke test): a few minutes

15 minutes for a single Rust binary on a 20-core box was the obvious
remaining lever. Pre-fix CPU utilisation during that step was
**1.36×** (≈1.4 of 20 cores active) — essentially serial.

## What changed

### Before

```rust
for way in ways {
    let admin = idx.find_admin(lat, lng);  // serial, expensive
    if !seen.insert((way.name_id, suburb, cc)) { continue; }
    let cb = get_or_create(&mut per_country, ..., heap_bytes)?;
    cb.writer.add_document(...);  // hands off to tantivy queue
}
```

`find_admin` runs ~50 M times serially. Tantivy's internal indexer
threads (default `min(8, ncpus/2)`) consume docs from the writer
queue concurrently, but they're starved by the serial producer.

### After

```rust
// Phase 1: parallel classify
let way_candidates: Vec<(...)> = ways.par_iter()
    .filter_map(|way| {
        let admin = idx.find_admin(lat, lng);  // now parallel
        ...
        Some((name_id, suburb, cc, PendingDoc { ... }))
    })
    .collect();

// Sequential dedup pass — cheap, just HashSet inserts
for (name_id, suburb, cc, doc) in way_candidates {
    if seen.insert((name_id, suburb, cc)) {
        buckets.entry(cc).or_default().push(doc);
    }
}

// Phase 2: parallel per-country tantivy build
let stats = buckets.into_par_iter()
    .map(|(cc, docs)| {
        let writer = idx.writer_with_num_threads(1, heap_bytes)?;
        for d in &docs { writer.add_document(...)?; }
        writer.commit()?;
        Ok((cc, ...))
    })
    .collect::<Result<HashMap<_, _>, _>>()?;
```

Two key choices:

1. **Materialise candidates to `Vec` between par_iter and dedup.** The
   alternative — per-thread fold-reduce with thread-local seen sets
   — would let cross-thread duplicates survive the merge unless we
   re-deduped in the reducer. For 48 M ways × ~80 byte intermediate
   that's ~3.8 GB extra peak memory; freed once dedup completes.
2. **`writer_with_num_threads(1, ...)`** in phase 2. Each rayon
   worker owns one country build; if each one then spawned tantivy's
   default 8 internal indexer threads, we'd have 20×8=160 threads
   contending on 20 cores. Single-threaded writers + outer rayon
   parallelism gives near-linear scaling without oversubscription.

## Measured numbers

Source: GB10 host (Grace + Blackwell, 20-core CPU, 122 GiB visible
RAM, M.2 NVMe). Same planet input as the baseline build. Ran with
three rayon thread settings to triangulate scaling.

| `RAYON_NUM_THREADS` | classify | build_parallel | total | speedup vs N=1 total |
|---:|---:|---:|---:|---:|
| 1 | 837.4 s | 68.1 s | 905.7 s | 1.00× |
| 8 | 113.5 s | 19.0 s | 132.7 s | **6.83×** |
| 20 | 66.9 s | 12.8 s | **80.1 s** | **11.31×** |

Pre-fix baseline (the binary before any of these commits, single
producer + tantivy internal threads): 896.6 s wall, 109 % CPU.
The N=1 line above (905.7 s) approximates that baseline — slightly
slower because the new phase 2 with `writer_with_num_threads(1, ...)`
loses tantivy's internal parallelism that previously masked some of
the producer cost. At any thread count above 1 the new code wins
decisively.

### Phase 1 — `forward_classify` (find_admin loop)

| Range | Speedup observed | Theoretical max | Efficiency |
|---|---:|---:|---:|
| 1→8 threads | 7.38× | 8× | **92 %** |
| 1→20 threads | 12.51× | 20× | 63 % |

Near-perfect linear scaling at 8 threads — `find_admin` is genuinely
embarrassingly parallel (read-only against `&self`, no
synchronization in the call path). The drop to 63 % at 20 threads is
the standard Amdahl regime: LPDDR5x memory-bandwidth contention as
20 threads concurrently random-read the mmap'd admin polygon data.
That's a hardware ceiling, not a code bug.

### Phase 2 — `forward_build_parallel` (per-country tantivy)

| Range | Speedup observed | Notes |
|---|---:|---|
| 1→8 threads | 3.58× | Long-pole bound starts kicking in |
| 1→20 threads | 5.32× | Long-pole binding |

Phase 2 plateaus because it's bounded by the **longest-running
single country build**. The US, Russia, India, Germany etc. each
have an order of magnitude more docs than an average country; with
`writer_with_num_threads(1, ...)` we can't break a single country's
build across multiple cores. As soon as 20 threads are processing
20 countries, the ones that finish small countries early sit idle
waiting on the long pole.

Empirically: AU's tantivy build (524 k docs) takes ~1.4 s on this
host; the US is ~10× larger, putting the long pole near 12-15 s.
The 12.8 s observed at 20 threads matches that floor.

## Tuning knobs

| Var | Default | Effect |
|---|---|---|
| `RAYON_NUM_THREADS` | num_cpus (20 on GB10) | Caps both phases' parallelism. Lower for memory-tight hosts (fewer concurrent writers × heap_bytes peak) or to leave cores for other work. |
| `--tantivy-heap-mb` | 512 | Per-writer heap budget. Concurrent peak ≈ rayon_threads × heap_bytes. On a 20-core box at default that's ~10 GB. |

The thread-count diagnostic (`[stage] forward_classify: starting
(rayon threads = N, RAYON_NUM_THREADS = ...)`) prints at phase 1
start so production log analysis can correlate wall-time anomalies
with the configured concurrency without re-running.

## Memory profile

Peak in-process memory during a planet `--partition-by-country`
build at `RAYON_NUM_THREADS=20`:

| Component | Peak |
|---|---:|
| Loaded `Index` (mmap'd .bin files) | ~1 GiB resident, larger virtual |
| Phase 1 candidates `Vec` (~21 M entries × ~120 bytes) | ~2.5 GiB |
| Phase 1 dedup HashSet | ~500 MiB at peak |
| Phase 2: 20 concurrent tantivy writers × 512 MiB heap | ~10 GiB |
| **Headline peak** | **~14 GiB** plus loaded index |

Comfortable on a 122 GiB host. For tighter hosts (smaller workstations,
EC2 m6i.xlarge class) lower `--tantivy-heap-mb` to e.g. 128 and/or set
`RAYON_NUM_THREADS=4` to cap concurrent writers. The phase 1 candidates
Vec is unavoidable given the dedup-correctness invariant; if it
becomes a problem, refactor to fold-reduce with cross-thread
re-dedup at merge.

## What's not parallelised (out of scope)

1. **The dedup pass between phase 1a and phase 1b** — sequential
   walk of the candidates Vec. It's cheap (HashSet inserts on ~50 M
   elements) relative to find_admin, so it doesn't show up in
   profiles. Could be made parallel via fold-reduce if needed.

2. **Per-country build threading.** Each country writer is single-
   threaded internally so we don't oversubscribe. There's a possible
   ~4× phase-2 win by giving the largest countries multi-threaded
   writers and small countries single-threaded ones. With phase 2 at
   12.8 s out of 80 s total that's a ~10 s saving — not worth the
   complexity for this snapshot.

3. **Phase 1 `find_admin` per-call cost.** S2-cell-based admin
   polygon lookup with point-in-polygon tests. The hot path is
   already cache-friendly. Further wins would come from caching
   admin results by S2 cell across adjacent ways (consecutive ways
   often share admin context); estimated 5-10× extra speedup. Not
   yet implemented; tracked as a possible follow-up.

## Reproduction recipe

```bash
cd /path/to/traccar-geocoder
cargo build --release --manifest-path server/Cargo.toml --bin build-forward-index

# Three runs at different thread counts to confirm scaling.
for n in 1 8 20; do
    rm -rf data/index/tantivy_*
    RAYON_NUM_THREADS=$n \
        ./target/release/build-forward-index data/index --partition-by-country \
        2>&1 | tee /tmp/fwd-build-N${n}.log
    grep "\[stage\]" /tmp/fwd-build-N${n}.log
done
```

The binary's first stage line surfaces the actual rayon thread count
so you can confirm the env var was applied. The subsequent two
`[stage]` lines give the per-phase split.

## Limitations

- Single hardware data point (GB10 + 122 GiB LPDDR5x). Different
  CPU/memory architectures may show different efficiency curves at
  higher thread counts; the 92 % efficiency at 8 threads should be
  portable, but the 63 % at 20 threads is influenced by the GB10's
  unified-memory bandwidth split.
- Single workload (planet build, 21 M docs across 211 countries).
  AU-only or single-region builds have one country and won't show
  phase-2 parallelism wins. The phase-1 win still applies but is
  smaller in absolute terms.
- Tantivy 0.22 was used. Newer tantivy versions might change the
  internal-thread default; if `writer(...)` (no thread count) is
  ever used in this codebase again, re-check the oversubscription
  budget against rayon's pool.
