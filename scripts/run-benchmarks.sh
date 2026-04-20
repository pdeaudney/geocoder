#!/bin/sh
# Run criterion benchmarks and refresh docs/performance/benchmarks.md.
#
# Requires a built index to exist at $GEOCODER_INDEX_DIR (defaults to
# ./data/index). Uses criterion's bencher-format output for easy
# parsing, then renders a markdown table with median wall-times.
#
# Usage:
#   ./scripts/run-benchmarks.sh
#
# Environment:
#   GEOCODER_INDEX_DIR    Path to the built index (default ./data/index)
#   BENCH_MEASURE_SEC     Criterion measurement time in seconds (default 3)
#   BENCH_WARMUP_SEC      Criterion warm-up time in seconds  (default 1)
set -eu

DATA_DIR="${GEOCODER_INDEX_DIR:-./data/index}"
MEASURE="${BENCH_MEASURE_SEC:-3}"
WARMUP="${BENCH_WARMUP_SEC:-1}"
DOC_DIR="docs/performance"
DOC_FILE="$DOC_DIR/benchmarks.md"

if [ ! -d "$DATA_DIR" ]; then
    echo "error: index dir $DATA_DIR missing — build the index first" >&2
    exit 2
fi

# Cargo bench runs the harness from the server crate's working
# directory, so a relative `./data/index` resolves incorrectly. Pin
# to an absolute path before handing it to the child process.
case "$DATA_DIR" in
    /*) ABS_DATA_DIR="$DATA_DIR" ;;
    *)  ABS_DATA_DIR="$(cd "$DATA_DIR" && pwd)" ;;
esac

mkdir -p "$DOC_DIR"

# Capture platform metadata for the doc header.
OS_NAME="$(uname -s)"
OS_VERSION="$(uname -r)"
CPU="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 '^model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | sed 's/^ *//' || echo 'unknown')"
RAM_GB="$(sysctl -n hw.memsize 2>/dev/null | awk '{print int($1/1024/1024/1024)}' || awk '/MemTotal/{print int($2/1024/1024)}' /proc/meminfo 2>/dev/null || echo '?')"
RUSTC="$(rustc --version 2>/dev/null || echo 'unknown')"
STAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo "==> running criterion benches (warmup=${WARMUP}s, measure=${MEASURE}s)"
RAW="$(mktemp -t bench-raw.XXXXXX)"
GEOCODER_INDEX_DIR="$ABS_DATA_DIR" cargo bench -p query-server --bench reverse_geocode -- \
    --warm-up-time "$WARMUP" \
    --measurement-time "$MEASURE" \
    --output-format bencher \
    2>&1 | tee "$RAW" > /dev/null

# Extract lines of the form:
#   test query_geo/sydney_cbd ... bench:          20,278 ns/iter (+/- 101)
# Build the markdown table.
TABLE="$(mktemp -t bench-table.XXXXXX)"

# Produce one markdown table per benchmark group. Groups come from
# criterion's "test <group>/<fixture> ... bench: ..." shape. We split
# the name on `/`, treat everything before the last segment as the
# group, and emit a new table header whenever the group changes.
awk '
function fmt_time(ns,    val) {
    if (ns+0 >= 1000) return sprintf("%.2f µs", ns/1000.0)
    return sprintf("%d ns", ns+0)
}
/^test [^ ]+ \.\.\. bench:/ {
    name=$2
    n = split(name, parts, "/")
    fixture = parts[n]
    # Group is all-but-last, with duplicated segments collapsed
    # (criterion writes "query_geo/query_geo/sydney_cbd"); dedup
    # consecutive identical tokens.
    group=""
    last=""
    for (i=1; i<n; i++) {
        if (parts[i] != last) {
            group = (group == "" ? parts[i] : group "/" parts[i])
            last = parts[i]
        }
    }
    if (group != current_group) {
        if (current_group != "") print ""
        printf("### `%s`\n\n", group)
        print "| Fixture | Median | ±σ |"
        print "|---------|-------:|---:|"
        current_group = group
    }

    gsub(",", "", $5); ns=$5
    gsub(",", "", $8); sigma=$8
    printf("| `%s` | %s | %s |\n", fixture, fmt_time(ns), fmt_time(sigma))
}
' "$RAW" > "$TABLE"

cat > "$DOC_FILE" <<EOF
# Performance benchmarks

Machine-generated. Do not hand-edit — re-run \`scripts/run-benchmarks.sh\` to refresh.

- **Captured**:    $STAMP
- **Platform**:    $OS_NAME $OS_VERSION
- **CPU**:         $CPU
- **RAM**:         ${RAM_GB} GB
- **Compiler**:    $RUSTC
- **Index**:       $DATA_DIR (AU only)
- **Bench tool**:  criterion (warmup=${WARMUP}s, measure=${MEASURE}s)

## Reverse geocoding

Three benchmark groups, each re-run against the twelve fixtures
defined in \`server/benches/reverse_geocode.rs\`:

- **\`reverse_geocode/query\`** — full \`Index::query(lat, lng)\`:
  S2 cell lookup → admin polygon point-in-polygon → postcode
  enrichment → display-name formatting.
- **\`find_admin\`** — admin polygon resolution only (the historical
  hot spot). Useful to see how much of the full-query cost is
  polygon containment vs. formatting.
- **\`query_geo\`** — the low-level geo cell lookup without admin
  processing. Closest to "hash + mmap read" cost.

Fixtures mix dense-urban, suburban, rural, and no-hit coords so
medians capture both hot (dense admin cells) and cold (ocean —
polygon index returns quickly) paths.

EOF
cat "$TABLE" >> "$DOC_FILE"

cat >> "$DOC_FILE" <<'EOF'

## Notes

- Criterion's µs-to-ns threshold is 1000; sub-µs fixtures are
  rendered in ns to preserve precision.
- The cold-path fixtures (`tasman_sea`, `great_australian_bight`)
  measure empty-cell lookup — the index correctly bails without
  probing any polygon. The sub-µs number is mostly cache-resident
  hash math.
- Urban centroids hit the densest admin polygons (Sydney has
  hundreds of overlapping suburb boundaries). Rural coords scan a
  much smaller candidate set, which is why they run faster.

## How to re-run

```bash
# Defaults: ./data/index as the index dir, 1s warmup + 3s measurement.
./scripts/run-benchmarks.sh

# Longer runs produce tighter confidence intervals at the cost of time.
BENCH_MEASURE_SEC=10 BENCH_WARMUP_SEC=3 ./scripts/run-benchmarks.sh
```

Criterion also writes HTML reports under `target/criterion/` — open
`target/criterion/report/index.html` for per-fixture histograms and
change detection vs. the previous run.
EOF

rm -f "$RAW" "$TABLE"
echo "==> wrote $DOC_FILE"
