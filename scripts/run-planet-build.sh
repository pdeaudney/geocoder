#!/usr/bin/env bash
# End-to-end planet build orchestrator.
#
# Wraps the existing fetch + build steps with restart-safety, per-stage
# timing, and pre-flight sanity checks. Designed for unattended overnight
# runs on a 128+ GB box; emits enough log + marker output that a kernel
# OOM kill or `Ctrl-C` can be diagnosed and the build resumed from the
# last completed step.
#
# Steps (run in order, each writes a `.done/<step>` marker on success):
#
#     1. preflight       — verify deps, disk, swap before starting
#     2. fetch-data      — OSM PBF + WhosOnFirst (skips OA, optional G-NAF)
#     3. build-binaries  — cmake build of C++ indexer + cargo release of Rust
#     4. build-index     — the long C++ reverse-index pass over the planet PBF
#     5. forward-index   — tantivy per-country (rayon-parallelised)
#     6. autocomplete    — FST builder
#     7. postcode-lookup — G-NAF postcodes (AU only, skipped if no G-NAF)
#     8. gnaf-index      — G-NAF address points (AU only, skipped if no G-NAF)
#     9. smoke           — boot the server, verify a coord per continent
#
# Resume semantics: re-running the script after partial completion picks
# up at the first not-yet-done step. To force a re-run of a step, delete
# its marker:
#
#     rm $DATA_DIR/.done/build-index
#     ./scripts/run-planet-build.sh
#
# Required env (the script will tell you if you forgot):
#
#     DATA_DIR              default ./data-planet — all artefacts live under here
#     GNAF_ARCHIVE_URL      optional; license-accepted G-NAF download URL.
#                           Without this, postcode-lookup + gnaf-index steps skip.
#
# Optional tuning:
#
#     TANTIVY_HEAP_MB       default 4096 — per-writer heap. 4 GB on 128 GB box.
#     SMOKE_PORT            default 13099 — local port for the smoke step.
#     PLANET_PBF            default 0 — opt back into the legacy single-stream
#                           planet PBF download (slower, less resumable; only
#                           useful if Geofabrik is unreachable). Default is to
#                           pull 9 continent extracts in parallel via
#                           --region all-continents.
#     FETCH_PARALLEL        default 4 — concurrent download streams when
#                           fetching continents. Honoured by fetch-build-data.sh.
#
# Exit codes:
#     0    everything completed (or already done from a previous run)
#     1    a step failed; see the matching log file under $DATA_DIR/logs/
#     2    pre-flight failed (missing deps / not enough disk / no swap)

set -euo pipefail

DATA_DIR="${DATA_DIR:-./data-planet}"
GNAF_ARCHIVE_URL="${GNAF_ARCHIVE_URL:-}"
TANTIVY_HEAP_MB="${TANTIVY_HEAP_MB:-4096}"
SMOKE_PORT="${SMOKE_PORT:-13099}"
PLANET_PBF="${PLANET_PBF:-0}"

DONE_DIR="$DATA_DIR/.done"
LOG_DIR="$DATA_DIR/logs"
PBF_DIR="$DATA_DIR/pbf"
INDEX_DIR="$DATA_DIR/index"

# Region argument handed to fetch-build-data.sh + the post-fetch sanity
# check. all-continents (default) downloads 9 Geofabrik continent
# extracts in parallel; planet pulls the single 80 GB stream from
# planet.osm.org for operators who explicitly opt in via PLANET_PBF=1.
if [ "$PLANET_PBF" = "1" ]; then
    FETCH_REGION="planet"
else
    FETCH_REGION="all-continents"
fi

mkdir -p "$DATA_DIR" "$DONE_DIR" "$LOG_DIR"

# ---------------------------------------------------------------------------
# Logging + step harness.
# ---------------------------------------------------------------------------

# Plain prefix for human reading; per-stage timings emit a separate
# `[stage] <name>: <secs>s` line at the end of each step so a CI grep
# matches the same convention the C++ builder + Rust builders use.
log()    { printf '\033[1;34m[planet-build]\033[0m %s\n' "$*"; }
warn()   { printf '\033[1;33m[planet-build]\033[0m WARN: %s\n' "$*" >&2; }
err()    { printf '\033[1;31m[planet-build]\033[0m ERROR: %s\n' "$*" >&2; }

# Track per-step elapsed times in arrays so we can print a summary at
# the end. Indices are step name → elapsed seconds.
declare -a SUMMARY_NAMES=()
declare -a SUMMARY_SECS=()
declare -a SUMMARY_STATUS=()

step_start_ts=0
current_step=""

step_start() {
    current_step="$1"
    step_start_ts=$(date +%s)
    log "── step ${current_step} ──"
}

step_done() {
    local end_ts elapsed
    end_ts=$(date +%s)
    elapsed=$((end_ts - step_start_ts))
    SUMMARY_NAMES+=("$current_step")
    SUMMARY_SECS+=("$elapsed")
    SUMMARY_STATUS+=("ok")
    touch "$DONE_DIR/$current_step"
    printf '[stage] %s: %ds\n' "$current_step" "$elapsed"
    log "step ${current_step} done in ${elapsed}s"
}

step_skipped() {
    SUMMARY_NAMES+=("$current_step")
    SUMMARY_SECS+=("0")
    SUMMARY_STATUS+=("skip")
    log "step ${current_step} skipped (already done — delete $DONE_DIR/${current_step} to re-run)"
}

step_skipped_intentionally() {
    SUMMARY_NAMES+=("$current_step")
    SUMMARY_SECS+=("0")
    SUMMARY_STATUS+=("not-applicable")
    log "step ${current_step} not applicable: $1"
}

is_done() {
    [ -f "$DONE_DIR/$1" ]
}

# Emit a final summary table on exit. Trapped on EXIT so even partial
# runs (Ctrl-C, OOM) print what completed before the kill.
print_summary() {
    if [ "${#SUMMARY_NAMES[@]}" -eq 0 ]; then
        return
    fi
    printf '\n──── planet-build summary ────\n'
    local total=0 i name secs status
    for i in "${!SUMMARY_NAMES[@]}"; do
        name="${SUMMARY_NAMES[$i]}"
        secs="${SUMMARY_SECS[$i]}"
        status="${SUMMARY_STATUS[$i]}"
        printf '  %-18s  %6ds  %s\n' "$name" "$secs" "$status"
        total=$((total + secs))
    done
    printf '  %-18s  %6ds\n' "TOTAL" "$total"
    printf '──────────────────────────────\n\n'
}

trap print_summary EXIT

# ---------------------------------------------------------------------------
# Step 1: preflight
# ---------------------------------------------------------------------------

run_preflight() {
    step_start "preflight"

    local missing=""
    for cmd in cmake make cargo curl unzip bzip2; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            missing="$missing $cmd"
        fi
    done
    if [ -n "$missing" ]; then
        err "missing required commands:$missing"
        err "see README 'Build from source' for install commands."
        exit 2
    fi

    # Disk: need ~350 GB free in the data directory's filesystem.
    local free_kb free_gb
    free_kb=$(df -Pk "$DATA_DIR" | awk 'NR==2 {print $4}')
    free_gb=$((free_kb / 1024 / 1024))
    if [ "$free_gb" -lt 350 ]; then
        err "only ${free_gb} GB free in $(df -Ph "$DATA_DIR" | awk 'NR==2 {print $6}'); planet build needs ~350 GB"
        exit 2
    fi
    log "disk: ${free_gb} GB free (need ~350 GB)"

    # Swap: warn (don't fail) if absent. The build's last write phase has
    # been the OOM landmine on smaller boxes; even 16 GB of swap turns a
    # kill into a slowdown.
    local swap_kb
    swap_kb=$(free -k 2>/dev/null | awk '/^Swap:/ {print $2}' || echo 0)
    if [ "${swap_kb:-0}" -lt $((4 * 1024 * 1024)) ]; then
        warn "less than 4 GB swap configured. Recommended: 32 GB swap as a safety net."
        warn "  sudo fallocate -l 32G /swapfile && sudo chmod 600 /swapfile"
        warn "  sudo mkswap /swapfile && sudo swapon /swapfile"
    else
        log "swap: $((swap_kb / 1024 / 1024)) GB configured"
    fi

    # GNAF presence — informational, not a failure.
    if [ -z "$GNAF_ARCHIVE_URL" ]; then
        log "G-NAF: GNAF_ARCHIVE_URL not set; postcode-lookup + gnaf-index steps will skip"
    else
        log "G-NAF: archive URL configured"
    fi

    # bzip2 decoder priority — informational. fetch-build-data.sh
    # auto-picks lbzip2 (best) → pbzip2 (better than baseline) → bzip2
    # (single-thread baseline). Surface which one is in play here so
    # operators know whether they're going to pay 5+ minutes for the
    # WoF SQLite decompression or 60 seconds.
    if command -v lbzip2 >/dev/null 2>&1; then
        log "bz2 decoder: lbzip2 (parallel; ~3–5× faster than bzip2 -d on WoF)"
    elif command -v pbzip2 >/dev/null 2>&1; then
        warn "bz2 decoder: pbzip2 (single-threaded on WoF's stock-bzip2 stream — install lbzip2 for ~3–5× speedup)"
    else
        warn "bz2 decoder: bzip2 (single-threaded; install lbzip2 to save ~5 min on the WoF decompression step)"
        warn "  Debian/Ubuntu: apt-get install lbzip2"
        warn "  macOS:         brew install lbzip2"
    fi

    log "preflight passed"
    step_done
}

# ---------------------------------------------------------------------------
# Step 2: fetch-data — delegates to fetch-build-data.sh, --skip-oa.
# ---------------------------------------------------------------------------

run_fetch_data() {
    step_start "fetch-data"
    log "region=$FETCH_REGION (set PLANET_PBF=1 for the legacy single-stream path)"
    DATA_DIR="$DATA_DIR" GNAF_ARCHIVE_URL="$GNAF_ARCHIVE_URL" \
        ./scripts/fetch-build-data.sh --region "$FETCH_REGION" --skip-oa \
        2>&1 | tee "$LOG_DIR/fetch-data.log"

    # Sanity: at least one PBF must exist under $PBF_DIR. Globbing here
    # rather than a fixed filename so both --region planet and
    # --region all-continents validate the same way.
    if ! ls -1 "$PBF_DIR"/*.osm.pbf >/dev/null 2>&1; then
        err "no PBFs found under $PBF_DIR — fetch-build-data.sh did not produce any"
        exit 1
    fi
    log "PBFs ready: $(ls -1 "$PBF_DIR"/*.osm.pbf | wc -l | tr -d ' ') file(s), $(du -sh "$PBF_DIR" | cut -f1) total"
    step_done
}

# ---------------------------------------------------------------------------
# Step 3: build-binaries — C++ indexer + Rust release.
# ---------------------------------------------------------------------------

run_build_binaries() {
    step_start "build-binaries"
    if [ ! -d build ]; then
        mkdir -p build
        ( cd build && cmake ../builder ) 2>&1 | tee "$LOG_DIR/cmake.log"
    fi
    ( cd build && make -j ) 2>&1 | tee "$LOG_DIR/make.log"
    cargo build --release --manifest-path server/Cargo.toml \
        2>&1 | tee "$LOG_DIR/cargo-build.log"
    step_done
}

# ---------------------------------------------------------------------------
# Step 4: build-index — the long C++ reverse-index pass.
# ---------------------------------------------------------------------------

run_build_index() {
    step_start "build-index"
    mkdir -p "$INDEX_DIR"
    # Capture the PBF list with shell glob expansion. build-index accepts
    # multiple positional PBF args and shares the admin/street tables
    # across them — exactly what we want for the multi-continent path.
    local pbf_count pbf_total
    pbf_count=$(ls -1 "$PBF_DIR"/*.osm.pbf 2>/dev/null | wc -l | tr -d ' ')
    pbf_total=$(du -ch "$PBF_DIR"/*.osm.pbf 2>/dev/null | awk '/total$/ {print $1}')
    log "starting build-index over $pbf_count PBF file(s), $pbf_total total"
    log "  (dominant phase; expect 12+ hours wall time on the planet scope)"
    log "  (per-stage timings stream into $LOG_DIR/build-index.log)"
    ./build/build-index "$INDEX_DIR" "$PBF_DIR"/*.osm.pbf 2>&1 | tee "$LOG_DIR/build-index.log"

    # Sanity-check the output: geo_cells.bin must exist and be non-empty.
    # The build-index tool exits 0 on partial output in some failure modes;
    # the file existence is the more reliable signal of "the write phase
    # actually finished."
    if [ ! -s "$INDEX_DIR/geo_cells.bin" ]; then
        err "build-index produced no geo_cells.bin under $INDEX_DIR — partial output, do not mark done"
        exit 1
    fi
    step_done
}

# ---------------------------------------------------------------------------
# Step 5: forward-index — tantivy per-country.
# ---------------------------------------------------------------------------

run_forward_index() {
    step_start "forward-index"
    ./server/target/release/build-forward-index "$INDEX_DIR" \
        --partition-by-country \
        --tantivy-heap-mb "$TANTIVY_HEAP_MB" \
        2>&1 | tee "$LOG_DIR/forward-index.log"
    step_done
}

# ---------------------------------------------------------------------------
# Step 6: autocomplete — FST builder.
# ---------------------------------------------------------------------------

run_autocomplete() {
    step_start "autocomplete"
    ./server/target/release/build-autocomplete-fst "$INDEX_DIR" --layout both \
        2>&1 | tee "$LOG_DIR/autocomplete.log"
    step_done
}

# ---------------------------------------------------------------------------
# Step 7+8: G-NAF (AU only) — skipped without GNAF_ARCHIVE_URL.
# ---------------------------------------------------------------------------

run_postcode_lookup() {
    step_start "postcode-lookup"
    if [ ! -d "$DATA_DIR/gnaf/psv" ] || [ -z "$(ls -A "$DATA_DIR/gnaf/psv" 2>/dev/null || true)" ]; then
        step_skipped_intentionally "no G-NAF PSV under $DATA_DIR/gnaf/psv"
        return
    fi
    ./server/target/release/build-postcode-lookup "$DATA_DIR/gnaf/psv" "$INDEX_DIR" \
        2>&1 | tee "$LOG_DIR/postcode-lookup.log"
    step_done
}

run_gnaf_index() {
    step_start "gnaf-index"
    if [ ! -d "$DATA_DIR/gnaf/psv" ] || [ -z "$(ls -A "$DATA_DIR/gnaf/psv" 2>/dev/null || true)" ]; then
        step_skipped_intentionally "no G-NAF PSV under $DATA_DIR/gnaf/psv"
        return
    fi
    ./server/target/release/build-gnaf-index "$DATA_DIR/gnaf/psv" "$INDEX_DIR" \
        2>&1 | tee "$LOG_DIR/gnaf-index.log"
    step_done
}

# ---------------------------------------------------------------------------
# Step 9: smoke — boot the server, hit known coords on a few continents.
# ---------------------------------------------------------------------------

run_smoke() {
    step_start "smoke"
    log "starting query-server on :$SMOKE_PORT for verification"
    # Disable shadow + OTLP in the smoke run; we just want the read path.
    GOOGLE_GEOCODING_ENABLED=false OTEL_TRACE_ENABLED=false OTEL_METRICS_ENABLED=false \
        ./server/target/release/query-server "$INDEX_DIR" "0.0.0.0:$SMOKE_PORT" \
        > "$LOG_DIR/smoke-server.log" 2>&1 &
    local server_pid=$!
    # Always tear down the server, even if smoke checks fail.
    trap 'kill '"$server_pid"' 2>/dev/null || true; wait '"$server_pid"' 2>/dev/null || true; print_summary' EXIT

    # Wait for the readiness probe to flip to 200.
    log "waiting for /healthz/ready..."
    local ready=""
    for _ in $(seq 1 60); do
        if curl -fsS --max-time 2 "http://127.0.0.1:$SMOKE_PORT/healthz/ready" >/dev/null 2>&1; then
            ready=ok
            break
        fi
        sleep 5
    done
    [ -n "$ready" ] || { err "/healthz/ready never returned 200"; kill "$server_pid"; exit 1; }

    local base="http://127.0.0.1:$SMOKE_PORT"
    local fail=0
    # One coord per major continent; the smoke script handles the
    # actual assertion shape. Skip continents where the fixture data
    # is unavailable in your build (e.g. if you've trimmed PBF).
    set +e
    ./scripts/smoke-test.sh "$base" au -33.8568 151.2153 || fail=$((fail+1))
    ./scripts/smoke-test.sh "$base" us 40.7128  -74.0060 || fail=$((fail+1))
    ./scripts/smoke-test.sh "$base" gb 51.5074   -0.1278 || fail=$((fail+1))
    ./scripts/smoke-test.sh "$base" de 52.5200   13.4050 || fail=$((fail+1))
    ./scripts/smoke-test.sh "$base" jp 35.6762  139.6503 || fail=$((fail+1))
    ./scripts/smoke-test.sh "$base" br -23.5505 -46.6333 || fail=$((fail+1))
    set -e

    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    trap print_summary EXIT

    if [ "$fail" -ne 0 ]; then
        err "$fail of 6 continent smoke checks failed"
        exit 1
    fi
    log "all 6 continent smoke checks passed"
    step_done
}

# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

run_step() {
    local name="$1" ; shift
    if is_done "$name"; then
        current_step="$name"
        step_skipped
        return
    fi
    "$@"
}

log "starting planet build: DATA_DIR=$DATA_DIR  TANTIVY_HEAP_MB=$TANTIVY_HEAP_MB  region=$FETCH_REGION"
log "logs land under $LOG_DIR/"

# preflight intentionally has no .done marker — re-run every invocation
# so disk-fill / dep-removal between runs gets caught.
run_preflight

run_step fetch-data       run_fetch_data
run_step build-binaries   run_build_binaries
run_step build-index      run_build_index
run_step forward-index    run_forward_index
run_step autocomplete     run_autocomplete
run_step postcode-lookup  run_postcode_lookup
run_step gnaf-index       run_gnaf_index
run_step smoke            run_smoke

log "planet build complete. Index ready at $INDEX_DIR"
