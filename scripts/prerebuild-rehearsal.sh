#!/usr/bin/env bash
# Pre-rebuild rehearsal — build the index against a small PBF in a
# scratch directory, then spot-check the outputs. Catches format /
# wiring bugs that survive unit tests but would only surface after
# the 13-hour planet rebuild.
#
# Usage:
#   ./scripts/prerebuild-rehearsal.sh <small.pbf> [scratch-dir]
#
# Defaults:
#   scratch-dir   ./data-rehearsal
#
# Recommended PBF size: a single small region (NSW, Catalonia, North
# Rhine-Westphalia) takes 5–15 minutes end-to-end on a laptop, exercises
# every emit path, and surfaces enough places/exonyms/POIs to validate.
# Whole continents work too but add nothing for the rehearsal.
#
# Exit codes:
#   0   build clean, all spot-checks pass
#   1   build failed
#   2   spot-check failed (format/wiring drift detected)
#   3   missing pre-requisite (PBF, builder binary, etc.)
set -euo pipefail

PBF="${1:-}"
SCRATCH="${2:-./data-rehearsal}"

if [ -z "$PBF" ]; then
    echo "usage: $0 <small.pbf> [scratch-dir]" >&2
    exit 3
fi
if [ ! -f "$PBF" ]; then
    echo "error: PBF not found: $PBF" >&2
    exit 3
fi

# Use absolute paths so step output is unambiguous in logs.
PBF=$(cd "$(dirname "$PBF")" && pwd)/$(basename "$PBF")
SCRATCH=$(mkdir -p "$SCRATCH" && cd "$SCRATCH" && pwd)
INDEX_DIR="$SCRATCH/index"

log()  { printf '\033[1;34m[rehearsal]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[rehearsal]\033[0m WARN: %s\n' "$*" >&2; }
err()  { printf '\033[1;31m[rehearsal]\033[0m ERROR: %s\n' "$*" >&2; }

# ---------------------------------------------------------------------------
# Step 1: build the binaries we'll exercise.
# ---------------------------------------------------------------------------

log "Step 1/4: build C++ + Rust binaries"
make builder >/dev/null
cargo build --release --bin build-forward-index --bin index-dumper >/dev/null

if [ ! -x ./build/build-index ]; then
    err "build/build-index missing after make builder — check the build log"
    exit 1
fi

# ---------------------------------------------------------------------------
# Step 2: reverse index from the PBF.
# ---------------------------------------------------------------------------

log "Step 2/4: build reverse index from $PBF"
mkdir -p "$INDEX_DIR"
./build/build-index "$PBF" "$INDEX_DIR" 2>&1 | tail -3

if [ ! -f "$INDEX_DIR/place_points.bin" ]; then
    err "place_points.bin missing — reverse build did not complete"
    exit 1
fi

# ---------------------------------------------------------------------------
# Step 3: forward index (validates the tantivy schema + importance flow).
# ---------------------------------------------------------------------------

log "Step 3/4: build forward index (per-country tantivy — mirrors planet path)"
./target/release/build-forward-index "$INDEX_DIR" --partition-by-country 2>&1 | tail -3

if ! ls "$INDEX_DIR"/tantivy* >/dev/null 2>&1; then
    err "no tantivy directory after forward build — schema mismatch?"
    exit 1
fi

# ---------------------------------------------------------------------------
# Step 4: dump CSVs and run spot checks.
# ---------------------------------------------------------------------------

log "Step 4/4: dump CSVs + run spot checks"
DUMP_DIR="$INDEX_DIR/dump-csv"
./target/release/index-dumper "$INDEX_DIR" "$DUMP_DIR" >/dev/null

PLACE_CSV="$DUMP_DIR/place_points.csv"
I18N_CSV="$DUMP_DIR/i18n_names.csv"

fail=0

# --- Check A: place_points.csv exists and the new `importance` column
# is wired up. The header alone proves the dumper compiled against the
# updated PlacePoint struct; at least one row with importance > 0
# proves the C++ builder is actually populating the field.
if [ ! -f "$PLACE_CSV" ]; then
    err "place_points.csv missing — dumper failed silently?"
    fail=1
elif ! head -1 "$PLACE_CSV" | grep -q importance; then
    err "place_points.csv header is missing 'importance' — dumper out of sync"
    fail=1
else
    nonzero_importance=$(awk -F, 'NR>1 && $4 > 0 { c++ } END { print c+0 }' "$PLACE_CSV")
    total_places=$(awk -F, 'NR>1 { c++ } END { print c+0 }' "$PLACE_CSV")
    log "place_points: $total_places rows, $nonzero_importance with importance > 0"
    if [ "$nonzero_importance" = "0" ] && [ "$total_places" != "0" ]; then
        err "every place_point has importance=0 — population/wikidata/wikipedia signals are not being read"
        fail=1
    fi
fi

# --- Check B: i18n_names.csv exists and contains at least one
# synthetic English exonym alias. If the PBF includes any city in
# kEnglishExonyms (München, Wien, Roma, Praha, Москва, ...), the
# build emits `name:en` aliases for them. We grep for the well-known
# English forms; even one match proves the wiring works.
if [ ! -f "$I18N_CSV" ]; then
    warn "i18n_names.csv missing — exonym pipeline can't be checked"
else
    total_i18n=$(awk -F, 'NR>1 { c++ } END { print c+0 }' "$I18N_CSV")
    log "i18n_names: $total_i18n rows total"
    # Grep for well-known English exonym forms. Any match means the
    # exonym table fired against this PBF.
    exonym_hits=$(grep -ciE 'Munich|Cologne|Vienna|Moscow|Beijing|Tokyo|Cairo|Prague|Warsaw|Florence|Naples|Rome' "$I18N_CSV" || true)
    log "exonym-shaped i18n rows: $exonym_hits"
    if [ "$exonym_hits" = "0" ]; then
        warn "no exonym-shaped names found — either the PBF doesn't cover any exonym source, or the table isn't firing"
        warn "  (this is expected for AU/NZ/UK-only PBFs; investigate only if the input includes DE/IT/RU/CN/JP)"
    fi
fi

# --- Check C: manifest sanity. git_dirty=true means the build
# captured an uncommitted tree; not an error per se, but worth
# surfacing so operators don't ship a phantom-SHA index.
for kind in reverse forward; do
    M="$INDEX_DIR/manifest_${kind}.json"
    [ -f "$M" ] || continue
    sha=$(grep -o '"git_sha":[^,}]*' "$M" | head -1)
    dirty=$(grep -o '"git_dirty":[^,}]*' "$M" | head -1)
    log "manifest_${kind}: $sha, $dirty"
done

if [ "$fail" != "0" ]; then
    err "spot-checks failed — DO NOT START THE 13h REBUILD until these are resolved"
    exit 2
fi

log "OK — all spot-checks passed; safe to proceed with the planet rebuild"
