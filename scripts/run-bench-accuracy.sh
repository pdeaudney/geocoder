#!/bin/sh
# Bench-fixture accuracy run.
#
# Companion to scripts/bench-http.sh: same corpus, but checks
# correctness of responses instead of latency. Boots query-server
# against `--index DIR`, fires the bench-accuracy tool against it,
# tears down. Runs in two passes (reverse + search + autocomplete)
# from the JSONs at scripts/bench/fixtures/ — same data the planet
# load test uses.
#
# This is the regression-suite analogue of
# `./scripts/bench-http.sh --workload planet`. The Pelias-style
# regression suite (`scripts/run-regression.sh`) handles
# hand-curated, address-level expectations; this one handles the
# at-scale "does the geocoder return the right country / a result
# near the hint / a result that matches the prefix" sweep.
#
# Output: JSON report at $REPORT_DIR/bench-accuracy-<label>.json,
# plus a human-readable summary on stdout. Exits 0 when the
# overall pass rate ≥ --pass-threshold (default 95 %), 1 otherwise.
#
# Usage:
#   ./scripts/run-bench-accuracy.sh [--index DIR] [--port N] [--label NAME]
#                                   [--sample N] [--scenarios r,s,a]
#                                   [--search-radius-km N]
#                                   [--pass-threshold 0.95]
#                                   [--skip-build]
#
# Defaults: --index ./data/index --port 13586 --sample 500
#           --scenarios r,s,a --search-radius-km 100
#           --pass-threshold 0.95
#           --label  <git short SHA>
set -eu

INDEX_DIR="./data/index"
PORT="13586"
SKIP_BUILD="0"
LABEL=""
SAMPLE="500"
SCENARIOS="r,s,a"
SEARCH_RADIUS_KM="200"
PASS_THRESHOLD="0.90"
FIXTURES_DIR="./scripts/bench/fixtures"

while [ $# -gt 0 ]; do
    case "$1" in
        --index)             INDEX_DIR="$2"; shift 2 ;;
        --port)              PORT="$2"; shift 2 ;;
        --skip-build)        SKIP_BUILD="1"; shift ;;
        --label)             LABEL="$2"; shift 2 ;;
        --sample)            SAMPLE="$2"; shift 2 ;;
        --scenarios)         SCENARIOS="$2"; shift 2 ;;
        --search-radius-km)  SEARCH_RADIUS_KM="$2"; shift 2 ;;
        --pass-threshold)    PASS_THRESHOLD="$2"; shift 2 ;;
        --fixtures)          FIXTURES_DIR="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$LABEL" ]; then
    LABEL="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
fi

# Sanity checks
if [ ! -d "$INDEX_DIR" ]; then
    echo "error: --index $INDEX_DIR is not a directory" >&2
    exit 2
fi
for f in reverse_coords.json search_queries.json autocomplete_prefixes.json; do
    if [ ! -f "$FIXTURES_DIR/$f" ]; then
        echo "error: $FIXTURES_DIR/$f missing" >&2
        echo "       run ./scripts/bench/build-fixtures.sh first" >&2
        exit 2
    fi
done

REPORT_DIR="${REPORT_DIR:-./tests/regression/reports}"
mkdir -p "$REPORT_DIR"
REPORT_FILE="$REPORT_DIR/bench-accuracy-${LABEL}.json"

# Build (release)
if [ "$SKIP_BUILD" != "1" ]; then
    echo "==> building query-server + bench-accuracy (release)"
    cargo build --release --bin query-server
    cargo build --release -p regression-runner --bin bench-accuracy
fi
SERVER_BIN="./target/release/query-server"
ACC_BIN="./target/release/bench-accuracy"
if [ ! -x "$SERVER_BIN" ] || [ ! -x "$ACC_BIN" ]; then
    echo "error: missing release binaries; rerun without --skip-build" >&2
    exit 2
fi

# Server lifecycle
SERVER_PID=""
SERVER_LOG=""
cleanup() {
    rc=$?
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -TERM "$SERVER_PID" 2>/dev/null || true
        for _ in 1 2 3 4 5; do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.5
        done
        kill -KILL "$SERVER_PID" 2>/dev/null || true
    fi
    return "$rc"
}
trap cleanup EXIT INT TERM

BIND="127.0.0.1:$PORT"
SERVER_LOG="$(mktemp -t bench-accuracy-server.XXXXXX.log)"
echo "==> starting query-server on $BIND (log: $SERVER_LOG)"
"$SERVER_BIN" "$INDEX_DIR" "$BIND" > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

echo "==> waiting for /healthz/ready"
ready=0
for i in $(seq 1 60); do
    if curl -fs --max-time 1 "http://$BIND/healthz/ready" > /dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 0.5
done
if [ "$ready" != "1" ]; then
    echo "error: server didn't pass /healthz/ready within 30s" >&2
    echo "--- server log ---" >&2
    tail -40 "$SERVER_LOG" >&2 || true
    exit 1
fi
echo "    ready"

echo "==> running bench-accuracy (sample=$SAMPLE, scenarios=$SCENARIOS, threshold=$PASS_THRESHOLD)"
set +e
"$ACC_BIN" "$FIXTURES_DIR" \
    --base-url "http://$BIND" \
    --sample "$SAMPLE" \
    --scenarios "$SCENARIOS" \
    --search-radius-km "$SEARCH_RADIUS_KM" \
    --pass-threshold "$PASS_THRESHOLD" \
    --out "$REPORT_FILE"
RC=$?
set -e

echo
echo "==> report at $REPORT_FILE"
exit $RC
