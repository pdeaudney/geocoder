#!/bin/sh
# Run the full Pelias regression suite against a multi-country
# worldwide index.
#
# Assumes:
#   - Combined index already built at $INDEX_DIR (default ./data/index-worldwide)
#   - Pelias corpora already converted into per-country files at
#     tests/regression/corpora/pelias-{au,nz,gb,us,ca}-full.json
#
# Spins up one query-server against the combined index, iterates
# every pelias-*-full.json corpus, and aggregates the pass/fail totals.
# Exits non-zero if any corpus had zero passes (hard gate) — partial
# failures are expected and reported but don't fail the script.
set -eu

INDEX_DIR="${INDEX_DIR:-./data/index-worldwide}"
PORT="${PORT:-13600}"
CARGO="${CARGO:-cargo}"
REPORT_DIR="${REPORT_DIR:-./tests/regression/reports}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"

if [ ! -d "$INDEX_DIR" ]; then
    echo "error: index dir $INDEX_DIR not found" >&2
    exit 2
fi

mkdir -p "$REPORT_DIR"

SERVER_BIN="./target/release/query-server"
RUNNER_BIN="./target/release/regression-runner"
if [ ! -x "$SERVER_BIN" ] || [ ! -x "$RUNNER_BIN" ]; then
    $CARGO build --release -p query-server --bin query-server
    $CARGO build --release -p regression-runner --bin regression-runner
fi

echo "==> seeding regression auth token into $INDEX_DIR/geocoder.json"
./scripts/seed-test-token.sh "$INDEX_DIR"

BIND="127.0.0.1:$PORT"
LOG="$(mktemp -t worldwide-server.XXXXXX.log)"
echo "==> starting query-server on $BIND (log: $LOG)"
"$SERVER_BIN" "$INDEX_DIR" "$BIND" > "$LOG" 2>&1 &
SERVER_PID=$!
cleanup() {
    rc=$?
    if kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -TERM "$SERVER_PID" 2>/dev/null || true
        for _ in 1 2 3 4 5 6 7 8; do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.5
        done
        kill -KILL "$SERVER_PID" 2>/dev/null || true
    fi
    return "$rc"
}
trap cleanup EXIT INT TERM

URL="http://$BIND"
READY="0"
# Index is bigger now; allow more startup time.
for i in $(seq 1 30); do
    if curl -fsS "$URL/healthz" > /dev/null 2>&1; then
        READY="1"
        echo "==> server ready after ${i}s"
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "error: server exited during startup. Last 60 lines:" >&2
        tail -n 60 "$LOG" >&2
        exit 1
    fi
    sleep 1
done
if [ "$READY" != "1" ]; then
    echo "error: server did not become ready within 30 s" >&2
    tail -n 60 "$LOG" >&2
    exit 1
fi

# Report what's loaded so the summary makes sense.
echo "==> /healthz/indexes:"
curl -fsS "$URL/healthz/indexes" | python3 -m json.tool || true

TOTAL_PASSED=0
TOTAL_FAILED=0
TOTAL_CASES=0
COUNTRIES=""

for corpus in ./tests/regression/corpora/pelias-*-full.json; do
    [ -f "$corpus" ] || continue
    cc=$(basename "$corpus" | sed -E 's/^pelias-(.+)-full\.json$/\1/')
    report="$REPORT_DIR/pelias-$cc-$STAMP.json"
    echo ""
    echo "=================================================================="
    echo "== corpus: $cc ($corpus)"
    echo "=================================================================="

    set +e
    "$RUNNER_BIN" --corpus "$corpus" --base-url "$URL" --report "$report" --quiet
    rc=$?
    set -e

    # Parse summary from report.
    passed=$(python3 -c "import json; d=json.load(open('$report')); print(d['summary']['passed'])")
    failed=$(python3 -c "import json; d=json.load(open('$report')); print(d['summary']['failed'])")
    total=$(python3 -c "import json; d=json.load(open('$report')); print(d['summary']['total'])")
    wall=$(python3 -c "import json; d=json.load(open('$report')); print(d['summary']['wall_ms'])")

    printf "== %s: %d/%d passed (%dms)\n" "$cc" "$passed" "$total" "$wall"
    TOTAL_PASSED=$((TOTAL_PASSED + passed))
    TOTAL_FAILED=$((TOTAL_FAILED + failed))
    TOTAL_CASES=$((TOTAL_CASES + total))
    COUNTRIES="$COUNTRIES $cc"
    # $rc is intentionally ignored here — the runner returns non-zero on
    # any failure, but we want the aggregate view across every corpus.
done

echo ""
echo "=================================================================="
echo "== worldwide summary"
echo "=================================================================="
printf "  countries: %s\n" "$(echo "$COUNTRIES" | sed 's/^ //')"
printf "  total:     %d cases\n" "$TOTAL_CASES"
printf "  passed:    %d\n" "$TOTAL_PASSED"
printf "  failed:    %d\n" "$TOTAL_FAILED"
if [ "$TOTAL_CASES" -gt 0 ]; then
    pct=$(python3 -c "print(f'{$TOTAL_PASSED * 100 / $TOTAL_CASES:.1f}')")
    printf "  pass rate: %s%%\n" "$pct"
fi
echo "  reports:   $REPORT_DIR/pelias-*-$STAMP.json"
