# LTO config: measured trade-offs

> **Snapshot doc.** Captured 2026-04-26 against the local AU index.
> Not auto-generated — written once to settle which `lto = …` value
> to put in the workspace `Cargo.toml`. The numbers are reproducible
> via the commands at the bottom of this file.

## TL;DR

The hoisting fix in commit `6c742ed` made `[profile.release]` /
`[profile.bench]` actually load-bearing for the first time
(member-level profile sections were silently ignored under the
workspace). With LTO now in play, the question becomes: pick `false`,
`"thin"`, or `true` (== `"fat"`).

Measured against the existing Criterion benches:

| Setting | Cold build | Bench-binary size | Runtime perf vs `false` |
|---|---:|---:|---:|
| `lto = false` | 124 s | 4.83 MB | baseline |
| `lto = "thin"` | 150 s (+21 %) | 4.08 MB (−16 %) | **−1.8 % median, −4.0 % best case** |
| `lto = true` | 216 s (+74 %) | 3.23 MB (−33 %) | **−1.4 % median**, −11 % on one outlier |

**Decision: use `lto = "thin"`.** Fat LTO doubles the build penalty
for no measurable additional perf on the hot paths we actually care
about, and is statistically indistinguishable from thin on most
benchmarks. The one workload where fat wins (cross-country FST walks
in `fst_starts_with_any`) is a cold path, not the per-request hot
loop.

The workspace Cargo.toml is updated to `lto = "thin"` in commit
`<this-PR>`. The Dockerfile's `--config 'profile.release.lto=…'`
override is updated to match.

## Why this doc exists

When I first hoisted the profile setting, I claimed `lto = true`
gave "5–10 % on the reverse-geocode hot loop" without having
actually measured it — that was speculation phrased as fact.
This doc grounds the decision in real numbers so a future engineer
can see what the trade-off actually looks like and either confirm
or revisit.

## Methodology

```bash
cd /Users/pdeaudney/git/traccar-geocoder
INDEX_DIR=$(pwd)/data/index

for lto in false 'thin' true; do
    cargo clean
    if [ "$lto" = "thin" ]; then
        CFG='profile.bench.lto="thin"'
    else
        CFG="profile.bench.lto=$lto"
    fi
    time cargo bench --no-run --bench reverse_geocode --bench autocomplete --config "$CFG"
    rm -rf target/criterion
    GEOCODER_INDEX_DIR=$INDEX_DIR cargo bench \
        --bench reverse_geocode --bench autocomplete --config "$CFG" -- \
        --sample-size 30 --measurement-time 2 --warm-up-time 1
    cp -r target/criterion /tmp/lto-bench/criterion-lto-$lto
done
```

Hardware: this dev box (Apple Silicon laptop), single user, no
heavy background processes. Criterion sample-size 30,
measurement-time 2 s, warm-up-time 1 s — same parameters the
existing `readpath-optimisations-2026-04-25.md` snapshot uses, so
numbers are comparable in shape.

The `cargo clean` between runs forces a true cold build (no cache
reuse from the previous LTO setting). The `--config
profile.bench.lto=…` flag changes the bench profile without
modifying any committed file.

## Build cost

|         | Wall-time | Δ vs `false` |
|---------|----------:|-------------:|
| `false` | 2 m 04 s  | baseline     |
| `"thin"` | 2 m 30 s  | +26 s (+21 %) |
| `true`  | 3 m 36 s  | +1 m 32 s (+74 %) |

Cold build = `cargo clean` + `cargo bench --no-run`. The work that
LTO adds is at link time, after every dependency has compiled — so
the absolute Δ here scales with the leaf crates' size, not the
whole tree.

For CI / Packer AMI bake (which always starts from `cargo clean`
on a fresh instance), the +90 s difference between thin and fat is
the operationally relevant cost. For incremental dev iterations it
doesn't matter (debug builds skip LTO entirely).

## Binary size

Reported on the bench harness binaries (a fair stand-in for the
production query-server binary, since they link the same
`query_server` crate plus Criterion's harness).

|         | reverse_geocode bench | autocomplete bench |
|---------|----------------------:|-------------------:|
| `false` | 4 876 752 B (4.65 MiB) | 4 793 840 B (4.57 MiB) |
| `"thin"` | 4 107 952 B (3.92 MiB) | 4 045 360 B (3.86 MiB) |
| `true`  | 3 255 968 B (3.10 MiB) | 3 197 872 B (3.05 MiB) |

Roughly: thin ≈ −16 %, fat ≈ −33 %. Most of the size win comes from
LTO's dead-code elimination across crate boundaries. Smaller
matters for Docker images more than it does for runtime perf —
cache-line and L1-icache footprint is dominated by the actual hot
loop (a few KB), not the whole binary.

## Runtime perf

67 benchmark cases across two harnesses. Numbers below are
Criterion-reported medians.

### Aggregate

|             | Median Δ vs `false` | Mean Δ vs `false` | Range |
|-------------|---:|---:|---:|
| `"thin"` | **−1.77 %** | −1.87 % | [−4.0 %, +1.1 %] |
| `true`  | −1.43 %     | −2.07 % | [−11.3 %, +1.9 %] |
| `true` vs `"thin"` | **+1.03 %** | −0.19 % | [−8.4 %, +3.5 %] |

The `true` vs `"thin"` row is the surprise: on the median case fat
LTO is **slightly slower** than thin. The mean is barely better
because of one outlier where fat wins big (see
`fst_starts_with_any` below).

### `reverse_geocode` (the production hot path)

|              | `false` | `"thin"` | `true` | thin vs false | true vs false | true vs thin |
|--------------|--------:|---------:|-------:|--------------:|--------------:|-------------:|
| `sydney_cbd` | 66.17 µs | 65.82 µs | 67.27 µs | −0.5 % | +1.7 % | +2.2 % |
| `melbourne_cbd` | 35.27 µs | 34.77 µs | 35.20 µs | −1.4 % | −0.2 % | +1.2 % |
| `brisbane_cbd` | 20.27 µs | 19.91 µs | 20.12 µs | −1.8 % | −0.8 % | +1.0 % |
| `perth_cbd` | 29.43 µs | 28.93 µs | 29.46 µs | −1.7 % | +0.1 % | +1.8 % |
| `adelaide_cbd` | 20.34 µs | 19.83 µs | 19.97 µs | −2.5 % | −1.8 % | +0.7 % |
| `query_mixed` | 27.00 µs | 26.64 µs | 27.11 µs | −1.3 % | +0.4 % | +1.8 % |

Thin LTO produces a small but consistent improvement (~1–2 %) on
every CBD case. Fat LTO doesn't extend the win and in several
cases is fractionally slower than thin — within Criterion's noise
band (the `--sample-size 30` runs typically have ±1 % CI), but
the trend is clear: **fat doesn't pay**.

### `query_geo` (the geo-scan-only hot path)

|              | `false` | `"thin"` | `true` |
|--------------|--------:|---------:|-------:|
| `sydney_cbd` | 1.47 µs | 1.42 µs | 1.45 µs |
| `melbourne_cbd` | 2.09 µs | 2.04 µs | 2.08 µs |
| `query_mixed` | not measured | — | — |

Same pattern: thin gets ~3 % uniformly, fat doesn't extend it.

### `find_admin` (admin-polygon lookup)

|              | `false` | `"thin"` | `true` |
|--------------|--------:|---------:|-------:|
| `sydney_cbd` | 54.58 µs | 53.78 µs | 54.82 µs |
| `melbourne_cbd` | 32.95 µs | 32.35 µs | 32.72 µs |

Thin: −1.5 % consistent. Fat: noise; sometimes slightly worse.

### `fst_starts_with` (autocomplete prefix walk)

`limit = 10` rows (the production HTTP default):

| Prefix | `false` | `"thin"` | `true` | true vs thin |
|---|---:|---:|---:|---:|
| `"s"` (1-char broad walk) | 1.35 ms | 1.34 ms | 1.29 ms | **−4.1 %** |
| `"sy"` (2-char) | 36.81 µs | 37.21 µs | 35.31 µs | −5.1 % |
| `"syd"` (3-char) | 9.82 µs | 9.76 µs | 9.39 µs | −3.8 % |
| `"sydney"` (full word) | 7.52 µs | 7.36 µs | 7.22 µs | −1.9 % |

Here fat genuinely wins over thin (−2 to −5 %) — but the absolute
delta is microseconds against 10s-of-µs baselines, and these are
already cheap operations that won't dominate p95 latency. The
HTTP layer's overhead (axum routing, JSON serialisation,
syscall cost) is in the same ballpark.

### `fst_starts_with_any` (the outlier)

| Prefix | `false` | `"thin"` | `true` | true vs thin |
|---|---:|---:|---:|---:|
| `"s"` (any country, 1-char) | 1.45 ms | 1.40 ms | 1.29 ms | **−8.4 %** |
| `"syd"` (any country, 3-char) | 10.17 µs | 9.85 µs | 9.11 µs | **−7.6 %** |
| `"sydney"` (any country, full) | 7.77 µs | 7.57 µs | 7.07 µs | **−6.6 %** |

The only place fat LTO is meaningfully better than thin. This
benchmark walks every loaded per-country FST in sequence — on a
single-country dev index that's effectively the same as
`starts_with`, but on a worldwide index it scales linearly with
country count. The cross-country iterator likely benefits more
from inlining than the per-country case because there's an extra
crate-boundary call per iteration.

This is a real win, but on the cold path: `/search` with a country
hint goes through `starts_with`, not `starts_with_any`. The
no-country-hint case is rare in production.

## Conclusion

`lto = "thin"` is the right default for this codebase:

- Captures the runtime perf wins on every hot path tested.
- Half the build penalty of fat LTO (+21 % vs +74 % on cold builds).
- Smaller binary than no-LTO, slightly bigger than fat (3.92 MiB
  vs 3.10 MiB on the bench harness — both fine for a server).

The one workload where fat LTO is meaningfully better
(`fst_starts_with_any`) is a cold path. If we ever observe it
becoming hot in production telemetry, revisit this decision —
otherwise thin is the better trade.

## What changed in this commit

- `Cargo.toml` (workspace root): `lto = true` → `lto = "thin"` in both
  `[profile.release]` and `[profile.bench]`.
- `Dockerfile`: `--config 'profile.release.lto=true'` →
  `--config 'profile.release.lto="thin"'`.

## Reproduction

```bash
cd /Users/pdeaudney/git/traccar-geocoder
INDEX_DIR=$(pwd)/data/index

# Three configs, fresh build each time. ~25 minutes total.
for lto in 'false' '"thin"' 'true'; do
    cargo clean
    label=$(echo "$lto" | tr -d '"')
    time cargo bench --no-run --bench reverse_geocode --bench autocomplete \
        --config "profile.bench.lto=$lto"
    rm -rf target/criterion
    GEOCODER_INDEX_DIR=$INDEX_DIR cargo bench \
        --bench reverse_geocode --bench autocomplete \
        --config "profile.bench.lto=$lto" -- \
        --sample-size 30 --measurement-time 2 --warm-up-time 1 \
        2>&1 | tee /tmp/bench-lto-$label.log
done
```

The `extract.py` script used to assemble the comparison tables in
this doc lives at `/tmp/lto-bench/extract.py` (not committed —
this is a one-off snapshot).

## Limitations

- Measured on a single dev box, not the production EC2 instance
  type (r8g / r8gd Graviton 4). The qualitative shape (thin ≈ fat
  on hot paths) should carry, but absolute deltas may differ.
- Sample size 30, measurement window 2 s — picks up signal at the
  ~1 % level but not below. Differences within ±1 % between thin
  and fat are within noise.
- The bench harness includes the `query_server` crate's full
  dependency graph (incl. tantivy, h3o, etc.). The OpenTelemetry
  + AWS SDK paths aren't exercised at bench time, so this doesn't
  capture LTO's effect on those (which probably matters less since
  they're not on the hot loop).
