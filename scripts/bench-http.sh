#!/bin/sh
# HTTP read-path bench runner.
#
# Boots the query-server in release mode against `--index DIR`, waits
# for /healthz, runs `scripts/bench/k6-readpath.js` via the official
# `grafana/k6` Docker image, then tears the server down.
#
# k6 in Docker keeps the host clean (no `brew install k6`) and uses
# the same image CI would use, so local numbers carry forward.
#
# Usage:
#   ./scripts/bench-http.sh [--index DIR] [--port N] [--skip-build] [--label NAME]
#
# Defaults:
#   --index   ./data/index
#   --port    13580
#   --label   <git short SHA>
#
# A JSON summary is written to:
#   tests/regression/reports/http-bench-<label>.json
# Subsequent runs at different commits can be diffed by comparing
# those JSON files; the script also writes a side-by-side diff vs the
# most-recent prior run when one exists.
set -eu

INDEX_DIR="./data/index"
PORT="13580"
SKIP_BUILD="0"
LABEL=""

while [ $# -gt 0 ]; do
    case "$1" in
        --index)      INDEX_DIR="$2"; shift 2 ;;
        --port)       PORT="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD="1"; shift ;;
        --label)      LABEL="$2"; shift 2 ;;
        *)            echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$LABEL" ]; then
    LABEL="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
fi

REPORT_DIR="tests/regression/reports"
mkdir -p "$REPORT_DIR"
REPORT_FILE="$REPORT_DIR/http-bench-${LABEL}.json"

# -----------------------------------------------------------------------------
# Sanity checks
# -----------------------------------------------------------------------------
if ! command -v docker > /dev/null 2>&1; then
    echo "error: docker not found in PATH; install Docker Desktop or supply k6 directly" >&2
    exit 2
fi
if ! docker info > /dev/null 2>&1; then
    echo "error: docker daemon not running" >&2
    exit 2
fi
if [ ! -d "$INDEX_DIR" ]; then
    echo "error: --index $INDEX_DIR is not a directory" >&2
    exit 2
fi

# -----------------------------------------------------------------------------
# Build (release)
# -----------------------------------------------------------------------------
if [ "$SKIP_BUILD" != "1" ]; then
    echo "==> building query-server (release)"
    cargo build --release --bin query-server
fi
SERVER_BIN="./target/release/query-server"
if [ ! -x "$SERVER_BIN" ]; then
    echo "error: $SERVER_BIN not found; rerun without --skip-build" >&2
    exit 2
fi

# -----------------------------------------------------------------------------
# Start server
# -----------------------------------------------------------------------------
BIND="127.0.0.1:$PORT"
LOG="$(mktemp -t http-bench-server.XXXXXX.log)"

echo "==> starting query-server on $BIND (log: $LOG)"
"$SERVER_BIN" "$INDEX_DIR" "$BIND" > "$LOG" 2>&1 &
SERVER_PID=$!

cleanup() {
    rc=$?
    if kill -0 "$SERVER_PID" 2>/dev/null; then
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

# Poll /healthz with a backoff. 30 s max — server load time on AU is
# ~3 s, planet is ~10–20 s; bigger indexes warrant a longer timeout
# but we keep the bench dev-friendly.
echo "==> waiting for /healthz on $BIND"
ready=0
for i in $(seq 1 60); do
    if curl -fs --max-time 1 "http://$BIND/healthz" > /dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 0.5
done
if [ "$ready" != "1" ]; then
    echo "error: server didn't pass /healthz within 30s" >&2
    echo "--- server log ---" >&2
    tail -40 "$LOG" >&2 || true
    exit 1
fi
echo "    ready after ${i}×0.5s"

# -----------------------------------------------------------------------------
# Run k6 in Docker
# -----------------------------------------------------------------------------
# `host.docker.internal` works on Docker Desktop (Mac/Windows). On
# native Linux we fall back to --network=host. Detect via OS.
if [ "$(uname -s)" = "Linux" ]; then
    DOCKER_NET="--network=host"
    BASE_URL="http://localhost:$PORT"
else
    DOCKER_NET="--add-host=host.docker.internal:host-gateway"
    BASE_URL="http://host.docker.internal:$PORT"
fi

# k6 writes the machine-readable summary to /out/k6-summary.json
# inside the container. Mount a host scratch dir there. /out is
# preferable to /tmp because k6's container /tmp may not be writable
# under the runtime user; an explicit out-mount is UID-friendly.
SCRATCH="$(mktemp -d -t k6-scratch.XXXXXX)"
chmod 777 "$SCRATCH"
trap 'rm -rf "$SCRATCH"; cleanup' EXIT INT TERM

echo "==> running k6 (this takes ~2 minutes for 5 scenarios × 20s each)"
docker run --rm \
    $DOCKER_NET \
    -v "$(pwd)/scripts/bench:/scripts:ro" \
    -v "$SCRATCH:/out" \
    -e "BASE_URL=$BASE_URL" \
    grafana/k6:latest \
    run /scripts/k6-readpath.js

# -----------------------------------------------------------------------------
# Persist + report
# -----------------------------------------------------------------------------
if [ -f "$SCRATCH/k6-summary.json" ]; then
    cp "$SCRATCH/k6-summary.json" "$REPORT_FILE"
    echo "==> wrote $REPORT_FILE"

    # Diff against the most recent prior report, if one exists.
    PRIOR="$(ls -1t "$REPORT_DIR"/http-bench-*.json 2>/dev/null | grep -v "$LABEL" | head -1 || true)"
    if [ -n "$PRIOR" ] && command -v jq > /dev/null 2>&1; then
        echo
        echo "==> diff vs $(basename "$PRIOR"):"
        jq -n --slurpfile a "$PRIOR" --slurpfile b "$REPORT_FILE" '
            $a[0].endpoints as $A | $b[0].endpoints as $B |
            ($A | keys) as $keys |
            $keys[] as $k |
            { endpoint: $k,
              p50_before: $A[$k].med, p50_after: $B[$k].med,
              p50_delta_pct: (if $A[$k].med > 0 then (($B[$k].med - $A[$k].med) / $A[$k].med * 100) else 0 end),
              p95_before: $A[$k].p95, p95_after: $B[$k].p95,
              p95_delta_pct: (if $A[$k].p95 > 0 then (($B[$k].p95 - $A[$k].p95) / $A[$k].p95 * 100) else 0 end)
            }
        '
    fi
else
    echo "warn: k6 summary file missing — see stdout above" >&2
fi

echo
echo "==> bench complete; server log at $LOG"
