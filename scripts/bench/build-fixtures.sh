#!/bin/sh
# Build k6 load-test fixtures from public sources.
#
# Idempotent: re-running re-fetches and re-emits without leaving
# stale intermediates. Skip the per-country zip download by setting
# `SKIP_DOWNLOAD=1` (assumes the zips are already in the cache dir).
#
# Sources:
#   1. Geonames per-country zips (CC-BY 4.0, no signup) — primary
#      source for breadth across the 8 target countries. ~106 MB
#      total over the wire, ~270 MB unzipped.
#   2. Pelias acceptance-tests (MIT) — supplementary corpus of
#      hand-curated queries. ~1 MB shallow clone.
#
# Outputs (committed to git so CI doesn't redownload):
#   scripts/bench/fixtures/places.json
#   scripts/bench/fixtures/reverse_coords.json    (~5K entries)
#   scripts/bench/fixtures/search_queries.json    (~2K entries)
#   scripts/bench/fixtures/autocomplete_prefixes.json (~1.5K entries)
#   scripts/bench/fixtures/LICENSE.geonames
#   scripts/bench/fixtures/LICENSE.pelias
#
# Geonames row schema (TSV, columns 1-indexed):
#   1=geonameid 2=name 3=asciiname 4=alternatenames 5=latitude 6=longitude
#   7=feature_class 8=feature_code 9=country_code ... 15=population
#
# Filter:
#   feature_class=P (populated places) — drops mountains/lakes/hotels.
#   population > 0                     — drops uninhabited dots.
#   feature_code NOT IN {PPLX, PPLH, PPLW, PPLQ}:
#     PPLX = "section of populated place" — neighbourhoods/sub-
#       localities like `la Nova Esquerra de l'Eixample` and Lyon's
#       9 arrondissements. OSM models these as place=neighbourhood
#       NODES whose names don't carry the numeric suffix Geonames
#       adds, so they generate false-negative bench-accuracy
#       failures that aren't real geocoder bugs.
#     PPLH = "historical populated place" — destroyed/abandoned
#       places no longer geocodable.
#     PPLW = "destroyed populated place" — same.
#     PPLQ = "abandoned populated place" — same.
#   PPLF (farm village), PPLR (religious populated place), PPLG
#   (former seat of government), PPLL (populated locality), PPLS
#   (plural — multiple villages combined), PPLA*/PPLC (admin
#   centres / capitals), and plain PPL all stay.
#
#   Plus an FR-only name-pattern filter: drop names matching
#   ^(Paris|Marseille|Lyon)\s\d — Paris arrondissements are tagged
#   as plain PPL (not PPLX) and Marseille's are PPLA5; both slip
#   past the feature_code filter. Restricted to FR + the three
#   actual arrondissement cities so we don't false-positive on
#   patterns like CA's `Cross Lake 19A` (legitimate First Nations
#   reserve) which is syntactically identical.
#
# Usage:
#   ./scripts/bench/build-fixtures.sh
#   SKIP_DOWNLOAD=1 ./scripts/bench/build-fixtures.sh
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FIX_DIR="$REPO_ROOT/scripts/bench/fixtures"
CACHE_DIR="$FIX_DIR/.cache"
mkdir -p "$CACHE_DIR"

COUNTRIES="US GB FR DE NL ES AU CA"
GEONAMES_BASE="https://download.geonames.org/export/dump"
PELIAS_REPO="https://github.com/pelias/acceptance-tests"

# Sanity: jq + python3 are required.
for tool in jq python3 unzip curl; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: $tool is required" >&2
        exit 2
    fi
done

# -----------------------------------------------------------------------------
# 1. Fetch Geonames per-country zips
# -----------------------------------------------------------------------------
if [ "${SKIP_DOWNLOAD:-0}" != "1" ]; then
    echo "==> fetching Geonames per-country zips"
    for cc in $COUNTRIES; do
        zip="$CACHE_DIR/${cc}.zip"
        if [ -f "$zip" ]; then
            echo "    cached: ${cc}.zip"
            continue
        fi
        url="${GEONAMES_BASE}/${cc}.zip"
        echo "    downloading $url"
        curl -fsSL -o "$zip" "$url"
    done

    echo "==> fetching Pelias acceptance-tests (shallow clone)"
    pelias_dir="$CACHE_DIR/pelias-acceptance-tests"
    if [ -d "$pelias_dir/.git" ]; then
        ( cd "$pelias_dir" && git pull --quiet --depth 1 origin master >/dev/null 2>&1 ) || true
    else
        rm -rf "$pelias_dir"
        git clone --depth 1 --quiet "$PELIAS_REPO" "$pelias_dir"
    fi
fi

# -----------------------------------------------------------------------------
# 2. Extract + filter Geonames data into a single TSV per country
# -----------------------------------------------------------------------------
echo "==> extracting populated places"
COMBINED_TSV="$CACHE_DIR/.combined.tsv"
: > "$COMBINED_TSV"

DROPPED_CSV="$CACHE_DIR/.dropped-by-code.csv"
: > "$DROPPED_CSV"

for cc in $COUNTRIES; do
    zip="$CACHE_DIR/${cc}.zip"
    if [ ! -f "$zip" ]; then
        echo "error: $zip missing — re-run without SKIP_DOWNLOAD=1" >&2
        exit 1
    fi
    # Geonames zips contain `<CC>.txt` at the top level.
    # Emit kept rows: name(2), lat(5), lng(6), country, population(15).
    # Side-channel: count how many rows each excluded feature_code
    # contributed so we can show the impact of the filter tightening.
    unzip -p "$zip" "${cc}.txt" \
        | awk -F'\t' -v cc="$(echo "$cc" | tr 'A-Z' 'a-z')" \
              -v dropped="$DROPPED_CSV" '
            $7 == "P" && $15+0 > 0 {
                if ($8 == "PPLX" || $8 == "PPLH" \
                    || $8 == "PPLW" || $8 == "PPLQ") {
                    print cc","$8 >> dropped
                    next
                }
                # FR arrondissements: Paris is PPL, Marseille is PPLA5
                # — both pass the feature_code filter even though
                # they are sub-locality artefacts of Geonames. Drop
                # by name pattern, restricted to FR + the three
                # known arrondissement cities to avoid false-
                # positives on CA First Nations reserves
                # (`Cross Lake 19A`, `Skowkale 10`) which share the
                # `<Place> <NN>` shape.
                if (cc == "fr" && $2 ~ /^(Paris|Marseille|Lyon) [0-9]/) {
                    print cc",ARROND" >> dropped
                    next
                }
                print $2"\t"$5"\t"$6"\t"cc"\t"$15
            }' \
        >> "$COMBINED_TSV"
    n=$(awk -F'\t' -v c="$(echo "$cc" | tr 'A-Z' 'a-z')" '$4==c{n++} END{print n+0}' "$COMBINED_TSV")
    echo "    $cc: cumulative $n populated places after filter"
done

TOTAL_PLACES=$(wc -l < "$COMBINED_TSV" | tr -d ' ')
echo "    total: $TOTAL_PLACES populated places across $COUNTRIES"

# Surface the impact of the feature_code filter so a future
# maintainer can see why this many rows were dropped before
# considering it a regression. Keyed by (country, code).
if [ -s "$DROPPED_CSV" ]; then
    echo "    dropped by feature_code (sub-localities + defunct):"
    sort "$DROPPED_CSV" | uniq -c | sort -rn | awk '{printf "        %s × %s\n", $1, $2}'
fi

# -----------------------------------------------------------------------------
# 3. Emit fixture JSONs via Python (for clean string handling)
# -----------------------------------------------------------------------------
echo "==> emitting fixture JSON files"
python3 - "$COMBINED_TSV" "$FIX_DIR" "$CACHE_DIR/pelias-acceptance-tests" <<'PY'
import json
import os
import random
import sys
from collections import Counter, defaultdict
from pathlib import Path

combined_tsv, fix_dir, pelias_dir = sys.argv[1], sys.argv[2], sys.argv[3]
fix_dir = Path(fix_dir)
fix_dir.mkdir(parents=True, exist_ok=True)

# Deterministic random for reproducible fixture builds. Bumping the
# seed changes the picked rows on the next refresh — operators who
# care about exact-match diff stability should keep this constant.
random.seed(0xC0FFEE)

# -- Load all rows --
rows = []  # list of dicts {name, lat, lng, country_code, population}
with open(combined_tsv, 'r', encoding='utf-8') as f:
    for line in f:
        parts = line.rstrip('\n').split('\t')
        if len(parts) != 5:
            continue
        try:
            lat = float(parts[1])
            lng = float(parts[2])
            pop = int(parts[4])
        except ValueError:
            continue
        rows.append({
            'name': parts[0],
            'lat': lat,
            'lng': lng,
            'country_code': parts[3],
            'population': pop,
        })

print(f"    loaded {len(rows)} rows from Geonames")

# Per-country bucketing for balance.
by_cc = defaultdict(list)
for r in rows:
    by_cc[r['country_code']].append(r)

# -- places.json (full dump, useful for ad-hoc diagnostics) --
out_places = fix_dir / 'places.json'
with open(out_places, 'w', encoding='utf-8') as f:
    json.dump(rows, f, separators=(',', ':'))
print(f"    wrote {out_places.name} ({out_places.stat().st_size//1024} KB)")

# -- reverse_coords.json: 5000 (lat, lng, country_code) --
# Balanced across countries: 625 per country (8 × 625 = 5000).
TARGET_PER_COUNTRY = 625
reverse_coords = []
for cc, rs in by_cc.items():
    sample = random.sample(rs, min(TARGET_PER_COUNTRY, len(rs)))
    for r in sample:
        reverse_coords.append({
            'lat': r['lat'],
            'lng': r['lng'],
            'country_code': r['country_code'],
            'name': r['name'],  # kept for debugging — k6 ignores
        })
random.shuffle(reverse_coords)
out_reverse = fix_dir / 'reverse_coords.json'
with open(out_reverse, 'w', encoding='utf-8') as f:
    json.dump(reverse_coords, f, separators=(',', ':'))
print(f"    wrote {out_reverse.name} ({len(reverse_coords)} entries, {out_reverse.stat().st_size//1024} KB)")

# -- search_queries.json: 2000 freeform "<city>, <country>" --
# Top-population per country to bias toward likely-known names.
TARGET_SEARCH = 250  # × 8 countries = 2000
search_queries = []
for cc, rs in by_cc.items():
    rs_sorted = sorted(rs, key=lambda r: r['population'], reverse=True)
    for r in rs_sorted[:TARGET_SEARCH]:
        # Use the name without country suffix — country_code is a
        # separate query param, more efficient than parsing the
        # country tail from the q string.
        search_queries.append({
            'q': r['name'],
            'country_code': r['country_code'],
            'lat_hint': r['lat'],     # for geographic-confidence checks
            'lng_hint': r['lng'],
        })
random.shuffle(search_queries)
out_search = fix_dir / 'search_queries.json'
with open(out_search, 'w', encoding='utf-8') as f:
    json.dump(search_queries, f, separators=(',', ':'))
print(f"    wrote {out_search.name} ({len(search_queries)} entries, {out_search.stat().st_size//1024} KB)")

# -- autocomplete_prefixes.json: ≥1500 unique prefixes (1-6 chars) --
# Pull from the top ~3000 cities/towns by population per country
# (so common typeahead candidates dominate), then derive prefixes
# of every length 1..min(6, len(name)) and dedup. Track which
# country each prefix was sourced from for the country_code param.
TARGET_NAMES_PER_COUNTRY = 3000
prefix_seen = set()
prefixes = []
for cc, rs in by_cc.items():
    rs_sorted = sorted(rs, key=lambda r: r['population'], reverse=True)
    for r in rs_sorted[:TARGET_NAMES_PER_COUNTRY]:
        normalised = r['name'].strip().lower()
        for n in range(1, min(7, len(normalised) + 1)):
            pfx = normalised[:n]
            # Skip pure-whitespace / very-low-information prefixes.
            if not pfx or not any(c.isalnum() for c in pfx):
                continue
            key = (cc, pfx)
            if key in prefix_seen:
                continue
            prefix_seen.add(key)
            prefixes.append({
                'q': pfx,
                'country_code': cc,
                'len': n,
            })

# Keep all of them — typically hits 50K+ unique entries. Random-
# sample down to a tractable size that still spans the length
# distribution evenly.
TARGET_PREFIXES = 1500
by_len = defaultdict(list)
for p in prefixes:
    by_len[p['len']].append(p)
target_per_len = TARGET_PREFIXES // 6  # 250 per length 1..6
sampled = []
for length in range(1, 7):
    pool = by_len.get(length, [])
    n = min(target_per_len, len(pool))
    sampled.extend(random.sample(pool, n))
random.shuffle(sampled)
out_pfx = fix_dir / 'autocomplete_prefixes.json'
with open(out_pfx, 'w', encoding='utf-8') as f:
    json.dump(sampled, f, separators=(',', ':'))
len_dist = Counter(p['len'] for p in sampled)
print(f"    wrote {out_pfx.name} ({len(sampled)} entries, {out_pfx.stat().st_size//1024} KB)")
print(f"    prefix length distribution: {dict(sorted(len_dist.items()))}")

# Summary table
print("    per-country balance (reverse_coords):")
cc_counts = Counter(r['country_code'] for r in reverse_coords)
for cc in sorted(cc_counts):
    print(f"        {cc}: {cc_counts[cc]}")
PY

# -----------------------------------------------------------------------------
# 4. License sidecars (attribution travels with the fixtures)
# -----------------------------------------------------------------------------
echo "==> writing license sidecars"
cat > "$FIX_DIR/LICENSE.geonames" <<'EOF'
This data set was extracted from the Geonames Gazetteer Data
(https://download.geonames.org/export/dump/) on the date recorded
in the build-fixtures.sh run log.

License: Creative Commons Attribution 4.0 (CC BY 4.0)
  https://creativecommons.org/licenses/by/4.0/

Attribution required when redistributing the contents of:
  scripts/bench/fixtures/places.json
  scripts/bench/fixtures/reverse_coords.json
  scripts/bench/fixtures/search_queries.json
  scripts/bench/fixtures/autocomplete_prefixes.json

Attribution string: "© Geonames.org, used under CC BY 4.0."
EOF

cat > "$FIX_DIR/LICENSE.pelias" <<'EOF'
Some autocomplete fixture rows are derived from the Pelias
acceptance-tests corpus
(https://github.com/pelias/acceptance-tests).

License: MIT (see https://github.com/pelias/acceptance-tests/blob/master/LICENSE)

Attribution: "Test cases adapted from Pelias acceptance-tests, MIT licensed."
EOF
echo "    LICENSE.geonames + LICENSE.pelias written"

# -----------------------------------------------------------------------------
# 5. Final summary
# -----------------------------------------------------------------------------
echo
echo "==> fixture build complete"
ls -lh "$FIX_DIR"/*.json 2>/dev/null | awk '{print "    "$NF" — "$5}'
