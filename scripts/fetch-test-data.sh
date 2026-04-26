#!/bin/sh
# Fetch all external data needed to run the regression suite locally.
#
# Idempotent: everything lands under ./test-data/ (gitignored) and
# downloads are skipped when the target is already present.
#
# Sources:
#   - Pelias acceptance-tests (git clone, ~2 MB)
#   - Nominatim BDD feature files (git clone, ~1 MB)
#   - OSM PBFs per region (Geofabrik, on demand)
#   - OpenAddresses sample CSVs (downloads per --country, on demand)
#   - MaxMind GeoLite2-City       (license-gated; skipped if no key)
#   - G-NAF                       (license-acceptance-gated; skipped)
#
# Usage:
#   ./scripts/fetch-test-data.sh                 # fetch corpora only
#   ./scripts/fetch-test-data.sh --region au     # + OSM PBF for AU
#   ./scripts/fetch-test-data.sh --region europe # multi-region
#   ./scripts/fetch-test-data.sh --all-corpora   # every corpus source
#
# Environment:
#   TEST_DATA_DIR            default ./test-data
#   MAXMIND_LICENSE_KEY      needed to fetch GeoLite2-City
#   GNAF_ARCHIVE_URL         HTTPS URL to a G-NAF ZIP you've already
#                            license-accepted on data.gov.au
set -eu

TEST_DATA_DIR="${TEST_DATA_DIR:-./test-data}"

REGION=""
ALL_CORPORA=0

while [ $# -gt 0 ]; do
    case "$1" in
        --region)       REGION="$2"; shift 2 ;;
        --all-corpora)  ALL_CORPORA=1; shift ;;
        -h|--help)
            sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *)
            echo "unknown flag: $1" >&2; exit 2 ;;
    esac
done

mkdir -p "$TEST_DATA_DIR"

# -----------------------------------------------------------------------------
# Test corpora (always safe to fetch — small, open-licensed)
# -----------------------------------------------------------------------------

fetch_repo() {
    name="$1"; repo_url="$2"; subdir="$3"
    target="$TEST_DATA_DIR/$subdir"
    if [ -d "$target/.git" ]; then
        echo "==> $name corpus already present at $target (pulling latest)"
        git -C "$target" pull --ff-only --quiet || {
            echo "warning: $name pull failed; using existing snapshot" >&2
        }
    else
        echo "==> cloning $name into $target"
        git clone --depth 1 --quiet "$repo_url" "$target"
    fi
}

fetch_repo "pelias/acceptance-tests" \
    "https://github.com/pelias/acceptance-tests.git" \
    "pelias-acceptance-tests"

if [ "$ALL_CORPORA" = "1" ]; then
    fetch_repo "osm-search/Nominatim" \
        "https://github.com/osm-search/Nominatim.git" \
        "nominatim"
fi

# -----------------------------------------------------------------------------
# OSM PBF per region (delegates to the `fetch-data` Rust binary —
# conditional GET + resumable + MD5 verify built in)
# -----------------------------------------------------------------------------

if [ -n "$REGION" ]; then
    echo "==> ensuring OSM PBF for region=$REGION"
    if [ ! -x ./target/release/fetch-data ]; then
        echo "==> building fetch-data binary (one-time release compile)"
        cargo build --release --manifest-path server/Cargo.toml --bin fetch-data
    fi
    DATA_DIR="$TEST_DATA_DIR" ./target/release/fetch-data \
        --region "$REGION" --data-dir "$TEST_DATA_DIR"
fi

# -----------------------------------------------------------------------------
# License-gated sources — skip with a clear message when not configured
# -----------------------------------------------------------------------------

# -----------------------------------------------------------------------------
# Who's on First admin SQLite — country-level fallback polygons used when
# Geofabrik extracts miss their own `admin_level=2` relation (common for
# great-britain and us).
#
# WOF_COUNTRIES env var selects scope:
#   - "planet" (default): fetch the single combined admin SQLite (~8.6 GB
#     bz2) covering every country in WoF. Recommended for worldwide
#     deployments — Eurasia, Africa, Americas, Oceania, every polity.
#   - "none": skip WoF entirely.
#   - space-separated list of ISO 3166-1 alpha-2 codes (e.g. "au gb us"):
#     fetch per-country SQLite files only. Useful for narrow dev setups
#     where a ~200 MB-per-country download is cheaper than pulling the
#     whole planet.
# -----------------------------------------------------------------------------

WOF_COUNTRIES="${WOF_COUNTRIES:-planet}"
case "$WOF_COUNTRIES" in
    planet)
        db="$TEST_DATA_DIR/whosonfirst-data-admin-planet-latest.db"
        if [ ! -f "$db" ]; then
            echo "==> fetching whole-planet WoF admin SQLite (~8.6 GB bz2)"
            tmp_bz2="$TEST_DATA_DIR/.wof-planet.db.bz2"
            curl -fSL -o "$tmp_bz2" \
                "https://data.geocode.earth/wof/dist/sqlite/whosonfirst-data-admin-latest.db.bz2"
            bzip2 -d "$tmp_bz2"
            mv "${tmp_bz2%.bz2}" "$db"
            echo "    wrote $db ($(du -h "$db" | cut -f1))"
        fi
        ;;
    none)
        echo "==> skipping WoF (WOF_COUNTRIES=none)"
        ;;
    *)
        for cc in $WOF_COUNTRIES; do
            db="$TEST_DATA_DIR/whosonfirst-data-admin-${cc}-latest.db"
            if [ -f "$db" ]; then
                continue
            fi
            echo "==> fetching WoF admin SQLite for $cc"
            tmp_bz2="$TEST_DATA_DIR/.wof-${cc}.db.bz2"
            curl -fsSL -o "$tmp_bz2" \
                "https://data.geocode.earth/wof/dist/sqlite/whosonfirst-data-admin-${cc}-latest.db.bz2"
            bzip2 -d "$tmp_bz2"
            mv "${tmp_bz2%.bz2}" "$db"
            echo "    wrote $db ($(du -h "$db" | cut -f1))"
        done
        ;;
esac

if [ -n "${MAXMIND_LICENSE_KEY:-}" ]; then
    MMDB="$TEST_DATA_DIR/GeoLite2-City.mmdb"
    if [ ! -f "$MMDB" ]; then
        echo "==> fetching MaxMind GeoLite2-City (license-gated)"
        TMP="$(mktemp -d)"
        URL="https://download.maxmind.com/app/geoip_download?edition_id=GeoLite2-City&license_key=${MAXMIND_LICENSE_KEY}&suffix=tar.gz"
        curl -fsSL "$URL" | tar -xz -C "$TMP"
        find "$TMP" -name GeoLite2-City.mmdb -exec cp {} "$MMDB" \;
        rm -rf "$TMP"
        echo "    wrote $MMDB"
    fi
else
    echo "==> skipping MaxMind (set MAXMIND_LICENSE_KEY to fetch GeoLite2-City)"
fi

if [ -n "${GNAF_ARCHIVE_URL:-}" ]; then
    GNAF="$TEST_DATA_DIR/gnaf.zip"
    if [ ! -f "$GNAF" ]; then
        echo "==> fetching G-NAF archive (license-accepted by caller)"
        curl -fsSL -o "$GNAF" "$GNAF_ARCHIVE_URL"
        echo "    wrote $GNAF"
    fi
else
    echo "==> skipping G-NAF (set GNAF_ARCHIVE_URL to a license-accepted URL)"
fi

echo "==> done. test-data root: $TEST_DATA_DIR"
