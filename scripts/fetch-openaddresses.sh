#!/bin/sh
# Fetch OpenAddresses per-source cache.zip artifacts from the OA
# Requester-Pays S3 bucket. No account / API token needed — the
# caller just needs AWS credentials with the ability to pay for the
# downloads (standard EC2 instance profile, a user AWS key, or
# AWS_PROFILE pointing at a profile with `s3:GetObject` +
# `s3:ListBucket` on `s3://v2.openaddresses.io/*`).
#
# This is what the Packer worldwide-build job uses: the builder EC2
# instance already has an IAM role attached, and egress from
# `v2.openaddresses.io` (us-east-1) to the builder instance is pennies
# on a fleet basis ($0.02-0.09/GB depending on region; global scope
# is ~66 GB = $1-6 one-off).
#
# Usage:
#   ./scripts/fetch-openaddresses.sh [--output DIR] [--sources "au nz gb ..."]
#
# Env:
#   OA_OUTPUT_DIR    default: test-data/openaddresses
#   OA_SOURCES       default: "all" — every OA source globally.
#                    Space- or comma-separated alpha-2 codes for a
#                    narrower set (e.g. "au gb us fr de").
#
# AWS creds: looked up via the normal chain (env vars, profile,
# EC2 IMDS). Script exits cleanly with a warning when no creds are
# available so local dev setups don't trip.
set -eu

OUTPUT_DIR="${OA_OUTPUT_DIR:-test-data/openaddresses}"
SOURCES="${OA_SOURCES:-all}"

while [ $# -gt 0 ]; do
    case "$1" in
        --output)   OUTPUT_DIR="$2"; shift 2 ;;
        --sources)  SOURCES="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "unknown flag: $1" >&2; exit 2 ;;
    esac
done

# Verify we have AWS creds. On EC2 this is trivially true (IAM role
# via IMDS); on a dev laptop it's hit-or-miss. Skip gracefully when
# missing rather than producing a misleading 403.
if ! aws sts get-caller-identity >/dev/null 2>&1; then
    cat >&2 <<'EOF'
==> skipping OpenAddresses (no AWS credentials)

    OpenAddresses data lives in a Requester-Pays S3 bucket
    (s3://v2.openaddresses.io/). The caller's AWS account pays a
    small egress cost (pennies/GB) for the transfer. Configure
    credentials via any of:

      - AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY env vars
      - AWS_PROFILE pointing at a profile in ~/.aws/config
      - An EC2 instance profile (automatic in the Packer worldwide
        build — that's the intended execution environment)

    Without credentials we skip OA entirely. Worldwide builds still
    give full country-level coverage via OSM + WoF; OA only adds
    per-address precision for the ~60 countries it curates (US
    TIGER-backed, most EU countries, NZ side streets).
EOF
    exit 0
fi

mkdir -p "$OUTPUT_DIR"
API="https://batch.openaddresses.io/api"
# The cache.zip layout inside the Requester-Pays bucket. Validated by
# GETing one via the API (`/api/job/<id>/output/cache.zip` redirects
# to this path under the hood) then substituting the S3 URI. If OA
# reshuffles their bucket keys, this is the single line to change.
S3_BUCKET="s3://v2.openaddresses.io"
S3_CACHE_KEY="jobs/%s/cache.zip"
S3_GEOJSON_KEY="jobs/%s/source.geojson.gz"

# Normalise SOURCES. `all` = no filter (every OA source globally).
SOURCES="$(echo "$SOURCES" | tr ',' ' ')"
case "$SOURCES" in
    all) source_queries="" ;;
    *)   source_queries="$SOURCES" ;;
esac

download_job() {
    job=$1
    source=$2
    has_cache=$3

    safe="$(echo "$source" | tr '/' '_')"
    if [ "$has_cache" = "1" ]; then
        key="$(printf "$S3_CACHE_KEY" "$job")"
        target="$OUTPUT_DIR/${safe}.cache.zip"
        ext="cache.zip"
    else
        key="$(printf "$S3_GEOJSON_KEY" "$job")"
        target="$OUTPUT_DIR/${safe}.source.geojson.gz"
        ext="source.geojson.gz"
    fi

    if [ -f "$target" ]; then
        return
    fi

    printf "  fetch %-40s (job %s) %s\n" "$source" "$job" "$ext"
    # --request-payer requester: the caller's account pays the
    # egress cost. Required for any read on v2.openaddresses.io.
    if ! aws s3 cp --quiet --request-payer requester \
        "$S3_BUCKET/$key" "$target" 2>/tmp/oa-fetch-err; then
        echo "    failed ($(cat /tmp/oa-fetch-err | head -1))" >&2
        rm -f "$target"
    fi
}

iterate_sources() {
    # Per-source listing: `GET /api/data?source=<prefix>&layer=addresses`
    # returns the latest successful job per source whose prefix matches.
    # The API itself doesn't require auth for this read; only the
    # cache.zip download (via S3) does.
    prefix=$1
    url="${API}/data?layer=addresses"
    if [ -n "$prefix" ]; then
        url="${url}&source=${prefix}"
    fi
    list_json="$(mktemp -t oa-list.XXXXXX.json)"
    if ! curl -fsSL "$url" > "$list_json"; then
        echo "==> failed to list OA sources for prefix '$prefix'" >&2
        rm -f "$list_json"
        return 1
    fi
    # Extract (job_id, source, has_cache) tuples, one per line.
    python3 -c "
import json, sys
for row in json.load(open('$list_json')):
    job = row.get('job')
    if not job: continue
    source = row.get('source', '')
    has_cache = bool((row.get('output') or {}).get('cache'))
    print(f'{job}|{source}|{int(has_cache)}')
"
    rm -f "$list_json"
}

if [ -z "$source_queries" ]; then
    echo "==> listing all OpenAddresses sources (global)"
    rows="$(iterate_sources '')"
    if [ -n "$rows" ]; then
        echo "$rows" | while IFS='|' read -r job source has_cache; do
            download_job "$job" "$source" "$has_cache"
        done
    fi
else
    for src in $source_queries; do
        echo "==> listing OA sources matching '$src'"
        rows="$(iterate_sources "$src")"
        if [ -n "$rows" ]; then
            echo "$rows" | while IFS='|' read -r job source has_cache; do
                download_job "$job" "$source" "$has_cache"
            done
        fi
    done
fi

echo "==> OpenAddresses fetch done. artifacts under $OUTPUT_DIR"
du -sh "$OUTPUT_DIR" 2>/dev/null || true
