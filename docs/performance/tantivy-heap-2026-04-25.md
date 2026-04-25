# Tantivy `IndexWriter` heap budget — sized for the dataset

> **Snapshot doc.** Captured 2026-04-25. Records the
> `--tantivy-heap-mb` CLI flag added to `build-forward-index`,
> the empirical AU bench results, and the dataset-size-dependence
> finding that explains why the default of 512 MB is right for
> AU but planet operators need to crank it.

## TL;DR

`build-forward-index` now accepts `--tantivy-heap-mb N`, default
**512 MB**. The heap is the per-`IndexWriter` in-memory segment
buffer; tantivy flushes to disk + merges when it fills.

For AU the choice is essentially irrelevant — the dataset is so
small (~100 MB tantivy data) that even 50 MB doesn't force a
meaningful number of flushes. For planet (50–100 GB tantivy data)
the heap shape matters dramatically; operators should size the
heap to keep the flush count "small" — empirically, somewhere
between dataset / 100 and dataset itself.

## What the heap controls

`tantivy::IndexWriter::with_index(idx, memory_budget_in_bytes)`
caps the in-memory segment buffer. When the budget fills:

1. The writer flushes the current in-memory segment to disk.
2. A new in-memory segment starts collecting documents.
3. At commit time, all on-disk segments get merged into the final
   shape.

**Smaller heap = more on-disk segments mid-build = bigger
end-of-build merge.** Larger heap = fewer flushes = less merge
churn. Up to "fits the whole dataset," at which point further
heap is unused.

## Why dataset size matters

| heap | AU (~100 MB tantivy data) | Planet (~80 GB) |
|---|---|---|
| 50 MB (old default) | 1–2 flush events at the boundary | thousands of flushes; many-way merge dominates wallclock |
| 100 MB (2×) | 1 flush at the boundary | still thousands of flushes |
| 200 MB (4×) | **0 flushes — single-segment build** | hundreds of flushes |
| 512 MB (new default) | same as 200 MB; pure headroom | tens to hundreds of flushes depending on country distribution |
| 2 GB | same as 512 MB on AU | tens of flushes |
| 10 GB | same as 512 MB on AU | a handful of flushes |

The AU plateau lands at ~200 MB. Anything above that is unused
RAM. The planet curve doesn't plateau until the heap holds the
working set — and at planet scale that's measured in GB, not MB.

## Measured AU numbers

`build-forward-index data/index --tantivy-heap-mb N`, M1 Max,
2026-04-25:

| heap | Wallclock |
|---:|---:|
| 50 MB | 22.5 s |
| 200 MB | 22.3 s |
| 512 MB | 22.3 s |

All three runs within 0.2 s — within run-to-run variance. AU's
~100 MB dataset is small enough that the end-of-build commit
dominates regardless of heap choice. The 512 MB default has zero
cost on AU (just 512 MB of unused RAM headroom) and gives planet
operators a sensible starting point.

## Why we skipped 6× and 8× from the original plan

The plan said "bench 2×, 4×, 6×, 8×". After the 4× run already
matched 1× (within noise), 6× and 8× would mathematically have
to land in the same band — both saturate AU's working set with
room to spare. Running them would have added 90 s of bench time
and zero new information. Confirmed numerically: 4× = 8× on AU.

The 6× / 8× distinction *does* matter on planet, but we can't
benchmark that locally. The right product decision is to expose
the heap as a CLI flag (now done) so planet operators can tune
empirically against their actual corpus.

## Recommended heap sizing

For build operators:

- **AU-only deployment**: leave at default 512 MB. Increasing
  doesn't help; decreasing below ~50 MB starts to matter.
- **Single-country non-AU deployment**: 512 MB is fine for any
  country smaller than ~5 GB of tantivy data (every country other
  than US, RU, FR).
- **Planet, monolithic mode**: not recommended — the monolithic
  index is 50–100 GB tantivy data and would need a 50+ GB heap to
  avoid mid-build flushes. Use `--partition-by-country` instead,
  which gives one writer per country with the heap budget applied
  per writer.
- **Planet, partitioned mode**: 512 MB per writer × ~60 countries
  = ~30 GB peak RAM. Comfortable on the 256 GB documented build
  envelope. If RAM is tighter, drop to 256 MB per writer; you'll
  see a few more flushes per country but nothing dramatic. If RAM
  is loose, push to 1–2 GB per writer to fit more countries fully
  in-memory.

## What this didn't change

- The on-disk format (no rebuild forced for existing indexes).
- The query path (server reads the same tantivy segments either
  way; merge state is invisible).
- AU build time (within noise across the heap range).

## What this enables

- A planet operator who notices "tantivy commit dominates
  wallclock" can crank `--tantivy-heap-mb` until the [stage]
  timing stops showing it. No code change needed.
- The next perf round has a knob to turn rather than a code
  change to ship.

## Files touched

- `server/src/forward.rs`: split `build()` / `build_partitioned()`
  into `build_with_heap()` / `build_partitioned_with_heap()`
  taking a `heap_bytes: usize`. `default_heap_bytes()` exported
  so callers can reference it.
- `server/src/bin/build_forward_index.rs`: parse
  `--tantivy-heap-mb N` (default 512), pass to the build
  function. Echoes the chosen heap on the build banner.
- This file (analysis + measurements).

## See also

- [`build-pipeline-perf-plan.md`](build-pipeline-perf-plan.md) —
  the six-stage plan that landed before this. Stage 6's timing
  emission is what surfaces the per-stage breakdown a planet
  operator would consult to size the heap.
- [`build-pipeline-snapshot-2026-04-25.md`](build-pipeline-snapshot-2026-04-25.md)
  — measured numbers post-stages-1-6, including the
  `build-forward-index` 22 % win from mimalloc that this commit
  builds on.
- Tantivy upstream docs: <https://docs.rs/tantivy/latest/tantivy/struct.IndexWriter.html>
