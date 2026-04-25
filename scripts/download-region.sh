#!/bin/sh
# Download OpenStreetMap PBF files for a named region from Geofabrik.
#
# Usage: download-region.sh [region] [output-dir]
#   region      oceania (default; full Australia/Oceania continent — AU,
#               NZ, Fiji, PNG, Vanuatu, Solomon Is, New Caledonia,
#               Cook Is, Samoa, Tonga, Kiribati, etc.),
#               australia, new-zealand (sub-region extracts),
#               africa, antarctica, asia, europe, north-america,
#               south-america, central-america, russia, usa, planet
#   output-dir  destination directory (default: ./pbf or $DATA_DIR/pbf)
set -e

GEOFABRIK="https://download.geofabrik.de"
PLANET="https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf"

region="${1:-oceania}"
out_dir="${2:-${DATA_DIR:+$DATA_DIR/pbf}}"
out_dir="${out_dir:-./pbf}"

case "$region" in
    oceania|australia-oceania)
                     urls="$GEOFABRIK/australia-oceania-latest.osm.pbf" ;;
    australia)       urls="$GEOFABRIK/australia-oceania/australia-latest.osm.pbf" ;;
    new-zealand)     urls="$GEOFABRIK/australia-oceania/new-zealand-latest.osm.pbf" ;;
    africa)          urls="$GEOFABRIK/africa-latest.osm.pbf" ;;
    antarctica)      urls="$GEOFABRIK/antarctica-latest.osm.pbf" ;;
    asia)            urls="$GEOFABRIK/asia-latest.osm.pbf" ;;
    europe)          urls="$GEOFABRIK/europe-latest.osm.pbf" ;;
    north-america)   urls="$GEOFABRIK/north-america-latest.osm.pbf" ;;
    south-america)   urls="$GEOFABRIK/south-america-latest.osm.pbf" ;;
    central-america) urls="$GEOFABRIK/central-america-latest.osm.pbf" ;;
    russia)          urls="$GEOFABRIK/russia-latest.osm.pbf" ;;
    usa)             urls="$GEOFABRIK/north-america/us-latest.osm.pbf" ;;
    planet)          urls="$PLANET" ;;
    -h|--help|help)
        sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "Error: unknown region '$region'" >&2
        echo "Run '$0 --help' for supported regions." >&2
        exit 1
        ;;
esac

mkdir -p "$out_dir"
for url in $urls; do
    filename=$(basename "$url")
    dest="$out_dir/$filename"
    if [ -f "$dest" ]; then
        echo "Already downloaded: $filename"
    else
        echo "Downloading $url -> $dest"
        curl -fSL -o "$dest" "$url"
    fi
done

echo "$urls"
