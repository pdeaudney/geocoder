#!/bin/sh
# Exercise Docker's build orchestration without downloading multi-GB inputs.
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
mkdir -p "$scratch/bin" "$scratch/data/pbf" "$scratch/data/gnaf/psv" "$scratch/data/openaddresses/us"
touch "$scratch/data/pbf/test.osm.pbf" "$scratch/data/whosonfirst-data-admin-au-latest.db"
touch "$scratch/data/whosonfirst-data-postalcode-au-latest.db" "$scratch/data/gnaf/psv/NSW_STATE_psv.psv"
printf 'LON,LAT,NUMBER,STREET\n' > "$scratch/data/openaddresses/us/test.csv"

cat > "$scratch/bin/mock" <<'EOF'
#!/bin/sh
name=$(basename "$0")
printf '%s\n' "$name" >> "$CALLS"
case "$name" in
    build-index)
        [ "${FAIL_BUILD:-0}" != 1 ] || exit 1
        mkdir -p "$1"
        printf 'cells\n' > "$1/geo_cells.bin"
        printf '{"schema":{"version":3}}\n' > "$1/manifest_reverse.json"
        ;;
    build-forward-index) printf '{}\n' > "$1/manifest_forward.json" ;;
    build-autocomplete-fst) printf '{}\n' > "$1/manifest_autocomplete.json" ;;
esac
EOF
chmod +x "$scratch/bin/mock"
for command in fetch-data build-index wof-importer build-postcode-lookup build-gnaf-index \
    build-openaddresses-index build-forward-index build-autocomplete-fst query-server; do
    ln -s mock "$scratch/bin/$command"
done

export CALLS="$scratch/calls" DATA_DIR="$scratch/data" PATH="$scratch/bin:$PATH"
export WOF_COUNTRIES=au GNAF_ARCHIVE_URL=https://example.invalid/gnaf.zip MAXMIND_ENABLED=1
sh "$repo/entrypoint.sh" build
cat > "$scratch/expected" <<'EOF'
fetch-data
fetch-data
fetch-data
build-index
wof-importer
build-postcode-lookup
build-gnaf-index
build-openaddresses-index
build-forward-index
build-autocomplete-fst
EOF
diff -u "$scratch/expected" "$CALLS"
[ -f "$DATA_DIR/index/.docker-build-complete-v1" ]

# auto reuses the complete generation, but a missing marker forces a rebuild.
sh "$repo/entrypoint.sh" auto
[ "$(grep -c '^build-index$' "$CALLS")" = 1 ]
rm "$DATA_DIR/index/.docker-build-complete-v1"
sh "$repo/entrypoint.sh" auto
[ "$(grep -c '^build-index$' "$CALLS")" = 2 ]

# An unsuccessful replacement leaves the previous generation in place.
FAIL_BUILD=1 sh "$repo/entrypoint.sh" build >/dev/null 2>&1 && exit 1
[ -f "$DATA_DIR/index/.docker-build-complete-v1" ]
echo 'Docker build mode: OK'
