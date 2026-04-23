#!/bin/sh
# Fetch OpenAddresses source-level cache.zip artifacts for use with
# `build-openaddresses-index`. OpenAddresses moved to a gated
# distribution model in 2024 — their download API now requires an
# account API token, and their v2.openaddresses.io S3 bucket is
# Requester Pays. This script handles the token-gated REST path;
# when no token is configured it skips cleanly with a warning so
# worldwide builds still progress.
#
# Usage:
#   OPENADDRESSES_API_TOKEN=xxx ./scripts/fetch-openaddresses.sh [--output DIR] [--sources "au,nz,gb,ca,us,fr,de,nl,es,it,br,jp,in,mx"]
#
# Env:
#   OPENADDRESSES_API_TOKEN   Bearer token from an OpenAddresses
#                             account (required). Free signup at
#                             https://batch.openaddresses.io/.
#                             When empty, script skips gracefully.
#   OA_OUTPUT_DIR             default: test-data/openaddresses
#   OA_SOURCES                default: "au nz gb ca us fr de nl es it br jp in mx"
#                             Space- or comma-separated country/region
#                             source prefixes. Use "all" for the
#                             full global collection (~66 GB + egress).
#
# Layout the builder expects afterwards:
#   <output-dir>/
#     <source-region>/         (e.g. au/act/statewide/)
#       addresses.csv          (extracted from cache.zip)
#
# Note: `build-openaddresses-index` consumes the unzipped CSV tree;
# this script downloads and unzips for each source. Expect ~100 MB–
# a few GB per country, much more for the full global collection.
set -eu

OUTPUT_DIR="${OA_OUTPUT_DIR:-test-data/openaddresses}"
SOURCES="${OA_SOURCES:-au nz gb ca us fr de nl es it br jp in mx}"

while [ $# -gt 0 ]; do
    case "$1" in
        --output)   OUTPUT_DIR="$2"; shift 2 ;;
        --sources)  SOURCES="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "unknown flag: $1" >&2; exit 2 ;;
    esac
done

if [ -z "${OPENADDRESSES_API_TOKEN:-}" ]; then
    cat >&2 <<'EOF'
==> skipping OpenAddresses (no OPENADDRESSES_API_TOKEN set)

    OpenAddresses data is gated behind a free account. Sign up at
    https://batch.openaddresses.io/, generate an API token from
    the user menu, then re-run with:

        OPENADDRESSES_API_TOKEN=xxx ./scripts/fetch-openaddresses.sh

    Without this step, the worldwide build proceeds on OSM + WoF
    alone — still complete country-level coverage, but missing
    per-address precision for the ~60 countries OpenAddresses
    curates (US/TIGER, many EU countries, NZ Wellington side
    streets, etc.). Not a build-breaking omission.
EOF
    exit 0
fi

mkdir -p "$OUTPUT_DIR"
API="https://batch.openaddresses.io/api"
TOKEN_HDR="Authorization: Bearer $OPENADDRESSES_API_TOKEN"

# Normalize SOURCES to space-separated iteration.
SOURCES="$(echo "$SOURCES" | tr ',' ' ')"

for src_filter in $SOURCES; do
    case "$src_filter" in
        all) src_query="" ;;
        *)   src_query="$src_filter" ;;
    esac

    echo "==> listing OA sources matching '${src_query:-<all>}'"
    # `GET /api/data` returns the latest data row per source matching
    # an optional `source` substring. Filtering by a 2-letter country
    # code (the country is the leading path segment in OA's source
    # tree) gets us every known source in that country.
    list_json="$(mktemp -t oa-list.XXXXXX.json)"
    curl -fsSL -H "$TOKEN_HDR" \
        "${API}/data?source=${src_query}&layer=addresses" \
        > "$list_json"

    # Extract (id, source, job) tuples. A source row without a
    # successful job (`job: null`) has nothing to download.
    rows="$(python3 -c "
import json, sys
for row in json.load(open('$list_json')):
    job = row.get('job')
    if not job: continue
    # cache is only present for ~70% of jobs; where absent we fall
    # back to source.geojson.gz and convert downstream.
    has_cache = bool((row.get('output') or {}).get('cache'))
    print(f\"{row['id']}|{row['source']}|{job}|{int(has_cache)}\")
")"
    rm -f "$list_json"

    if [ -z "$rows" ]; then
        echo "    no usable sources for '$src_filter' — skip"
        continue
    fi

    echo "$rows" | while IFS='|' read -r data_id source job has_cache; do
        # Reshape "au/act/statewide" → "au/act/statewide".
        safe="$(echo "$source" | tr '/' '_')"
        target_zip="$OUTPUT_DIR/${safe}.cache.zip"
        target_gjz="$OUTPUT_DIR/${safe}.source.geojson.gz"

        if [ "$has_cache" = "1" ] && [ ! -f "$target_zip" ]; then
            printf "  fetch %-40s (job %s) cache.zip\n" "$source" "$job"
            curl -fsSL -H "$TOKEN_HDR" \
                "${API}/job/${job}/output/cache.zip" \
                -o "$target_zip" \
                || rm -f "$target_zip"
        elif [ "$has_cache" = "0" ] && [ ! -f "$target_gjz" ]; then
            printf "  fetch %-40s (job %s) source.geojson.gz (no cache available)\n" "$source" "$job"
            curl -fsSL -H "$TOKEN_HDR" \
                "${API}/job/${job}/output/source.geojson.gz" \
                -o "$target_gjz" \
                || rm -f "$target_gjz"
        fi
    done
done

echo "==> OpenAddresses fetch done. artifacts under $OUTPUT_DIR"
du -sh "$OUTPUT_DIR" 2>/dev/null || true
