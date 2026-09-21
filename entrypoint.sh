#!/bin/sh
set -eu

DATA_DIR="${DATA_DIR:-/data}"
INDEX_DIR="$DATA_DIR/index"

has_file() {
    for file in "$@"; do
        [ -f "$file" ] && return 0
    done
    return 1
}

has_oa_csv() {
    [ -d "$DATA_DIR/openaddresses" ] &&
        [ -n "$(find "$DATA_DIR/openaddresses" -type f -name '*.csv' -print -quit)" ]
}
download_pbf() {
    mkdir -p "$DATA_DIR/pbf"
    if [ -z "${PBF_URLS:-}" ] && [ -n "${REGION:-}" ]; then
        DATA_DIR="$DATA_DIR" fetch-data --region "$REGION" --data-dir "$DATA_DIR"
        return
    fi
    for url in ${PBF_URLS:-}; do
        filename=$(basename "$url")
        if [ ! -f "$DATA_DIR/pbf/$filename" ]; then
            echo "Downloading $url..."
            curl -fSL -o "$DATA_DIR/pbf/$filename" "$url"
        else
            echo "Already downloaded: $filename"
        fi
    done
}

fetch_optional_sources() {
    # Scope WoF explicitly: its planet SQLite is large. Preloaded SQLite
    # files are also accepted without asking Docker to download them.
    if [ -n "${WOF_COUNTRIES:-}" ] && [ "$WOF_COUNTRIES" != "none" ]; then
        fetch-data --data-dir "$DATA_DIR" --wof --wof-postcodes --wof-countries "$WOF_COUNTRIES"
    fi
    if [ -n "${OA_GEOJSON_SOURCES:-}" ]; then
        # The public GeoJSON importer writes the per-country CSVs consumed
        # by build-openaddresses-index. Each operator selects source IDs
        # whose licence permits their use.
        old_ifs=$IFS
        IFS=' ,'
        set -f
        set -- $OA_GEOJSON_SOURCES
        set +f
        IFS=$old_ifs
        python3 /usr/local/bin/import-oa-geojson.py --output "$DATA_DIR/openaddresses" "$@"
    fi
    [ -z "${GNAF_ARCHIVE_URL:-}" ] || fetch-data --data-dir "$DATA_DIR" --gnaf
    [ "${MAXMIND_ENABLED:-0}" != "1" ] || fetch-data --data-dir "$DATA_DIR" --maxmind
}

inputs_newer_than_build() {
    marker="$INDEX_DIR/.docker-build-complete-v1"
    for file in "$DATA_DIR"/pbf/*.osm.pbf "$DATA_DIR"/whosonfirst-data-*.db "$DATA_DIR"/gnaf/psv/*.psv; do
        [ -f "$file" ] && [ "$file" -nt "$marker" ] && return 0
    done
    [ -d "$DATA_DIR/openaddresses" ] &&
        [ -n "$(find "$DATA_DIR/openaddresses" -type f -name '*.csv' -newer "$marker" -print -quit)" ]
}

index_current() {
    # Older volumes have no completion marker. A bare geo_cells.bin is not
    # enough: the C++ format changed and a killed build can leave partial files.
    [ -f "$INDEX_DIR/.docker-build-complete-v1" ] &&
        [ -s "$INDEX_DIR/geo_cells.bin" ] &&
        [ -s "$INDEX_DIR/manifest_reverse.json" ] &&
        grep -Eq '"version"[[:space:]]*:[[:space:]]*3' "$INDEX_DIR/manifest_reverse.json" &&
        [ -f "$INDEX_DIR/manifest_autocomplete.json" ] &&
        { [ "${FORWARD_INDEX:-1}" = "0" ] || [ -f "$INDEX_DIR/manifest_forward.json" ]; } &&
        ! inputs_newer_than_build
}

build_index() {
    set -- "$DATA_DIR"/pbf/*.osm.pbf
    if [ ! -f "$1" ]; then
        echo "Error: no PBF files found in $DATA_DIR/pbf/"
        exit 1
    fi

    # Build the complete generation in a sibling directory. A failed
    # optional stage must not replace a working index with mixed files.
    staged="$DATA_DIR/index.next"
    rm -rf "$staged"
    mkdir -p "$staged"
    [ -z "${STREET_LEVEL:-}" ] || set -- "$@" --street-level "$STREET_LEVEL"
    [ -z "${ADMIN_LEVEL:-}" ] || set -- "$@" --admin-level "$ADMIN_LEVEL"
    build-index "$staged" "$@"
    if [ ! -s "$staged/geo_cells.bin" ] || [ ! -s "$staged/manifest_reverse.json" ]; then
        echo "Error: build-index left an incomplete reverse index" >&2
        exit 1
    fi

    if has_file "$DATA_DIR"/whosonfirst-data-admin-*.db; then
        wof-importer "$DATA_DIR" "$staged"
    elif has_file "$DATA_DIR"/whosonfirst-data-postalcode-*.db; then
        wof-importer "$DATA_DIR" "$staged" --postcodes-only
    fi

    gnaf_loaded=0
    if has_file "$DATA_DIR"/gnaf/psv/*.psv; then
        build-postcode-lookup "$DATA_DIR/gnaf/psv" "$staged"
        build-gnaf-index "$DATA_DIR/gnaf/psv" "$staged"
        gnaf_loaded=1
    fi
    if has_oa_csv; then
        if [ "$gnaf_loaded" = "1" ]; then
            build-openaddresses-index "$DATA_DIR/openaddresses" "$staged"
        else
            # OA's builder skips AU by default only because G-NAF is
            # usually present. Keep AU OA points when it is absent.
            build-openaddresses-index "$DATA_DIR/openaddresses" "$staged" --skip ""
        fi
    fi

    # Forward search and autocomplete must run last: both consume the
    # optional G-NAF/OA points and autocomplete consumes WoF postcodes.
    if [ "${FORWARD_INDEX:-1}" != "0" ]; then
        if [ -n "${TANTIVY_HEAP_MB:-}" ]; then
            build-forward-index "$staged" --partition-by-country --tantivy-heap-mb "$TANTIVY_HEAP_MB"
        else
            build-forward-index "$staged" --partition-by-country
        fi
    fi
    build-autocomplete-fst "$staged" --layout both
    printf 'docker-index-v1\n' > "$staged/.docker-build-complete-v1"

    previous="$DATA_DIR/index.previous.$$"
    restore_previous() {
        if [ ! -e "$INDEX_DIR" ] && [ -e "$previous" ]; then
            mv "$previous" "$INDEX_DIR"
        fi
    }
    trap restore_previous EXIT HUP INT TERM
    if [ -e "$INDEX_DIR" ]; then mv "$INDEX_DIR" "$previous"; fi
    if ! mv "$staged" "$INDEX_DIR"; then
        [ ! -e "$previous" ] || mv "$previous" "$INDEX_DIR"
        exit 1
    fi
    [ ! -e "$previous" ] || rm -rf "$previous"
    trap - EXIT HUP INT TERM
    echo "Index built at $INDEX_DIR"
}

run_build() {
    download_pbf
    fetch_optional_sources
    if [ "$1" = "auto" ] && index_current; then
        echo "Index is current, skipping build"
    else
        build_index
    fi
}

serve() {
    set -- "$INDEX_DIR"
    if [ -n "${DOMAIN:-}" ]; then
        set -- "$@" --domain "$DOMAIN"
        if [ -n "${CACHE_DIR:-}" ]; then
            set -- "$@" --cache "$CACHE_DIR"
        fi
    else
        set -- "$@" "${BIND_ADDR:-0.0.0.0:3000}"
    fi
    [ -z "${STREET_LEVEL:-}" ] || set -- "$@" --street-level "$STREET_LEVEL"
    [ -z "${ADMIN_LEVEL:-}" ] || set -- "$@" --admin-level "$ADMIN_LEVEL"
    [ -z "${SEARCH_DISTANCE:-}" ] || set -- "$@" --search-distance "$SEARCH_DISTANCE"
    echo "Starting server..."
    exec query-server "$@"
}

case "${1:-auto}" in
    build)
        run_build build
        ;;
    serve)
        serve
        ;;
    auto)
        run_build auto
        serve
        ;;
    *)
        exec "$@"
        ;;
esac
