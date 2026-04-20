#!/bin/sh
# Run the black-box regression suite against a locally-built index.
#
# Responsibilities:
#   1. Seed a test auth token in <index-dir>/geocoder.json
#   2. Start query-server listening on a chosen port
#   3. Poll /healthz until the server is ready (or timeout)
#   4. Invoke the regression-runner against the corpus
#   5. Stop the server, return the runner's exit code
#
# Usage:
#   ./scripts/run-regression.sh [--index DIR] [--corpus FILE] [--port N] [--skip-build]
#
# Defaults:
#   --index   ./data/index
#   --corpus  ./tests/regression/corpora/au-regression.json
#   --port    13579
#
# Environment:
#   CARGO         cargo binary (default: cargo)
#   RELEASE       set to 0 to build+run debug profile (default release)
#   QUIET         set to 1 to suppress per-case runner output
#   REPORT_DIR    where to write the JSON report
#                 (default: tests/regression/reports/)
set -eu

INDEX_DIR="./data/index"
CORPUS="./tests/regression/corpora/au-regression.json"
PORT="13579"
SKIP_BUILD="0"
CARGO="${CARGO:-cargo}"
RELEASE="${RELEASE:-1}"
QUIET="${QUIET:-0}"
REPORT_DIR="${REPORT_DIR:-./tests/regression/reports}"

while [ $# -gt 0 ]; do
    case "$1" in
        --index)      INDEX_DIR="$2"; shift 2 ;;
        --corpus)     CORPUS="$2";    shift 2 ;;
        --port)       PORT="$2";      shift 2 ;;
        --skip-build) SKIP_BUILD="1"; shift ;;
        -h|--help)
            sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *)
            echo "unknown flag: $1" >&2; exit 2 ;;
    esac
done

if [ ! -d "$INDEX_DIR" ]; then
    echo "error: index dir $INDEX_DIR not found — build the index first" >&2
    echo "       (see README.md and BUILD-DEPLOY.md)" >&2
    exit 2
fi
if [ ! -f "$CORPUS" ]; then
    echo "error: corpus $CORPUS not found" >&2
    exit 2
fi

PROFILE_FLAG=""
TARGET_SUBDIR="debug"
if [ "$RELEASE" != "0" ]; then
    PROFILE_FLAG="--release"
    TARGET_SUBDIR="release"
fi

mkdir -p "$REPORT_DIR"

# -----------------------------------------------------------------------------
# Build binaries
# -----------------------------------------------------------------------------
if [ "$SKIP_BUILD" = "0" ]; then
    echo "==> building query-server + regression-runner (profile=$TARGET_SUBDIR)"
    $CARGO build $PROFILE_FLAG -p query-server --bin query-server >&2
    $CARGO build $PROFILE_FLAG -p regression-runner --bin regression-runner >&2
fi

SERVER_BIN="./target/$TARGET_SUBDIR/query-server"
RUNNER_BIN="./target/$TARGET_SUBDIR/regression-runner"
if [ ! -x "$SERVER_BIN" ] || [ ! -x "$RUNNER_BIN" ]; then
    echo "error: expected binaries not found under ./target/$TARGET_SUBDIR — run without --skip-build" >&2
    exit 2
fi

# -----------------------------------------------------------------------------
# Seed auth token
# -----------------------------------------------------------------------------
echo "==> seeding regression auth token into $INDEX_DIR/geocoder.json"
./scripts/seed-test-token.sh "$INDEX_DIR"

# -----------------------------------------------------------------------------
# Start server
# -----------------------------------------------------------------------------
BIND="127.0.0.1:$PORT"
LOG="$(mktemp -t regression-server.XXXXXX.log)"

echo "==> starting query-server on $BIND (log: $LOG)"
"$SERVER_BIN" "$INDEX_DIR" "$BIND" > "$LOG" 2>&1 &
SERVER_PID=$!

cleanup() {
    # Preserve the script's outgoing exit code. A trap on EXIT can
    # overwrite $? with the trap function's last command's status, so
    # capture and re-return explicitly at the end.
    rc=$?
    if kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -TERM "$SERVER_PID" 2>/dev/null || true
        # Give it ~2 s to shut down cleanly; SIGKILL if stubborn.
        for _ in 1 2 3 4; do
            if ! kill -0 "$SERVER_PID" 2>/dev/null; then
                break
            fi
            sleep 0.5
        done
        kill -KILL "$SERVER_PID" 2>/dev/null || true
    fi
    return "$rc"
}
trap cleanup EXIT INT TERM

# Poll /healthz until the server responds or we give up.
URL="http://$BIND"
READY="0"
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    if curl -fsS "$URL/healthz" > /dev/null 2>&1; then
        READY="1"
        echo "==> server ready after ${i}s"
        break
    fi
    # If the server has already exited, bail — polling further is pointless.
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "error: server exited during startup. Last 40 log lines:" >&2
        tail -n 40 "$LOG" >&2
        exit 1
    fi
    sleep 1
done
if [ "$READY" != "1" ]; then
    echo "error: server did not become ready within 15 s" >&2
    tail -n 40 "$LOG" >&2
    exit 1
fi

# -----------------------------------------------------------------------------
# Run the suite
# -----------------------------------------------------------------------------
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
REPORT_FILE="$REPORT_DIR/au-$STAMP.json"

QUIET_FLAG=""
if [ "$QUIET" = "1" ]; then
    QUIET_FLAG="--quiet"
fi

echo "==> running regression suite"
set +e
"$RUNNER_BIN" \
    --corpus "$CORPUS" \
    --base-url "$URL" \
    --report "$REPORT_FILE" \
    $QUIET_FLAG
RC=$?
set -e

echo "==> report: $REPORT_FILE"
exit $RC
