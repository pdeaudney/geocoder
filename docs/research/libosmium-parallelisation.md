# Can we parallelise `build-index` across cores?

**Date**: 2026-04-22
**Context**: First worldwide (5-country) combined build was dragging on for hours. We observed `build-index` pinning at ~105 % CPU on a 10-core M1 Max — essentially single-threaded despite the hardware. This note captures what we learned about why, and what would be required to fix it.

## TL;DR

- **Root cause**: libosmium's Reader API deliberately serialises handler dispatch. There's a single "handler thread" that sees every `Node`, `Way`, and `Area` in order, and that thread is our bottleneck. I/O threads (decompressing PBF blocks) are already multi-threaded and mostly idle waiting on us.
- **Authoritative answer**: the library maintainer has confirmed this is by design. The "easy win" of bumping libosmium's thread count won't help.
- **To actually parallelise**: bypass `osmium::io::Reader` for PBF, read `PrimitiveBlock`s directly, and dispatch them to N worker threads — each running a thread-local `BuildHandler` — then merge at the end. This is a ~1–2 week C++ project, not a tweak.
- **Recommendation**: don't invest unless rebuild cadence justifies it. At current cadence (weekly-ish for a single region, monthly for worldwide), the baseline is acceptable. Revisit when a concrete pain point (e.g. CI-gated worldwide rebuilds) shows up.

## Observed symptoms

On the 2026-04-22 worldwide build (AU + NZ + GB + CA + US, ~20 GB of PBFs):

```
$ ps -A -o pid,pcpu,pmem,comm | grep build-index
82617 105.5  9.0 ./build/build-index
```

105 % CPU → roughly one core. The M1 Max has 10 cores; 9 are idle. We had not configured any `OSMIUM_POOL_THREADS` override, so libosmium should have been using its default pool size (`hardware_concurrency() / 2` per its source).

Pass 1 of the builder (relation scanning) does hit ~400 % CPU per issue [#143](https://github.com/osmcode/libosmium/issues/143) — the relations pass is multi-threaded because `osmium::handler::Handler` isn't the bottleneck there. Pass 2 (our `BuildHandler` processing nodes, ways, and areas) is the one stuck at ~100 %.

## Investigation

### Hypothesis 1: libosmium thread pool under-configured

Wrong. Libosmium already spins `hardware_concurrency() / 2` threads by default, for *decompression and parsing*. Those threads feed `osmium::memory::Buffer`s into a single queue that the handler thread drains. Adding more I/O threads doesn't help when the handler is the choke point — they just fill faster and block sooner.

### Hypothesis 2: `osmium::io::Reader` has a direct "parallel handler" mode

Wrong. The Reader is deliberately serial on the handler side. From Jochen Topf (libosmium maintainer) in [#370 (2023-12-23)](https://github.com/osmcode/libosmium/issues/370):

> Libosmium I/O is also designed with the idea that writing concurrent code in C++ is hard and that most people don't want to do that. So it gives you that stream of data without you having to think about any concurrency. It will use concurrency behind the scenes for some types of file formats when doing the actual I/O and en/decoding of the data, but hides it from the library user.
>
> This design has its limitations of course, and you found some of them.

### Hypothesis 3: a `start_batch` / `end_batch` handler API could preserve simplicity while allowing parallel dispatch

Proposed in the same issue by user @cldellow. Topf rejected it (2024-01-04):

> The problem with your proposed interface is that it would depend on the actual file format used whether you get any kind of parallel processing out of it. For PBF files this would (sort of) work, but for other file formats it doesn't, because they are not block-based.

Issue closed, no library-level fix landed.

### What about the future-direction comment?

Topf mentions (same thread):

> I think if we can solve the problem that moving the Buffers from one thread to another through that single queue seems to be blocking too much, we can decouple all of this. We'd still move everything through a single thread, but that thread doesn't have to do more than push the buffers on to other threads as it sees fit, if the developer wants this.

That's a roadmap pointer, not something shipped. The referenced design discussion is in [#151](https://github.com/osmcode/libosmium/issues/151). Nothing to merge in.

## Ecosystem check

- **osm2pgsql**: per [Nominatim discussion #3335](https://github.com/osm-search/Nominatim/discussions/3335), the import itself is not meaningfully multi-threaded — only the PostgreSQL indexing phase after load is parallel. Same architectural wall.
- **[ParallelPBF (woltapp)](https://github.com/woltapp/parallelpbf)**: Java library with reentrant callbacks from N worker threads. Exactly the pattern we'd need — but Java, so not a drop-in for our C++ handler. It's a useful *design reference* for a C++ port.
- **[protozero](https://github.com/mapbox/protozero)**: the low-level Protobuf decoder that libosmium uses under the hood. Not parallel-by-default either, but if we were writing our own block-dispatching PBF reader, this is the layer we'd call into directly.

## What a real fix would look like

Conceptually:

```
             ┌─────────────────┐
PBF file  →  │  block reader   │  →  N × PrimitiveBlock queue
             └─────────────────┘
                                        │
                                        ▼
             ┌─────────┐  ┌─────────┐  ┌─────────┐  ...  ┌─────────┐
             │ worker  │  │ worker  │  │ worker  │       │ worker  │
             │ thread  │  │ thread  │  │ thread  │       │ thread  │
             │   +     │  │   +     │  │   +     │       │   +     │
             │  local  │  │  local  │  │  local  │       │  local  │
             │ handler │  │ handler │  │ handler │       │ handler │
             └────┬────┘  └────┬────┘  └────┬────┘       └────┬────┘
                  │            │            │                 │
                  └────────────┴────────────┴─────────────────┘
                                      │
                                      ▼
                              merge-and-sort phase
                              (admin, place, street, addr, i18n)
```

Each worker:
- Owns a thread-local `addr_points`, `street_ways`, `place_points`, `admin_polygons`, `i18n_names` accumulator.
- Owns a thread-local string pool (compacted + renumbered into a global pool during merge).
- Owns a thread-local `osmium::area::Assembler` — complication: `Assembler` wants relations from Pass 1, which would need to be replicated per thread OR read once and broadcast read-only.

Merge phase:
- Concatenate per-worker arrays.
- Re-intern strings into a global pool (offset rewriting per record).
- Sort into the expected output order (for `addr_entries`, `street_entries`, etc.).
- Write `.bin` files.

Gotchas observed from the maintainer discussion and our own code:

1. **PrimitiveBlocks aren't relation-aware.** Areas (our admin polygons) are built from multipolygon relations that span many ways, often across blocks. The current two-pass design (relations first, then areas) depends on `MultipolygonManager::prepare_for_lookup()` finishing before any way processing starts. Per-worker `MultipolygonManager`s would each need the full relation set.
2. **`NodeLocationsForWays` caches node locations in a sparse file-backed index.** The current sparse-file index assumes one writer. Each worker needs its own, or a thread-safe shared one (adds a layer of locking).
3. **String interning is hot.** Our `BuildHandler` interns every name/housenumber/etc into a single pool. Per-worker pools are cheap to build; the post-hoc renumbering is the awkward part, especially since name_ids are already embedded in emitted records.
4. **Order preservation.** A few downstream consumers (e.g. the S2 cell sort) assume a specific row ordering. Merge has to reconstruct that before the final `.bin` writes.

Complexity estimate: ~1,500–2,500 lines of C++ added, centred on `build_index.cpp` plus probably a new `ParallelReader` header. Testing strategy: build AU both ways and `diff -r` the two output dirs with a canonical sort of each `.bin` table. Output must be bit-identical, subject to sort-key stability.

## When to revisit

Right now we don't need this. We rebuild a single region every week or two; worldwide rebuilds are monthly at most. The 2-hour worldwide baseline (once confirmed) is tolerable.

Concrete triggers that would make it worth doing:

- CI-gated worldwide rebuilds (every commit) — current pace is noise; full rebuild on each merge isn't.
- Moving to planet scale — a single-threaded planet build is probably 10+ hours, which crosses the "unattended overnight job" threshold.
- A hardware change — if we target smaller cores (Graviton, cloud commodity), the single-thread penalty becomes relatively worse.

If any of those land, the path forward is:

1. Read [`protozero`](https://github.com/mapbox/protozero) docs and the [ParallelPBF (Java)](https://github.com/woltapp/parallelpbf) callback model as a design reference.
2. Prototype a `ParallelPbfReader` that emits `PrimitiveBlock`s to a work-stealing pool, one of which drives a thread-local `BuildHandler`.
3. Measure end-to-end on AU first (small enough to iterate fast), compare bit-for-bit against the serial output.
4. Add thread-local `NodeLocationsForWays` scoped per PBF file (avoids cross-file sharing).
5. Merge phase: a small separate pass that renumbers string offsets and concatenates record arrays. This is ~200 lines, not complex.

Do not pursue without a concrete performance budget ask — this is a trap for premature optimisation without a clear "we need N× here, and the 1–2 weeks spent earn back M weeks of wait time" justification.

## Related internal notes

- [`docs/worldwide-build.md`](../worldwide-build.md) — measured build times per region.
- [`docs/performance/benchmarks.md`](../performance/benchmarks.md) — runtime query benchmarks (unrelated to build-time).

## External references

- [libosmium #370: best practice for CPU-intensive handlers](https://github.com/osmcode/libosmium/issues/370) — the definitive thread on this question
- [libosmium #143: parallelise area parsing](https://github.com/osmcode/libosmium/issues/143) — confirms Pass 1 (relations) hits 400 %+, Pass 2 (areas) stuck at ~100 %
- [libosmium #151: Reader I/O architecture](https://github.com/osmcode/libosmium/issues/151) — the "future direction" referenced by Topf
- [Nominatim discussion #3335](https://github.com/osm-search/Nominatim/discussions/3335) — osm2pgsql hits the same wall
- [woltapp/parallelpbf (Java)](https://github.com/woltapp/parallelpbf) — reference implementation of the pattern we'd want
- [protozero](https://github.com/mapbox/protozero) — Protobuf decoder used under the hood; parallel PBF would call into this directly
