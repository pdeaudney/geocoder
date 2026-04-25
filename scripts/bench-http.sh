#!/bin/sh
# HTTP read-path bench runner — cold-cache and warm-cache.
#
# Boots the query-server in release mode against `--index DIR`, waits
# for /healthz, runs `scripts/bench/k6-readpath.js` via the official
# `grafana/k6` Docker image once with cold OS file cache and once
# warm, and tears the server down. Records both numbers because they
# answer different production questions:
#
#   * cold = "what does the first traffic after a deploy / restart /
#     scale-out event look like?" — relevant to user-visible
#     post-deploy latency.
#   * warm = "what does steady-state look like once page-cache is
#     populated?" — relevant to capacity planning, p99 SLOs, and
#     comparing optimisation deltas.
#
# Mode selection:
#   --mode cold    — drop OS cache, then measure
#   --mode warm    — pre-warm with curl traffic, then measure
#   --mode both    — both passes (default)
#
# Output:
#   tests/regression/reports/http-bench-<label>.json
# with shape:
#   { "label": ..., "git": ..., "captured": ...,
#     "modes": { "cold": {...}, "warm": {...} } }
#
# Diff against the most recent prior report compares cold→cold and
# warm→warm (matching modes only); mixing the two is what produced
# the misleading -240% numbers in our prior runs.
#
# Cold-cache invalidation:
#   * macOS: `sudo purge` (asks for password if not already cached)
#   * Linux: `sudo sync && echo 3 > /proc/sys/vm/drop_caches`
# When sudo isn't available, the cold pass falls back to "best
# effort" — reasonable on a fresh laptop, untrustworthy on a busy
# CI runner. The script prints a warning so the operator knows.
#
# Usage:
#   ./scripts/bench-http.sh [--index DIR] [--port N] [--skip-build]
#                           [--label NAME] [--mode cold|warm|both]
#
# Defaults: --index ./data/index --port 13580 --mode both
#           --label  <git short SHA>
set -eu

INDEX_DIR="./data/index"
PORT="13580"
SKIP_BUILD="0"
LABEL=""
MODE="both"

while [ $# -gt 0 ]; do
    case "$1" in
        --index)      INDEX_DIR="$2"; shift 2 ;;
        --port)       PORT="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD="1"; shift ;;
        --label)      LABEL="$2"; shift 2 ;;
        --mode)       MODE="$2"; shift 2 ;;
        *)            echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

case "$MODE" in
    cold|warm|both) ;;
    *) echo "error: --mode must be cold, warm, or both" >&2; exit 2 ;;
esac

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
# OS file-cache helpers
# -----------------------------------------------------------------------------

# Drop the OS page cache so the cold-cache pass starts honest.
# Returns 0 on success, 1 if best-effort only (sudo unavailable).
drop_os_caches() {
    case "$(uname -s)" in
        Darwin)
            if sudo -n purge 2>/dev/null; then
                echo "    dropped page cache via sudo purge"
                return 0
            elif sudo purge 2>/dev/null; then
                echo "    dropped page cache via sudo purge"
                return 0
            else
                echo "    WARN: sudo purge failed; cold pass will be partially warm"
                return 1
            fi
            ;;
        Linux)
            if sudo -n sh -c 'sync && echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null; then
                echo "    dropped page cache via drop_caches"
                return 0
            elif sudo sh -c 'sync && echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null; then
                echo "    dropped page cache via drop_caches"
                return 0
            else
                echo "    WARN: drop_caches failed (no sudo?); cold pass will be partially warm"
                return 1
            fi
            ;;
        *)
            echo "    WARN: unknown OS, can't drop caches; cold pass will be partially warm"
            return 1
            ;;
    esac
}

# Spread curl traffic across every endpoint + a representative sample
# of fixtures for ~30 s. The goal is to fault every page the bench's
# k6 scenarios will eventually touch into the OS file cache *and* to
# warm the JIT-style internal caches inside Tantivy.
warm_caches() {
    local base="$1"
    echo "==> warming OS page cache (~30s of curl traffic)"
    local end=$(($(date +%s) + 30))
    while [ $(date +%s) -lt $end ]; do
        # Reverse — touches admin, geo, addr files
        curl -s "$base/reverse?lat=-33.8688&lon=151.2093" >/dev/null
        curl -s "$base/reverse?lat=-37.8136&lon=144.9631" >/dev/null
        curl -s "$base/reverse?lat=-27.4698&lon=153.0251" >/dev/null
        curl -s "$base/reverse?lat=-31.9523&lon=115.8613" >/dev/null
        curl -s "$base/reverse?lat=-33.2833&lon=149.1000" >/dev/null
        curl -s "$base/reverse?lat=-33.8688&lon=151.2093&lang=zh" >/dev/null
        # Search — touches tantivy + FST
        curl -s "$base/search?q=sydney&country_code=au" >/dev/null
        curl -s "$base/search?q=melbourne&country_code=au" >/dev/null
        curl -s "$base/search?q=elizabeth%20street&country_code=au" >/dev/null
        # Autocomplete — touches FST
        curl -s "$base/autocomplete?q=s&country_code=au&limit=10" >/dev/null
        curl -s "$base/autocomplete?q=sydney&country_code=au&limit=10" >/dev/null
        curl -s "$base/autocomplete?q=bondi&country_code=au&limit=10" >/dev/null
    done
    echo "    warm-up complete"
}

# -----------------------------------------------------------------------------
# Server lifecycle
# -----------------------------------------------------------------------------
SERVER_PID=""
SCRATCH=""

start_server() {
    BIND="127.0.0.1:$PORT"
    LOG="$(mktemp -t http-bench-server.XXXXXX.log)"
    echo "==> starting query-server on $BIND (log: $LOG)"
    "$SERVER_BIN" "$INDEX_DIR" "$BIND" > "$LOG" 2>&1 &
    SERVER_PID=$!

    echo "==> waiting for /healthz on $BIND"
    local ready=0
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
        return 1
    fi
    echo "    ready after ${i}×0.5s"
}

stop_server() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -TERM "$SERVER_PID" 2>/dev/null || true
        for _ in 1 2 3 4 5; do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.5
        done
        kill -KILL "$SERVER_PID" 2>/dev/null || true
    fi
    SERVER_PID=""
}

cleanup() {
    rc=$?
    stop_server
    if [ -n "$SCRATCH" ] && [ -d "$SCRATCH" ]; then
        rm -rf "$SCRATCH"
    fi
    return "$rc"
}
trap cleanup EXIT INT TERM

# -----------------------------------------------------------------------------
# k6 in Docker
# -----------------------------------------------------------------------------
if [ "$(uname -s)" = "Linux" ]; then
    DOCKER_NET="--network=host"
    BASE_URL="http://localhost:$PORT"
else
    DOCKER_NET="--add-host=host.docker.internal:host-gateway"
    BASE_URL="http://host.docker.internal:$PORT"
fi

SCRATCH="$(mktemp -d -t k6-scratch.XXXXXX)"
chmod 777 "$SCRATCH"

# Run a single mode (cold or warm). Writes the per-mode summary to
# $SCRATCH/k6-summary-<mode>.json by passing MODE_TAG to the k6
# script (handleSummary keys the output filename off it).
run_k6_mode() {
    local mode="$1"
    echo "==> running k6 (mode=$mode, ~2 minutes)"
    docker run --rm \
        $DOCKER_NET \
        -v "$(pwd)/scripts/bench:/scripts:ro" \
        -v "$SCRATCH:/out" \
        -e "BASE_URL=$BASE_URL" \
        -e "MODE_TAG=$mode" \
        grafana/k6:latest \
        run /scripts/k6-readpath.js
}

# -----------------------------------------------------------------------------
# Driver
# -----------------------------------------------------------------------------

# We re-use a single server process for both cold and warm passes —
# the server itself is already loaded; what changes is the OS file
# cache state. For "cold" we drop the OS cache *while the server is
# running but idle*; for "warm" we run a curl-loop pre-warmer.

start_server || exit 1
BIND="127.0.0.1:$PORT"  # for warm_caches calls below

if [ "$MODE" = "cold" ] || [ "$MODE" = "both" ]; then
    echo "==> cold pass"
    drop_os_caches || true
    # Quick sanity hit so /healthz isn't on the cold-pass measurement
    curl -s "http://$BIND/healthz" >/dev/null || true
    run_k6_mode cold
fi

if [ "$MODE" = "warm" ] || [ "$MODE" = "both" ]; then
    echo "==> warm pass"
    warm_caches "http://$BIND"
    run_k6_mode warm
fi

# -----------------------------------------------------------------------------
# Merge + persist
# -----------------------------------------------------------------------------

# Merge per-mode JSONs into one report file with shape:
#   { label, git, captured, modes: { cold: {...}, warm: {...} } }
GIT_SHA="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
CAPTURED="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

if ! command -v jq > /dev/null 2>&1; then
    echo "warn: jq not found; reports won't be merged into a single file" >&2
    cp "$SCRATCH"/k6-summary-*.json "$REPORT_DIR/" 2>/dev/null || true
    echo
    echo "==> bench complete (per-mode JSONs in $REPORT_DIR/k6-summary-*.json)"
    exit 0
fi

merge_args=""
for mode in cold warm; do
    f="$SCRATCH/k6-summary-$mode.json"
    if [ -f "$f" ]; then
        merge_args="$merge_args --slurpfile $mode $f"
    fi
done

if [ -z "$merge_args" ]; then
    echo "error: no k6 summary files produced" >&2
    exit 1
fi

# shellcheck disable=SC2086
jq -n $merge_args \
    --arg label "$LABEL" \
    --arg git "$GIT_SHA" \
    --arg captured "$CAPTURED" \
    '{
        label: $label,
        git: $git,
        captured: $captured,
        modes: (
            (if $cold then {cold: $cold[0]} else {} end) +
            (if $warm then {warm: $warm[0]} else {} end)
        )
    }' > "$REPORT_FILE"
echo "==> wrote $REPORT_FILE"

# -----------------------------------------------------------------------------
# Diff vs prior report (cold→cold, warm→warm only)
# -----------------------------------------------------------------------------
PRIOR="$(ls -1t "$REPORT_DIR"/http-bench-*.json 2>/dev/null | grep -v "$LABEL" | head -1 || true)"
if [ -n "$PRIOR" ]; then
    echo
    echo "==> diff vs $(basename "$PRIOR") (matching mode only):"
    jq -n --slurpfile a "$PRIOR" --slurpfile b "$REPORT_FILE" '
        ["cold", "warm"] as $modes |
        $modes[] as $m |
        ($a[0].modes[$m] // null) as $A |
        ($b[0].modes[$m] // null) as $B |
        select($A != null and $B != null) |
        ($A.endpoints | keys) as $keys |
        $keys[] as $k |
        {
            mode: $m,
            endpoint: $k,
            p50_before: $A.endpoints[$k].med,
            p50_after:  $B.endpoints[$k].med,
            p50_pct: (if $A.endpoints[$k].med > 0
                      then (($B.endpoints[$k].med - $A.endpoints[$k].med) / $A.endpoints[$k].med * 100)
                      else 0 end),
            p95_before: $A.endpoints[$k].p95,
            p95_after:  $B.endpoints[$k].p95,
            p95_pct: (if $A.endpoints[$k].p95 > 0
                      then (($B.endpoints[$k].p95 - $A.endpoints[$k].p95) / $A.endpoints[$k].p95 * 100)
                      else 0 end)
        }
    '
fi

echo
echo "==> bench complete; server log at $LOG"
