#!/bin/sh
# Refresh the geocoder index from OSM diff replication without downtime.
#
# This script:
#   1. Downloads OSM diffs since the local PBF's timestamp
#   2. Applies them to the local PBF with `osmium apply-changes`
#   3. Rebuilds the index into a fresh directory
#   4. Atomically moves the new index into place
#   5. Touches the reload marker so the running server picks it up
#
# Prerequisites (install separately):
#   - osmium-tool:     brew install osmium-tool  (or apt: osmium-tool)
#   - pyosmium-get-changes: pip install osmium
#   - build-index binary at $BUILD_INDEX (defaults to ./build/build-index)
#
# Usage:
#   DATA_DIR=/data REPLICATION_URL=https://download.geofabrik.de/australia-oceania-updates \
#     ./scripts/update-index.sh
#
# Cron example (run nightly at 03:00):
#   0 3 * * * /path/to/update-index.sh >> /var/log/geocoder-update.log 2>&1
set -e

DATA_DIR="${DATA_DIR:-/data}"
PBF="${PBF:-$DATA_DIR/pbf/region-latest.osm.pbf}"
INDEX_DIR="${INDEX_DIR:-$DATA_DIR/index}"
INDEX_NEW="${INDEX_DIR}.next"
BUILD_INDEX="${BUILD_INDEX:-./build/build-index}"

# OSM replication URL. Use the Geofabrik updates URL for your region, or
# https://planet.openstreetmap.org/replication/day for planet.
REPLICATION_URL="${REPLICATION_URL:-}"

if [ -z "$REPLICATION_URL" ]; then
    echo "Error: set REPLICATION_URL (e.g. https://download.geofabrik.de/australia-oceania-updates)" >&2
    exit 1
fi
if [ ! -f "$PBF" ]; then
    echo "Error: PBF not found at $PBF" >&2
    exit 1
fi
if [ ! -x "$BUILD_INDEX" ]; then
    echo "Error: build-index not found or not executable at $BUILD_INDEX" >&2
    exit 1
fi

echo "[update-index] Fetching diffs since $(stat -f '%Sm' "$PBF" 2>/dev/null || stat -c '%y' "$PBF")"

CHANGES="$DATA_DIR/pbf/changes.osc.gz"
STATE="$DATA_DIR/pbf/.state.txt"

pyosmium-get-changes \
    --server "$REPLICATION_URL" \
    --start-osm-data "$PBF" \
    -f "$STATE" \
    -o "$CHANGES"

if [ ! -s "$CHANGES" ]; then
    echo "[update-index] No changes since last update; nothing to do."
    rm -f "$CHANGES"
    exit 0
fi

echo "[update-index] Applying changes to PBF"
PBF_NEW="${PBF}.next"
osmium apply-changes "$PBF" "$CHANGES" -o "$PBF_NEW" --overwrite
mv "$PBF_NEW" "$PBF"
rm -f "$CHANGES"

echo "[update-index] Rebuilding index into $INDEX_NEW"
rm -rf "$INDEX_NEW"
mkdir -p "$INDEX_NEW"
"$BUILD_INDEX" "$INDEX_NEW" "$PBF"

echo "[update-index] Moving new index into place"
# Move old aside, new in, delete old.
if [ -d "$INDEX_DIR" ]; then
    mv "$INDEX_DIR" "${INDEX_DIR}.old"
fi
mv "$INDEX_NEW" "$INDEX_DIR"
rm -rf "${INDEX_DIR}.old"

echo "[update-index] Touching reload marker"
touch "$INDEX_DIR/.reload"

echo "[update-index] Done. Server will pick up the new index within ~5s."
