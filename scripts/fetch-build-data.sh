#!/bin/sh
# Fetch every external data source needed to build a geocoder index.
#
# Idempotent: existing files are detected by path and skipped — re-runs
# are safe and cheap. Output lands under `./data/` by default; override
# with `DATA_DIR=...`. None of the sources are baked into git, all are
# license-clean (CC-BY 4.0 or compatible) but two are license-acceptance
# gated and download only when the operator opts in via env var.
#
# What lands where (against the index format the build pipeline reads):
#
#   data/pbf/<region>-latest.osm.pbf            OSM, raw input for build-index
#   data/openaddresses/<cc>/*.zip               OpenAddresses, build-openaddresses-index
#   data/gnaf/psv/*_PSV/*.psv                   G-NAF, build-gnaf-index (AU only)
#   data/whosonfirst-data-admin-*.db            WhosOnFirst admin SQLite, build pipeline fallback
#   data/GeoLite2-City.mmdb                     MaxMind, runtime /geocode/ip
#
# Usage:
#   ./scripts/fetch-build-data.sh --region au                  # AU-only build
#   ./scripts/fetch-build-data.sh --region oceania             # AU + NZ
#   ./scripts/fetch-build-data.sh --region europe              # EU
#   ./scripts/fetch-build-data.sh --region all-continents      # planet via 9
#                                                                Geofabrik
#                                                                continent
#                                                                extracts in
#                                                                parallel
#                                                                (recommended)
#   ./scripts/fetch-build-data.sh --region planet              # planet as
#                                                                single 80 GB
#                                                                stream from
#                                                                planet.osm.org
#                                                                (legacy / slow)
#   ./scripts/fetch-build-data.sh --region au --skip-oa        # skip OpenAddresses
#   ./scripts/fetch-build-data.sh --region au --skip-wof       # skip WhosOnFirst
#
# Required deps on the build box:
#   curl, bzip2, unzip, awscli (only if fetching OpenAddresses)
#
# Optional but recommended:
#   lbzip2  — parallel bzip2 decoder. WoF planet SQLite ships as ~8.6 GB
#             of single-stream bzip2; lbzip2 decompresses it 3–5× faster
#             than stock bzip2 -d on a multi-core box. Wire-compatible
#             with bzip2 output. Auto-detected; falls back to bzip2 -d
#             when not installed.
#
# Env vars:
#   DATA_DIR             default ./data
#   FETCH_PARALLEL       default 4 — concurrent download streams when
#                        --region all-continents is selected. Higher on
#                        a fast cloud box; 4 is conservative for residential.
#   MAXMIND_LICENSE_KEY  needed for GeoLite2-City; skipped without it
#   GNAF_ARCHIVE_URL     HTTPS URL to a G-NAF ZIP you've already
#                        license-accepted on data.gov.au — skipped
#                        without it
#   OA_SOURCES           OpenAddresses scope, "all" (default) or
#                        space-separated alpha-2 codes (e.g. "au nz")
#   WOF_COUNTRIES        WoF admin scope, "planet" (default), "none",
#                        or space-separated alpha-2 codes
#
# Total downloads (no cache, all sources):
#   AU only:           ~1.5 GB   (PBF 800 MB + OA-AU 30 MB + WoF-au 50 MB
#                                 + GeoLite2 80 MB + G-NAF 6 GB if license-accepted)
#   all-continents:    ~80 GB    (continent PBFs ~70 GB + WoF planet 8.6 GB
#                                 uncompressed + GeoLite2 80 MB). ~1.5–2 h on
#                                 a residential connection with FETCH_PARALLEL=4;
#                                 much faster on a cloud box.
#   planet (single):   ~85 GB    (PBF 80 GB single stream from
#                                 planet.osm.org, frequently throttled to
#                                 1–3 MB/s — plan for 6–8 h).
set -eu

DATA_DIR="${DATA_DIR:-./data}"
REGION=""
SKIP_OA=0
SKIP_WOF=0
SKIP_OSM=0

while [ $# -gt 0 ]; do
    case "$1" in
        --region)    REGION="$2"; shift 2 ;;
        --skip-oa)   SKIP_OA=1; shift ;;
        --skip-wof)  SKIP_WOF=1; shift ;;
        --skip-osm)  SKIP_OSM=1; shift ;;
        -h|--help)
            sed -n '2,46p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *)
            echo "unknown flag: $1" >&2
            echo "see $0 --help" >&2
            exit 2 ;;
    esac
done

if [ -z "$REGION" ]; then
    echo "Error: --region is required (e.g. --region au, --region planet)" >&2
    echo "see $0 --help" >&2
    exit 2
fi

mkdir -p "$DATA_DIR"

# Pick the fastest available bzip2 decoder. lbzip2 parallelises bz2
# decompression by pipelining block decodes across cores; on the WoF
# planet 8.6 GB file the difference is roughly 3–5× wall-time saved.
# Wire-compatible with bzip2 output, so callers don't need to know.
if command -v lbzip2 >/dev/null 2>&1; then
    BZIP2_D="lbzip2 -d"
else
    BZIP2_D="bzip2 -d"
fi

decompress_bz2() {
    # Usage: decompress_bz2 <input.bz2>
    # Decompresses in-place (removes .bz2 suffix). Honours $BZIP2_D.
    $BZIP2_D "$1"
}

# -----------------------------------------------------------------------------
# 1. OSM PBF — the only mandatory source. Everything else degrades gracefully.
#
# `--region all-continents` fans the download across 9 Geofabrik continent
# extracts in parallel via `xargs -P`. Each extract is 30 MB to 30 GB; the
# parallel download from Geofabrik's CDN sustains much higher aggregate
# throughput than the single 80 GB stream from planet.osm.org. The build
# pipeline's pass 4 dedup handles the small amount of border overlap
# between adjacent continents (~5 % extra source bytes processed).
# -----------------------------------------------------------------------------

CONTINENTS="africa antarctica asia oceania central-america europe north-america russia south-america"
FETCH_PARALLEL="${FETCH_PARALLEL:-4}"

if [ "$SKIP_OSM" = "0" ]; then
    if [ "$REGION" = "all-continents" ]; then
        echo "==> fetching $(echo "$CONTINENTS" | wc -w | tr -d ' ') continent PBFs in parallel (FETCH_PARALLEL=$FETCH_PARALLEL)"
        # `printf '%s\n'` + xargs is POSIX and propagates non-zero exit
        # via the -P-aware xargs `--exit` flag where available. Each
        # download-region.sh invocation is independently idempotent
        # (skips already-downloaded files), so a re-run resumes cleanly
        # if one of the parallel slots failed.
        printf '%s\n' $CONTINENTS \
            | xargs -n 1 -P "$FETCH_PARALLEL" -I{} \
                ./scripts/download-region.sh {} "$DATA_DIR/pbf"
    else
        echo "==> fetching OSM PBF for region=$REGION"
        ./scripts/download-region.sh "$REGION" "$DATA_DIR/pbf"
    fi
else
    echo "==> skipping OSM (--skip-osm)"
fi

# -----------------------------------------------------------------------------
# 2. OpenAddresses — supplementary address points for ~60 countries.
#
#    **Requires AWS credentials.** OpenAddresses retired its free
#    HTTPS bulk-download mirror; the only programmatic path to the
#    processed GeoJSON is `s3://v2.openaddresses.io` with Requester-
#    Pays GetObject. The downloads are cheap (single-digit dollars
#    for the planet, ~$0.05 for AU); a free-tier AWS account is
#    sufficient. The browser UI at batch.openaddresses.io can
#    pre-sign URLs for manual downloads but isn't scriptable.
#
#    For deployments that can't use AWS at all:
#      - AU-only: skip OA entirely (`--skip-oa`); G-NAF is the
#        better address-points dataset for AU anyway.
#      - Other countries: OSM alone covers most use cases. /reverse
#        degrades gracefully without OA address points.
#      - Specific countries: pull from upstream sources directly via
#        the URLs in openaddresses/openaddresses sources/*.json
#        (per-source format adapter required; not generic).
#
#    This script skips on clean errors so dev setups without AWS
#    creds don't fail the whole pipeline — the build downstream
#    just won't have OA data to ingest.
# -----------------------------------------------------------------------------

if [ "$SKIP_OA" = "0" ]; then
    if command -v aws >/dev/null 2>&1; then
        echo "==> fetching OpenAddresses (sources=${OA_SOURCES:-all})"
        # fetch-openaddresses.sh defaults to test-data; redirect to data.
        OA_OUTPUT_DIR="$DATA_DIR/openaddresses" \
            ./scripts/fetch-openaddresses.sh \
            || echo "    (continuing without OpenAddresses — service degrades gracefully)"
    else
        echo "==> skipping OpenAddresses (awscli not installed)"
    fi
else
    echo "==> skipping OpenAddresses (--skip-oa)"
fi

# -----------------------------------------------------------------------------
# 3. WhosOnFirst admin SQLite — country-level admin polygon fallback for
#    countries where the Geofabrik extract is missing its own
#    admin_level=2 relation (great-britain, us, …). Without WoF those
#    countries' /reverse responses fall back to "country" being absent.
# -----------------------------------------------------------------------------

if [ "$SKIP_WOF" = "0" ]; then
    WOF_COUNTRIES="${WOF_COUNTRIES:-planet}"
    case "$WOF_COUNTRIES" in
        planet)
            db="$DATA_DIR/whosonfirst-data-admin-planet-latest.db"
            if [ ! -f "$db" ]; then
                echo "==> fetching planet-wide WoF admin SQLite (~8.6 GB bz2 → ~30 GB)"
                tmp_bz2="$DATA_DIR/.wof-planet.db.bz2"
                curl -fSL -o "$tmp_bz2" \
                    "https://data.geocode.earth/wof/dist/sqlite/whosonfirst-data-admin-latest.db.bz2"
                decompress_bz2 "$tmp_bz2"
                mv "${tmp_bz2%.bz2}" "$db"
                echo "    wrote $db ($(du -h "$db" | cut -f1))"
            else
                echo "==> WoF planet SQLite already present at $db"
            fi
            ;;
        none)
            echo "==> skipping WoF (WOF_COUNTRIES=none)"
            ;;
        *)
            for cc in $WOF_COUNTRIES; do
                db="$DATA_DIR/whosonfirst-data-admin-${cc}-latest.db"
                if [ -f "$db" ]; then
                    echo "==> WoF $cc SQLite already present at $db"
                    continue
                fi
                echo "==> fetching WoF admin SQLite for $cc"
                tmp_bz2="$DATA_DIR/.wof-${cc}.db.bz2"
                curl -fsSL -o "$tmp_bz2" \
                    "https://data.geocode.earth/wof/dist/sqlite/whosonfirst-data-admin-${cc}-latest.db.bz2"
                decompress_bz2 "$tmp_bz2"
                mv "${tmp_bz2%.bz2}" "$db"
                echo "    wrote $db ($(du -h "$db" | cut -f1))"
            done
            ;;
    esac
else
    echo "==> skipping WoF (--skip-wof)"
fi

# -----------------------------------------------------------------------------
# 4. MaxMind GeoLite2-City — license-gated; skipped without a license key.
#    Strictly optional: the /geocode/ip endpoint returns 503 without it.
# -----------------------------------------------------------------------------

if [ -n "${MAXMIND_LICENSE_KEY:-}" ]; then
    MMDB="$DATA_DIR/GeoLite2-City.mmdb"
    if [ ! -f "$MMDB" ]; then
        echo "==> fetching MaxMind GeoLite2-City (license-gated)"
        TMP="$(mktemp -d)"
        URL="https://download.maxmind.com/app/geoip_download?edition_id=GeoLite2-City&license_key=${MAXMIND_LICENSE_KEY}&suffix=tar.gz"
        curl -fsSL "$URL" | tar -xz -C "$TMP"
        find "$TMP" -name GeoLite2-City.mmdb -exec cp {} "$MMDB" \;
        rm -rf "$TMP"
        echo "    wrote $MMDB"
    else
        echo "==> MaxMind already present at $MMDB"
    fi
else
    echo "==> skipping MaxMind (set MAXMIND_LICENSE_KEY at https://www.maxmind.com/en/geolite2/signup)"
fi

# -----------------------------------------------------------------------------
# 5. G-NAF — Australian Geocoded National Address File. License must be
#    accepted manually at data.gov.au; we only fetch a known-accepted URL
#    the operator hands us. The build step expects the unzipped PSV
#    directory under data/gnaf/psv/.
# -----------------------------------------------------------------------------

if [ -n "${GNAF_ARCHIVE_URL:-}" ]; then
    GNAF_ZIP="$DATA_DIR/gnaf/gnaf.zip"
    GNAF_PSV="$DATA_DIR/gnaf/psv"
    if [ ! -d "$GNAF_PSV" ] || [ -z "$(ls -A "$GNAF_PSV" 2>/dev/null)" ]; then
        echo "==> fetching G-NAF archive (license-accepted by caller)"
        mkdir -p "$DATA_DIR/gnaf"
        if [ ! -f "$GNAF_ZIP" ]; then
            curl -fsSL -o "$GNAF_ZIP" "$GNAF_ARCHIVE_URL"
        fi
        echo "==> unzipping G-NAF into $GNAF_PSV"
        mkdir -p "$GNAF_PSV"
        # G-NAF archives ship as a nested directory tree of state-level
        # PSV files; flatten everything PSV-shaped into psv/.
        unzip -q -o "$GNAF_ZIP" -d "$DATA_DIR/gnaf/.unpack"
        find "$DATA_DIR/gnaf/.unpack" -name '*.psv' -exec cp {} "$GNAF_PSV/" \;
        rm -rf "$DATA_DIR/gnaf/.unpack"
        echo "    extracted $(find "$GNAF_PSV" -name '*.psv' | wc -l | tr -d ' ') PSV files"
    else
        echo "==> G-NAF PSV already present at $GNAF_PSV"
    fi
else
    echo "==> skipping G-NAF (set GNAF_ARCHIVE_URL to a license-accepted URL)"
fi

echo
echo "==> done. data root: $DATA_DIR"
echo "    PBFs available:"
ls -1sh "$DATA_DIR/pbf/" 2>/dev/null | tail -n +2 | sed 's/^/      /' || echo "      (none)"
echo "    next steps:"
echo "      cd build && cmake ../builder && make && cd .."
echo "      cargo build --release --manifest-path server/Cargo.toml"
echo "      ./build/build-index $DATA_DIR/index $DATA_DIR/pbf/*.osm.pbf"
echo "      ./server/target/release/build-forward-index $DATA_DIR/index --partition-by-country"
echo "      ./server/target/release/build-autocomplete-fst $DATA_DIR/index"
