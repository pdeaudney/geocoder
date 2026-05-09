#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <set>
#include <stdexcept>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

#include <osmium/handler.hpp>
#include <osmium/io/pbf_input.hpp>
#include <osmium/visitor.hpp>
#include <osmium/handler/node_locations_for_ways.hpp>
#include <fcntl.h>
#include <unistd.h>
#include <osmium/index/map/sparse_file_array.hpp>
#include <osmium/area/assembler.hpp>
#include <osmium/area/multipolygon_manager.hpp>

#include <s2/s2cell_id.h>
#include <s2/s2latlng.h>
#include <s2/s2region_coverer.h>
#include <s2/s2polyline.h>
#include <s2/s2polygon.h>
#include <s2/s2loop.h>
#include <s2/s2builder.h>

// Generated at build time by cmake/UpdateGitVersion.cmake; defines
// GEOCODER_GIT_SHA and GEOCODER_GIT_DIRTY string literals. The header
// is regenerated only when the SHA or dirty flag changes, so it does
// not invalidate ccache hits on no-op rebuilds.
#include "git_version.h"

// Vendored at builder/third_party/ankerl/unordered_dense.h (v4.8.1, MIT).
// Replaces std::unordered_map on the build-time hot path — chained-bucket
// libstdc++ unordered_map is "slow across the board" on insert-heavy
// workloads (Ankerl 2022, Allan 2024 benchmarks). See
// docs/performance/hashmap-choice-2026-04-25.md for the analysis.
#include <ankerl/unordered_dense.h>

// --- Binary format structs ---

struct WayHeader {
    uint32_t node_offset;
    uint8_t node_count;
    uint32_t name_id;
};

// AddrPoint flag bits stored in `AddrPoint.flags`. These let one
// 32-byte record represent both `addr:street` and `addr:place`
// addresses (and also `addr:full` / `addr:housename` overlays) without
// forking the on-disk format. Mirrors `FLAG_*` constants in
// `server/src/lib.rs`.
constexpr uint8_t FLAG_ADDR_PLACE   = 0x01; // street_or_place_id holds a place name
constexpr uint8_t FLAG_IS_HOUSENAME = 0x02; // housenumber_id holds a free-form housename / addr:full

struct AddrPoint {
    float lat;
    float lng;
    uint32_t housenumber_id;
    // Street name (default), or place name when (flags & FLAG_ADDR_PLACE).
    uint32_t street_or_place_id;
    // addr:unit / addr:flat / addr:door — 0 if absent.
    uint32_t unit_id;
    // addr:floor / addr:level — 0 if absent.
    uint32_t floor_id;
    // Tagged parent locality from addr:city|suburb|locality|state.
    // 0 if absent. Forward indexer prefers this over geometric
    // find_admin() enrichment when non-zero. Populated in commit 3.
    uint32_t parent_place_id;
    uint8_t flags;
    uint8_t _pad[3];
};

struct InterpWay {
    uint32_t node_offset;
    uint8_t node_count;
    uint32_t street_id;
    uint32_t start_number;
    uint32_t end_number;
    uint8_t interpolation;
};

// `importance` is the same Nominatim-style 0..255 prominence score that
// PlacePoint and PoiPoint carry — derived from `population` (log scale),
// `wikidata`, and `wikipedia` tags. Closes the gap where major cities
// represented as admin polygons (Arlington County VA, Münster NRW,
// Cornwall ON) were losing same-name disambiguation to tiny `place=town`
// siblings because admin docs entered the ranker with importance=0.
// Slot it into one byte of the existing 3-byte padding after
// `admin_level` — the struct stays 24 bytes (binary-format-stable).
struct AdminPolygon {
    uint32_t vertex_offset;     // 4
    uint16_t vertex_count;      // 2
    uint32_t name_id;           // 4
    uint8_t admin_level;        // 1
    uint8_t importance;         // 1 — 0..255, see compute_place_importance
    float area;                 // 4
    uint16_t country_code;      // 2
};

struct NodeCoord {
    float lat;
    float lng;
};

// place=* point feature (city/town/village/suburb/hamlet) — used as a
// fallback locality when admin boundaries don't cover an area.
//
// `importance` is a Nominatim-style prominence score derived from
// `population` (log scale), `wikidata`, and `wikipedia` tags at index
// time. The runtime uses it when proximity-biasing a search so that
// among same-name candidates within similar distance the more
// internationally-known place wins (Arlington VA outranks Arlington TX
// even when text BM25 favours TX, given a Washington-DC bias hint).
// 0..255, saturating; see `compute_place_importance` for the formula.
struct PlacePoint {
    float lat;
    float lng;
    uint32_t name_id;
    uint8_t rank;         // Nominatim-style address rank: 16=city/town, 19=suburb, 20=hamlet
    uint8_t importance;   // 0..255, derived from population + wiki tags
    uint8_t _pad[2];
};

// POI feature (amenity/shop/tourism/aeroway/historic/...). Indexed
// separately from place points and addresses because forward search
// needs a `category` filter and /reverse surfaces POIs as a sibling
// field to the address (never replaces it).
//
// rank: 10 if wikipedia/wikidata-backed, 15 otherwise. Lower wins ties
//   on /reverse distance. Autocomplete FST inclusion gates on rank<=10
//   to keep the FST small enough to mmap without paging (commit 5).
// importance: same Nominatim-style 0..255 prominence score that
//   PlacePoint carries — lets a search for "Sydney Opera House" pick
//   the wikipedia-backed UNESCO site over an arbitrary cafe of the
//   same name. Same formula as compute_place_importance (population
//   log + wikidata + wikipedia bonuses); for POIs population is rare
//   so the wiki signals dominate. Saturates at 255.
struct PoiPoint {
    float lat;
    float lng;
    uint32_t name_id;
    uint32_t category_id;       // interned "<key>:<value>" e.g. "amenity:cafe"
    uint8_t rank;
    uint8_t importance;         // 0..255, derived from population + wiki tags
    uint8_t _pad[2];
    uint32_t parent_place_id;   // tagged or geometric (filled in commit 5)
};

// Per-entity localized / alternate name. Sorted by
// (entity_type, entity_id, alias_type, lang_code) so the runtime does a
// single binary search per reverse query and a single contiguous walk
// per forward-index alternates pull.
//
// entity_type: 0 = admin polygon (entity_id = index into admin_polygons.bin)
//              1 = place point  (entity_id = index into place_points.bin)
//              (extended in later commits for streets / POIs)
// alias_type:  ALIAS_PRIMARY=0 (variant of the primary `name` tag, i.e.
//                              `name:<lang>`),
//              ALIAS_OFFICIAL=1 (`official_name` / `official_name:<lang>`),
//              ALIAS_ALT=2 (`alt_name` / `alt_name:<lang>`),
//              additional types (short_name/old_name/loc_name/int_name/
//              reg_name/ref/int_ref/nat_ref) added in commit 3.
// lang_code:   packed 2-char lowercase ASCII ("en" = 'e' | ('n'<<8)),
//              or 0 when the alias has no language tag (e.g. plain
//              `official_name=`).
struct I18nName {
    uint8_t entity_type;
    uint8_t alias_type;
    uint16_t lang_code;
    uint32_t entity_id;
    uint32_t name_id;
    uint32_t _pad1;
};

static const uint32_t INTERIOR_FLAG = 0x80000000u;
static const uint32_t ID_MASK = 0x7FFFFFFFu;

// --- On-disk layout pins ------------------------------------------------
// The runtime reader (server/src/lib.rs + server/tests/struct_layout.rs)
// mmaps these structs as `&[T]` and expects exact byte layouts. A silent
// drift in a C++ `sizeof` would serve garbled records. These asserts
// catch it at compile time on both sides of the bridge.
static_assert(sizeof(WayHeader)    == 12, "on-disk layout drift: WayHeader");
static_assert(sizeof(AddrPoint)    == 32, "on-disk layout drift: AddrPoint");
static_assert(sizeof(InterpWay)    == 24, "on-disk layout drift: InterpWay");
static_assert(sizeof(AdminPolygon) == 24, "on-disk layout drift: AdminPolygon");
static_assert(sizeof(NodeCoord)    == 8,  "on-disk layout drift: NodeCoord");
static_assert(sizeof(PlacePoint)   == 16, "on-disk layout drift: PlacePoint");
static_assert(sizeof(PoiPoint)     == 24, "on-disk layout drift: PoiPoint");
static_assert(sizeof(I18nName)     == 16, "on-disk layout drift: I18nName");

// --- uint32 offset guards ----------------------------------------------
// Every on-disk offset field is u32; planet-scale builds can legitimately
// push cumulative sizes toward 4 GB. Hitting that limit silently wraps
// the offset and serves corrupt data. These helpers throw with a clear
// message when we're about to wrap, so an operator sees "need to widen
// the on-disk format" instead of "server randomly returns junk".
template <typename T>
[[nodiscard]] static uint32_t checked_u32(T v, const char* what) {
    if (static_cast<uint64_t>(v) > static_cast<uint64_t>(UINT32_MAX)) {
        throw std::runtime_error(std::string("overflow: ") + what +
            " exceeds u32; widen the on-disk format or split the build");
    }
    return static_cast<uint32_t>(v);
}

// --- String interning ---

class StringPool {
public:
    uint32_t intern(const std::string& s) {
        auto it = index_.find(s);
        if (it != index_.end()) {
            return it->second;
        }
        // Guard against the string pool growing past 4 GB. On planet-scale
        // inputs with all name:<lang> tags kept this is within reach; the
        // offset field in every *_id is a u32, so we must refuse to mint
        // one that would truncate.
        uint32_t offset = checked_u32(data_.size(), "strings.bin offset");
        index_[s] = offset;
        data_.insert(data_.end(), s.begin(), s.end());
        data_.push_back('\0');
        return offset;
    }

    const std::vector<char>& data() const { return data_; }

private:
    // ankerl::unordered_dense::map<std::string, uint32_t> — flat, dense,
    // separate-payload layout. Hashes short strings via the library's
    // own avalanching hash, which is faster than libstdc++'s
    // std::hash<std::string> on the typical 1–30 char names we intern.
    ankerl::unordered_dense::map<std::string, uint32_t> index_;
    std::vector<char> data_;
};

// --- Collected data ---

static StringPool strings;

// All cell→[ids] maps below use ankerl::unordered_dense::segmented_map.
// Segmented (vs the regular `map`) means the underlying buckets array
// grows in 4096-element chunks instead of a single contiguous
// allocation that has to be reallocated+copied wholesale on every
// rehash. At planet scale (cell_to_addrs reaches ~100M cells), the
// segmented variant prevents a single multi-GB rehash from dominating
// the build's memory + time profile. The dense iterator order and
// SIMD-probed lookups are unchanged.
template <typename V>
using cell_map = ankerl::unordered_dense::segmented_map<uint64_t, V>;

// Streets
static std::vector<WayHeader> ways;
static std::vector<NodeCoord> street_nodes;
static cell_map<std::vector<uint32_t>> cell_to_ways;

// Addresses
static std::vector<AddrPoint> addr_points;
static cell_map<std::vector<uint32_t>> cell_to_addrs;

// Interpolation
static std::vector<InterpWay> interp_ways;
static std::vector<NodeCoord> interp_nodes;
static cell_map<std::vector<uint32_t>> cell_to_interps;

// Admin boundaries
static std::vector<AdminPolygon> admin_polygons;
static std::vector<NodeCoord> admin_vertices;
static cell_map<std::vector<uint32_t>> cell_to_admin;

// Place=* points (nodes and way centroids tagged as city/town/suburb/etc)
static std::vector<PlacePoint> place_points;
static cell_map<std::vector<uint32_t>> cell_to_places;
static uint64_t place_count_total = 0;

// POI points (amenity/shop/tourism/aeroway/historic/leisure/office/
// healthcare/military/man_made/railway-non-track/natural-subset/
// waterway-subset). Indexed at street_cell_level so /reverse can do
// the same 9-cell neighbour lookup as for streets and addresses.
static std::vector<PoiPoint> poi_points;
static cell_map<std::vector<uint32_t>> cell_to_pois;
static uint64_t poi_count_total = 0;

// associatedStreet relation members → interned street name id.
// Populated in the relation pre-pass before the main ingest. Looked
// up by `process_address_tags` as the street fallback when an entity
// has neither addr:street nor addr:place tags directly on it. We
// intentionally store only the relation's own `name` tag (per OSM
// convention for associatedStreet — see
// https://wiki.openstreetmap.org/wiki/Relation:associatedStreet) and
// don't chase way members for their `name` tag — that would need a
// second relation+way pass and the convention is well-followed enough
// that the simpler approach captures most of the value.
static ankerl::unordered_dense::map<int64_t, uint32_t> node_to_assoc_street;
static ankerl::unordered_dense::map<int64_t, uint32_t> way_to_assoc_street;

// Localized names from OSM `name:<lang>` tags. Populated inline as we
// process admin polygons and place points; written sorted.
static std::vector<I18nName> i18n_names;
static uint64_t i18n_count_total = 0;

// --- S2 helpers ---

static int kStreetCellLevel = 17;
static int kAdminCellLevel = 10;

static std::vector<S2CellId> cover_edge(double lat1, double lng1, double lat2, double lng2) {
    S2Point p1 = S2LatLng::FromDegrees(lat1, lng1).ToPoint();
    S2Point p2 = S2LatLng::FromDegrees(lat2, lng2).ToPoint();

    if (p1 == p2) {
        return {S2CellId(p1).parent(kStreetCellLevel)};
    }

    std::vector<S2Point> points = {p1, p2};
    S2Polyline polyline(points);

    S2RegionCoverer::Options options;
    options.set_fixed_level(kStreetCellLevel);

    S2RegionCoverer coverer(options);
    S2CellUnion covering = coverer.GetCovering(polyline);
    return covering.cell_ids();
}

static S2CellId point_to_cell(double lat, double lng) {
    return S2CellId(S2LatLng::FromDegrees(lat, lng)).parent(kStreetCellLevel);
}

// Returns pairs of (cell_id, is_interior)
static std::vector<std::pair<S2CellId, bool>> cover_polygon(const std::vector<std::pair<double,double>>& vertices) {
    std::vector<S2Point> points;
    points.reserve(vertices.size());
    for (const auto& [lat, lng] : vertices) {
        S2Point p = S2LatLng::FromDegrees(lat, lng).ToPoint();
        if (!points.empty() && points.back() == p) continue;
        points.push_back(p);
    }
    // Remove closing duplicate if first == last
    if (points.size() > 1 && points.front() == points.back()) {
        points.pop_back();
    }
    if (points.size() < 3) return {};

    // Build S2Loop (must be CCW), skip invalid polygons
    S2Error error;
    auto loop = std::make_unique<S2Loop>(points, S2Debug::DISABLE);
    loop->Normalize();
    if (loop->FindValidationError(&error)) return {};

    S2Polygon polygon(std::move(loop));

    S2RegionCoverer::Options options;
    options.set_max_level(kAdminCellLevel);
    options.set_max_cells(200);

    S2RegionCoverer coverer(options);
    S2CellUnion covering = coverer.GetCovering(polygon);
    S2CellUnion interior = coverer.GetInteriorCovering(polygon);

    // Build set of interior cell IDs for fast lookup
    std::unordered_set<uint64_t> interior_set;
    for (const auto& cell : interior.cell_ids()) {
        if (cell.level() <= kAdminCellLevel) {
            auto begin = cell.range_min().parent(kAdminCellLevel);
            auto end = cell.range_max().parent(kAdminCellLevel);
            for (auto c = begin; c != end; c = c.next()) {
                interior_set.insert(c.id());
            }
            interior_set.insert(end.id());
        } else {
            interior_set.insert(cell.parent(kAdminCellLevel).id());
        }
    }

    // Normalize all covering cells to kAdminCellLevel
    std::vector<std::pair<S2CellId, bool>> result;
    for (const auto& cell : covering.cell_ids()) {
        if (cell.level() <= kAdminCellLevel) {
            auto begin = cell.range_min().parent(kAdminCellLevel);
            auto end = cell.range_max().parent(kAdminCellLevel);
            for (auto c = begin; c != end; c = c.next()) {
                result.emplace_back(c, interior_set.count(c.id()) > 0);
            }
            result.emplace_back(end, interior_set.count(end.id()) > 0);
        } else {
            auto parent = cell.parent(kAdminCellLevel);
            result.emplace_back(parent, interior_set.count(parent.id()) > 0);
        }
    }

    // Deduplicate by cell_id, keeping interior=true if any duplicate is interior
    std::sort(result.begin(), result.end(), [](const auto& a, const auto& b) {
        return a.first < b.first;
    });
    auto it = result.begin();
    for (auto curr = result.begin(); curr != result.end(); ) {
        auto next = curr + 1;
        bool is_interior = curr->second;
        while (next != result.end() && next->first == curr->first) {
            is_interior = is_interior || next->second;
            ++next;
        }
        *it = {curr->first, is_interior};
        ++it;
        curr = next;
    }
    result.erase(it, result.end());
    return result;
}

// Approximate polygon area in square degrees. Uses the planar shoelace
// formula on lat/lng treated as a Cartesian plane. This is a rough
// approximation but the value is only used for ranking sibling
// polygons at the same admin_level, so absolute accuracy doesn't
// matter — only that larger polygons compare larger.
//
// Antimeridian-crossing polygons (Fiji, Russia-Far-East, the Aleutians)
// are the classic break case: planar shoelace over {..., 179, -179, ...}
// treats the crossing as a ~360° span and returns a huge bogus area.
// We detect the crossing by looking for any edge whose longitude delta
// exceeds 180° and, when we see one, normalise the ring by shifting
// the western half east by 360° before the shoelace. Pure-ranking use
// means the absolute value is still approximate but now monotonic.
static float polygon_area(const std::vector<std::pair<double,double>>& vertices) {
    size_t n = vertices.size();
    if (n < 3) return 0.0f;

    bool crosses_antimeridian = false;
    for (size_t i = 0; i < n; i++) {
        size_t j = (i + 1) % n;
        if (std::fabs(vertices[i].second - vertices[j].second) > 180.0) {
            crosses_antimeridian = true;
            break;
        }
    }

    auto lng_at = [&](size_t i) -> double {
        double lng = vertices[i].second;
        if (crosses_antimeridian && lng < 0.0) lng += 360.0;
        return lng;
    };

    double area = 0;
    for (size_t i = 0; i < n; i++) {
        size_t j = (i + 1) % n;
        area += vertices[i].first * lng_at(j);
        area -= vertices[j].first * lng_at(i);
    }
    return static_cast<float>(std::fabs(area) / 2.0);
}

// Douglas-Peucker simplification
static void dp_simplify(const std::vector<std::pair<double,double>>& pts,
                        size_t start, size_t end, double epsilon,
                        std::vector<bool>& keep) {
    if (end <= start + 1) return;

    double max_dist = 0;
    size_t max_idx = start;

    double ax = pts[start].first, ay = pts[start].second;
    double bx = pts[end].first, by = pts[end].second;
    double dx = bx - ax, dy = by - ay;
    double len_sq = dx * dx + dy * dy;

    for (size_t i = start + 1; i < end; i++) {
        double px = pts[i].first - ax, py = pts[i].second - ay;
        double dist;
        if (len_sq == 0) {
            dist = std::sqrt(px * px + py * py);
        } else {
            double t = std::max(0.0, std::min(1.0, (px * dx + py * dy) / len_sq));
            double proj_x = t * dx - px, proj_y = t * dy - py;
            dist = std::sqrt(proj_x * proj_x + proj_y * proj_y);
        }
        if (dist > max_dist) {
            max_dist = dist;
            max_idx = i;
        }
    }

    if (max_dist > epsilon) {
        keep[max_idx] = true;
        dp_simplify(pts, start, max_idx, epsilon, keep);
        dp_simplify(pts, max_idx, end, epsilon, keep);
    }
}

static std::vector<std::pair<double,double>> simplify_polygon(
    const std::vector<std::pair<double,double>>& pts, size_t max_vertices) {
    if (pts.size() <= max_vertices) return pts;

    // Binary search for epsilon that gives ~max_vertices
    double lo = 0, hi = 1.0;
    std::vector<std::pair<double,double>> result;

    for (int iter = 0; iter < 20; iter++) {
        double epsilon = (lo + hi) / 2;
        std::vector<bool> keep(pts.size(), false);
        keep[0] = true;
        keep[pts.size() - 1] = true;
        dp_simplify(pts, 0, pts.size() - 1, epsilon, keep);

        size_t count = 0;
        for (bool k : keep) if (k) count++;

        if (count > max_vertices) {
            lo = epsilon;
        } else {
            hi = epsilon;
        }
    }

    std::vector<bool> keep(pts.size(), false);
    keep[0] = true;
    keep[pts.size() - 1] = true;
    dp_simplify(pts, 0, pts.size() - 1, hi, keep);

    result.clear();
    for (size_t i = 0; i < pts.size(); i++) {
        if (keep[i]) result.push_back(pts[i]);
    }
    return result;
}

// --- Highway filter ---

// Highway values excluded from the street index. Narrowly scoped to
// way types that are almost always unnamed trails/service access or
// transient (`construction`). `highway=pedestrian` is intentionally
// kept IN the index: it tags named plazas and shopping streets that
// are legitimate geocoding targets (Martin Place Sydney, Bourke Street
// Mall Melbourne, Stephansplatz Vienna, most European old-town lanes).
// The surrounding `if (name)` gate in process_ways filters unnamed
// pedestrian ways out regardless, so including the type here only
// pulls in the subset with an explicit `name=` tag.
static const std::vector<std::string> kExcludedHighways = {
    "footway", "path", "track", "steps", "cycleway",
    "service", "bridleway", "construction"
};

static bool is_included_highway(const char* value) {
    for (const auto& excluded : kExcludedHighways) {
        if (excluded == value) return false;
    }
    return true;
}

// --- place=* rank mapping (compatible with Nominatim address_rank defaults) ---

// Returns 0 to skip this place tag (not useful for address output).
// --- Localized / alternate names ---

// Alias-type discriminator stored in I18nName.alias_type. The byte slot
// previously held a sentinel value packed into lang_code; splitting it
// off lets us cleanly represent alias-type × language as a 2-D key
// (e.g. `short_name:fr`, `old_name:en`). Mirrors ALIAS_* in
// server/src/i18n.rs.
constexpr uint8_t ALIAS_PRIMARY  = 0; // variant of the primary `name` tag (`name:<lang>`)
constexpr uint8_t ALIAS_OFFICIAL = 1; // `official_name` / `official_name:<lang>`
constexpr uint8_t ALIAS_ALT      = 2; // `alt_name` / `alt_name:<lang>` (also `name:left`/`right`)
constexpr uint8_t ALIAS_SHORT    = 3; // `short_name` / `short_name:<lang>` — e.g. "JFK"
constexpr uint8_t ALIAS_OLD      = 4; // `old_name` / `old_name:<lang>` — e.g. "Bombay"
constexpr uint8_t ALIAS_LOC      = 5; // `loc_name` / `loc_name:<lang>` — informal local name
constexpr uint8_t ALIAS_INT      = 6; // `int_name` / `int_name:<lang>` — international form
constexpr uint8_t ALIAS_REG      = 7; // `reg_name` / `reg_name:<lang>` — regional form
constexpr uint8_t ALIAS_REF      = 8; // `ref` — typically road / route reference (e.g. "A14")
constexpr uint8_t ALIAS_INT_REF  = 9; // `int_ref` — international ref (e.g. "E40")
constexpr uint8_t ALIAS_NAT_REF  = 10; // `nat_ref` — national ref

static void emit_alias(uint8_t entity_type, uint32_t entity_id,
                       uint8_t alias_type, uint16_t lang_code,
                       const char* value) {
    if (!value || !*value) return;
    I18nName rec{};
    rec.entity_type = entity_type;
    rec.alias_type = alias_type;
    rec.lang_code = lang_code;
    rec.entity_id = entity_id;
    rec.name_id = strings.intern(value);
    i18n_names.push_back(rec);
    i18n_count_total++;
}

// One alias-family table entry. Pairs an OSM tag prefix
// (`name`, `short_name`, ...) with the on-disk alias_type byte. The
// shared per-tag walk then handles both the bare-prefix (`short_name=`)
// and language-tagged (`short_name:fr=`) forms uniformly.
struct AliasFamily {
    const char* tag_prefix;
    size_t prefix_len;
    uint8_t alias_type;
};

// Order doesn't matter — we walk the whole table per tag and rely on the
// `*rest == '\0' || *rest == ':'` guard to skip "tag starts with prefix
// but isn't actually this family" cases (e.g. `name_typed`).
static const AliasFamily kAliasFamilies[] = {
    {"name",          4,  ALIAS_PRIMARY},
    {"official_name", 13, ALIAS_OFFICIAL},
    {"alt_name",      8,  ALIAS_ALT},
    {"short_name",    10, ALIAS_SHORT},
    {"old_name",      8,  ALIAS_OLD},
    {"loc_name",      8,  ALIAS_LOC},
    {"int_name",      8,  ALIAS_INT},
    {"reg_name",      8,  ALIAS_REG},
    {"ref",           3,  ALIAS_REF},
    {"int_ref",       7,  ALIAS_INT_REF},
    {"nat_ref",       7,  ALIAS_NAT_REF},
};

// Pack the first two letters of an OSM lang subtag into the runtime's
// 16-bit lang_code. Returns 0 for malformed / unsupported codes
// (3-letter ISO 639-3 like `nan`, mixed-case, etc.). For BCP47-shaped
// tags (`zh-Hant`, `en-AU`) we keep the 2-letter primary subtag and
// drop region/script — the runtime's pack_lang_code does the same on
// the query side, so `?lang=en-AU` and `name:en-AU` both resolve to the
// same packed code.
static uint16_t pack_lang_subtag(const char* suffix) {
    if (!suffix) return 0;
    size_t len = std::strlen(suffix);
    if (len < 2) return 0;
    char a = static_cast<char>(std::tolower(static_cast<unsigned char>(suffix[0])));
    char b = static_cast<char>(std::tolower(static_cast<unsigned char>(suffix[1])));
    if (a < 'a' || a > 'z' || b < 'a' || b > 'z') return 0;
    // Accept exactly 2 letters, or 2 letters + '-' + (region/script).
    // 3-letter primary subtags can't fit in our 16-bit code without
    // colliding with shorter codes (`nan` → `na` collides with
    // Norwegian `name:na`), so we drop them.
    if (len != 2 && !(len >= 5 && suffix[2] == '-')) return 0;
    return static_cast<uint16_t>(a) | (static_cast<uint16_t>(b) << 8);
}

// Emit a possibly multi-valued alias. OSM convention is to
// semicolon-separate alt_name (and similar) variants; split and intern
// each non-empty piece, trimming inner whitespace.
static void emit_alias_split(uint8_t entity_type, uint32_t entity_id,
                             uint8_t alias_type, uint16_t lang_code,
                             const char* value) {
    if (!value) return;
    std::string buf;
    for (const char* p = value;; ++p) {
        if (*p == ';' || *p == '\0') {
            while (!buf.empty() && std::isspace(static_cast<unsigned char>(buf.back()))) {
                buf.pop_back();
            }
            if (!buf.empty()) {
                emit_alias(entity_type, entity_id, alias_type, lang_code, buf.c_str());
            }
            if (*p == '\0') break;
            buf.clear();
        } else {
            if (buf.empty() && std::isspace(static_cast<unsigned char>(*p))) continue;
            buf.push_back(*p);
        }
    }
}

// Hand-curated English-exonym table. Maps an OSM `name` (in the local
// language / script) to the well-known English form. Triggered ONLY
// when the OSM entity doesn't already have a `name:en` tag — an
// existing curated name:en always wins.
//
// Inclusion rule: an exonym belongs here if (a) it's the dominant
// English form (encyclopaedia + travel + news), (b) it differs
// substantially from the local form (so English-only users won't find
// the place via the canonical name), and (c) the local string is
// distinctive enough that a value-only lookup match rules out false
// positives. Short common names ("Riga", "Lima", "Sofia") are skipped
// even when they have an exonym, because the local string overlaps
// with too many other places.
//
// The list intentionally stays small (~70 entries). It complements,
// not replaces, ICU transliteration: transliteration handles the bulk
// of Cyrillic/Arabic/CJK queries with no curation; exonyms cover the
// specific names where transliteration produces a string nobody types
// (`Moskva` vs `Moscow`, `al-Kharṭūm` vs `Khartoum`, `Beijing` vs the
// Pinyin-from-`Běijīng` `Beijing` … which happens to coincide).
static const std::unordered_map<std::string, const char*> kEnglishExonyms = {
    // German-language
    {"München", "Munich"},
    {"Köln", "Cologne"},
    {"Wien", "Vienna"},
    {"Nürnberg", "Nuremberg"},
    {"Hannover", "Hanover"},
    {"Braunschweig", "Brunswick"},
    {"Aachen", "Aachen"}, // same — but include so callers don't rely on alias absence

    // Italian
    {"Roma", "Rome"},
    {"Milano", "Milan"},
    {"Napoli", "Naples"},
    {"Firenze", "Florence"},
    {"Venezia", "Venice"},
    {"Torino", "Turin"},
    {"Genova", "Genoa"},
    {"Padova", "Padua"},
    {"Siracusa", "Syracuse"},
    {"Livorno", "Leghorn"},

    // Spanish / Portuguese / Catalan
    {"Sevilla", "Seville"},
    {"Lisboa", "Lisbon"},
    {"A Coruña", "Corunna"},
    {"Donostia / San Sebastián", "San Sebastian"},

    // Polish
    {"Warszawa", "Warsaw"},
    {"Kraków", "Krakow"},
    {"Wrocław", "Wroclaw"},
    {"Gdańsk", "Gdansk"},
    {"Łódź", "Lodz"},
    {"Poznań", "Poznan"},

    // Czech / Slovak / Hungarian
    {"Praha", "Prague"},
    {"Plzeň", "Pilsen"},

    // Romanian / Bulgarian / Serbian / Macedonian
    {"București", "Bucharest"},
    {"София", "Sofia"},
    {"Београд", "Belgrade"},
    {"Tiranë", "Tirana"},

    // Russian (Cyrillic — distinctive)
    {"Москва", "Moscow"},
    {"Санкт-Петербург", "Saint Petersburg"},
    {"Екатеринбург", "Yekaterinburg"},
    {"Нижний Новгород", "Nizhny Novgorod"},
    {"Новосибирск", "Novosibirsk"},
    {"Казань", "Kazan"},

    // Ukrainian
    {"Київ", "Kyiv"},
    {"Львів", "Lviv"},
    {"Одеса", "Odesa"},
    {"Харків", "Kharkiv"},
    {"Дніпро", "Dnipro"},

    // Greek
    {"Αθήνα", "Athens"},
    {"Θεσσαλονίκη", "Thessaloniki"},

    // Turkish (English drops the diacritics)
    {"İstanbul", "Istanbul"},
    {"İzmir", "Izmir"},

    // Arabic — highly divergent from English forms
    {"القاهرة", "Cairo"},
    {"الإسكندرية", "Alexandria"},
    {"الخرطوم", "Khartoum"},
    {"دمشق", "Damascus"},
    {"بغداد", "Baghdad"},
    {"الرياض", "Riyadh"},
    {"مكة المكرمة", "Mecca"},
    {"المدينة المنورة", "Medina"},
    {"الدوحة", "Doha"},
    {"بيروت", "Beirut"},
    {"عمّان", "Amman"},

    // Persian
    {"تهران", "Tehran"},
    {"اصفهان", "Isfahan"},
    {"شیراز", "Shiraz"},
    {"مشهد", "Mashhad"},
    {"تبریز", "Tabriz"},

    // Japanese
    {"東京", "Tokyo"},
    {"京都", "Kyoto"},
    {"大阪", "Osaka"},
    {"横浜", "Yokohama"},
    {"札幌", "Sapporo"},
    {"名古屋", "Nagoya"},

    // Chinese (Simplified — Beijing often present as 北京 in OSM)
    {"北京", "Beijing"},
    {"上海", "Shanghai"},
    {"广州", "Guangzhou"},
    {"深圳", "Shenzhen"},
    {"成都", "Chengdu"},
    {"重庆", "Chongqing"},
    {"杭州", "Hangzhou"},
    {"南京", "Nanjing"},
    {"西安", "Xi'an"},
    {"天津", "Tianjin"},

    // Korean
    {"서울", "Seoul"},
    {"부산", "Busan"},
    {"인천", "Incheon"},
};

// Emit a synthetic `name:en` alias for the entity when the OSM tags
// don't already include one and the entity's `name` is a curated
// exonym source (e.g. `Москва` → `Moscow`). Mirrors the shape of a
// real `name:en` tag in the i18n_names table, so the runtime treats
// it as an indistinguishable ALIAS_PRIMARY/lang=en entry.
//
// MUST be called AFTER `collect_i18n_names` so the OSM-curated
// `name:en` (when present) sits ahead of any synthetic fallback —
// the dedup-on-write sort key includes lang_code, so duplicates would
// collapse anyway, but emitting is wasted work when OSM already has it.
template <typename Tags>
static void maybe_emit_english_exonym(const Tags& tags,
                                      uint8_t entity_type,
                                      uint32_t entity_id) {
    if (tags["name:en"] != nullptr) return;
    const char* name = tags["name"];
    if (!name || !*name) return;
    auto it = kEnglishExonyms.find(std::string(name));
    if (it == kEnglishExonyms.end()) return;
    const uint16_t en = pack_lang_subtag("en");
    emit_alias(entity_type, entity_id, ALIAS_PRIMARY, en, it->second);
}

// Capture every alias-family tag (name:xx, official_name, alt_name,
// short_name, old_name, loc_name, int_name, reg_name, ref, int_ref,
// nat_ref — each plus per-language variants) on the given OSM entity
// and append them to the global i18n_names table. `entity_type` is 0
// for admin polygons, 1 for place points. `entity_id` must be the same
// index the runtime uses to look up the entity (i.e.
// admin_polygons.size() - 1 or place_points.size() - 1 depending on
// type, captured by the caller before it increments).
template <typename Tags>
static void collect_i18n_names(const Tags& tags, uint8_t entity_type, uint32_t entity_id) {
    for (const auto& tag : tags) {
        const char* k = tag.key();
        const char* v = tag.value();
        if (!k || !v || !*v) continue;

        for (const auto& fam : kAliasFamilies) {
            if (std::strncmp(k, fam.tag_prefix, fam.prefix_len) != 0) continue;
            const char* rest = k + fam.prefix_len;

            uint16_t lang_code = 0;
            if (*rest == '\0') {
                // Bare `<family>=...` — language-agnostic. The bare
                // `name=` lives on the entity itself
                // (PlacePoint.name_id / AdminPolygon.name_id), not in
                // i18n_names; we'd otherwise duplicate it, so skip.
                if (fam.alias_type == ALIAS_PRIMARY) break;
            } else if (*rest == ':') {
                const char* suffix = rest + 1;
                // OSM `name:left` / `name:right` are boundary
                // side-of-road names, not languages. Index them as
                // generic alternates so /search still finds them; only
                // honour the special case for the `name` family.
                if (fam.alias_type == ALIAS_PRIMARY &&
                    (std::strcmp(suffix, "left") == 0 || std::strcmp(suffix, "right") == 0)) {
                    emit_alias(entity_type, entity_id, ALIAS_ALT, /*lang=*/0, v);
                    break;
                }
                lang_code = pack_lang_subtag(suffix);
                if (lang_code == 0) break;
            } else {
                // Tag isn't actually this family (e.g. `name_typed`,
                // `referral`); keep walking the table.
                continue;
            }

            // alt_name and similar are conventionally semicolon-
            // separated; split and emit each variant.
            if (fam.alias_type == ALIAS_ALT) {
                emit_alias_split(entity_type, entity_id, fam.alias_type, lang_code, v);
            } else {
                emit_alias(entity_type, entity_id, fam.alias_type, lang_code, v);
            }
            break;
        }
    }
}

// Nominatim-aligned address rank. Lower = more important. The runtime's
// `find_place` prefers lower rank then closer distance, so the values
// chosen here decide which place tag wins when multiple cover the same
// query coordinate. `island` slots in between country and city; the new
// fine-grained tags (neighbourhood/quarter/locality/isolated_dwelling/
// farm) sit at or below suburb/hamlet so they never override a closer
// suburb result in dense urban areas.
static uint8_t place_rank(const char* place) {
    if (!place) return 0;
    if (std::strcmp(place, "city") == 0) return 16;
    if (std::strcmp(place, "town") == 0) return 16;
    if (std::strcmp(place, "village") == 0) return 16;
    if (std::strcmp(place, "island") == 0) return 17;
    if (std::strcmp(place, "suburb") == 0) return 19;
    if (std::strcmp(place, "hamlet") == 0) return 20;
    if (std::strcmp(place, "locality") == 0) return 21;
    if (std::strcmp(place, "islet") == 0) return 21;
    if (std::strcmp(place, "neighbourhood") == 0) return 22;
    if (std::strcmp(place, "quarter") == 0) return 22;
    if (std::strcmp(place, "isolated_dwelling") == 0) return 25;
    if (std::strcmp(place, "farm") == 0) return 25;
    return 0;
}

// Entity-type discriminator used in I18nName.entity_type. Mirrors the
// `ENTITY_*` constants in `server/src/i18n.rs`. PLACE is 1 (legacy
// value, kept for binary compat); POI is 2.
constexpr uint8_t ENTITY_ADMIN = 0;
constexpr uint8_t ENTITY_PLACE = 1;
constexpr uint8_t ENTITY_POI   = 2;

// --- POI extraction ---

// Tag whitelist for POI ingestion. Order is preference: when a feature
// carries multiple POI keys we use the first match from the table.
// `railway` is special-cased below to exclude track types (rail/tram/
// subway/light_rail/etc) which are linear features, not POIs.
static const std::vector<std::string> kPoiKeys = {
    "amenity", "shop", "tourism", "aeroway", "historic",
    "leisure", "office", "healthcare", "military", "man_made",
    "railway",
};

static bool is_excluded_railway(const char* v) {
    if (!v) return true;
    static const std::vector<std::string> kExcluded = {
        "rail", "tram", "subway", "light_rail", "narrow_gauge",
        "monorail", "preserved", "construction", "abandoned",
        "razed", "disused", "razed_rail", "yard",
    };
    for (const auto& e : kExcluded) {
        if (e == v) return true;
    }
    return false;
}

static const std::vector<std::string> kNaturalPoi = {
    "peak", "water", "bay", "cape", "volcano", "glacier",
    "cave_entrance", "spring",
};
static const std::vector<std::string> kWaterwayPoi = {
    "waterfall", "dock", "canal_lock",
};
static bool in_set(const char* v, const std::vector<std::string>& set) {
    if (!v) return false;
    for (const auto& s : set) if (s == v) return true;
    return false;
}

// Returns true when the entity is a POI worth indexing. On true, fills
// `category_out` (interned "<key>:<value>"), `rank_out`, and
// `parent_place_id_out`. Caller must already have a non-empty `name`
// because we drop unnamed POIs unconditionally.
template <typename Tags>
static bool extract_poi(const Tags& tags,
                        uint32_t& category_id_out,
                        uint8_t& rank_out,
                        uint32_t& parent_place_id_out) {
    const char* matched_key = nullptr;
    const char* matched_val = nullptr;

    for (const auto& k : kPoiKeys) {
        const char* v = tags[k.c_str()];
        if (!v || !*v) continue;
        if (k == "railway" && is_excluded_railway(v)) continue;
        matched_key = k.c_str();
        matched_val = v;
        break;
    }
    if (!matched_key) {
        const char* nat = tags["natural"];
        if (in_set(nat, kNaturalPoi)) {
            matched_key = "natural";
            matched_val = nat;
        }
    }
    if (!matched_key) {
        const char* ww = tags["waterway"];
        if (in_set(ww, kWaterwayPoi)) {
            matched_key = "waterway";
            matched_val = ww;
        }
    }
    if (!matched_key) return false;

    std::string category;
    category.reserve(std::strlen(matched_key) + 1 + std::strlen(matched_val));
    category.append(matched_key).push_back(':');
    category.append(matched_val);
    category_id_out = strings.intern(category);

    bool has_wiki = (tags["wikidata"] != nullptr) ||
                    (tags["wikipedia"] != nullptr);
    rank_out = has_wiki ? 10 : 15;

    const char* parent = tags["addr:city"];
    if (!parent) parent = tags["addr:suburb"];
    if (!parent) parent = tags["addr:locality"];
    if (!parent) parent = tags["is_in:city"];
    parent_place_id_out = (parent && *parent) ? strings.intern(parent) : 0;
    return true;
}

static uint32_t add_poi_point(double lat, double lng,
                              const char* name,
                              uint32_t category_id, uint8_t rank,
                              uint8_t importance,
                              uint32_t parent_place_id) {
    if (!name || !*name) return UINT32_MAX;
    uint32_t poi_id = checked_u32(poi_points.size(), "poi_points id");
    PoiPoint pt{};
    pt.lat = static_cast<float>(lat);
    pt.lng = static_cast<float>(lng);
    pt.name_id = strings.intern(name);
    pt.category_id = category_id;
    pt.rank = rank;
    pt.importance = importance;
    pt.parent_place_id = parent_place_id;
    poi_points.push_back(pt);

    S2CellId cell = point_to_cell(lat, lng);
    cell_to_pois[cell.id()].push_back(poi_id);
    poi_count_total++;
    if (poi_count_total % 100000 == 0) {
        std::cerr << "Collected " << poi_count_total / 1000 << "K POIs..." << std::endl;
    }
    return poi_id;
}

// Importance score from Nominatim-style prominence signals. Saturates
// at 255 so it fits in u8. Values:
//   - population: log10(pop)/8.0 * 100. log10(100M) = 8 ⇒ 100, 1M ⇒ 75,
//     100k ⇒ 62, 10k ⇒ 50, 1k ⇒ 37. Below ~10k contributes little.
//   - +30 if `wikidata` tag present.
//   - +50 if `wikipedia` tag present.
// Wikipedia is the strongest single signal; combined with a real
// population this commonly saturates for major world cities (75 + 30 +
// 50 = 155, room remains for super-prominent capitals like Tokyo
// where population alone hits 87).
//
// Used for both PlacePoint (places=city/town/village/...) and PoiPoint
// (amenity/shop/tourism/...) — the formula is identical because both
// use the same OSM signals, and the runtime ranks them on the same
// 0..255 scale.
template <typename Tags>
static uint8_t compute_place_importance(const Tags& tags) {
    uint32_t score = 0;

    const char* pop = tags["population"];
    if (pop && *pop) {
        char* end = nullptr;
        double n = std::strtod(pop, &end);
        if (end != pop && n > 0.0 && std::isfinite(n)) {
            double normalized = std::log10(n) / 8.0;
            if (normalized > 1.0) normalized = 1.0;
            if (normalized < 0.0) normalized = 0.0;
            score += static_cast<uint32_t>(normalized * 100.0);
        }
    }

    if (tags["wikidata"]  != nullptr) score += 30;
    if (tags["wikipedia"] != nullptr) score += 50;

    return static_cast<uint8_t>(std::min<uint32_t>(score, 255));
}

// Try to extract+emit a POI for the given OSM entity at (lat, lng).
// Returns true when a POI was emitted (also captures i18n alternates
// for the entity in that case).
template <typename Tags>
static bool try_emit_poi(double lat, double lng, const Tags& tags) {
    const char* name = tags["name"];
    if (!name || !*name) return false;
    uint32_t cat_id = 0, parent_id = 0;
    uint8_t rank = 0;
    if (!extract_poi(tags, cat_id, rank, parent_id)) return false;
    uint8_t importance = compute_place_importance(tags);
    uint32_t poi_id = add_poi_point(lat, lng, name, cat_id, rank, importance, parent_id);
    if (poi_id != UINT32_MAX) {
        collect_i18n_names(tags, ENTITY_POI, poi_id);
        return true;
    }
    return false;
}

// Returns the `place_id` the point was assigned to, or UINT32_MAX when
// the point was dropped. Callers can pass that id plus the feature's
// OSM tag list into `collect_i18n_names` to capture localized name:xx.
static uint32_t add_place_point(double lat, double lng, uint8_t rank,
                                uint8_t importance, const char* name) {
    if (!name || !*name) return UINT32_MAX;
    uint32_t place_id = checked_u32(place_points.size(), "place_points id");
    place_points.push_back({
        static_cast<float>(lat),
        static_cast<float>(lng),
        strings.intern(name),
        rank,
        importance,
        {0, 0},
    });

    // Index at kAdminCellLevel so nearest-neighbour queries use the same
    // cell neighbourhood as find_admin.
    S2CellId cell = S2CellId(S2LatLng::FromDegrees(lat, lng)).parent(kAdminCellLevel);
    cell_to_places[cell.id()].push_back(place_id);

    place_count_total++;
    if (place_count_total % 100000 == 0) {
        std::cerr << "Collected " << place_count_total / 1000 << "K place points..." << std::endl;
    }
    return place_id;
}

// --- Parse house number (leading digits) ---

// Used by interpolation endpoint extraction (start/end house numbers
// must be integers, by definition of `addr:interpolation`). For the
// AddrPoint housenumber field — which stores a free-form display
// string — see `normalise_housenumber` below.
static uint32_t parse_house_number(const char* s) {
    if (!s) return 0;
    uint32_t n = 0;
    while (*s >= '0' && *s <= '9') {
        n = n * 10 + (*s - '0');
        s++;
    }
    return n;
}

// --- Housenumber normalisation ---

// Recognised unit-prefix words. Lowercase; we case-fold the input.
// Covers EN (Apt/Apartment/Flat/Unit/Suite/Ste/Room/Rm/App), DE
// (Wohnung/WHG, Top), AT (Top), ES/PT (Apto/Lokal), FR (Appartement),
// IT (Interno/Int).
static const std::vector<std::string> kUnitPrefixWords = {
    "apt", "apt.", "apartment", "apartments",
    "flat", "unit", "suite", "ste", "ste.",
    "room", "rm", "rm.",
    "app", "apto", "appt", "appartement",
    "wohnung", "whg", "top", "lokal",
    "interno", "int", "int.",
};

// Returns true if `s`'s first whitespace-separated token (alpha chars
// + optional trailing '.') is a recognised unit prefix.
static bool starts_with_unit_prefix(const std::string& s) {
    std::string word;
    word.reserve(8);
    for (char c : s) {
        if (std::isalpha(static_cast<unsigned char>(c))) {
            word.push_back(static_cast<char>(std::tolower(static_cast<unsigned char>(c))));
        } else if (c == '.' && !word.empty()) {
            word.push_back('.');
            break;
        } else {
            break;
        }
    }
    if (word.empty()) return false;
    for (const auto& p : kUnitPrefixWords) {
        if (word == p) return true;
    }
    return false;
}

// Trim leading/trailing whitespace in place.
static void trim(std::string& s) {
    auto not_ws = [](unsigned char c) { return !std::isspace(c); };
    s.erase(s.begin(), std::find_if(s.begin(), s.end(), not_ws));
    s.erase(std::find_if(s.rbegin(), s.rend(), not_ws).base(), s.end());
}

// Normalise a free-form `addr:housenumber` string.
//
// Splits apartment-prefix forms `"Apt 4 / 12"` → housenumber="12",
// unit="Apt 4"; `"Flat 3, 22"` → housenumber="22", unit="Flat 3";
// `"Unit 5/12"` → housenumber="12", unit="Unit 5". Preserves `"12A"`,
// `"12-14"` (ranges), `"12/14"` (no unit prefix → treat slash as part
// of the housenumber rather than a unit boundary). Empty / unparseable
// inputs leave both outputs empty — caller's responsibility to detect
// and skip.
//
// Tantivy's existing ASCII-fold + lowercase tokenizer takes care of
// case-insensitive matching ("12A" matches "12a"), so we don't store a
// separate normalised form — the raw display string is what
// /reverse returns AND what tantivy indexes.
static void normalise_housenumber(const char* raw,
                                  std::string& housenumber_out,
                                  std::string& unit_out) {
    housenumber_out.clear();
    unit_out.clear();
    if (!raw) return;
    std::string s(raw);
    trim(s);
    if (s.empty()) return;

    // Look for a separator (`/`, `,`, `;`) that splits a unit-prefix
    // word from a digit-led housenumber. Walk left-to-right and keep
    // the FIRST split that satisfies both sides — apartment prefixes
    // are conventionally on the left.
    for (size_t i = 0; i < s.size(); i++) {
        char c = s[i];
        if (c != '/' && c != ',' && c != ';') continue;
        std::string lhs = s.substr(0, i);
        std::string rhs = s.substr(i + 1);
        trim(lhs);
        trim(rhs);
        if (lhs.empty() || rhs.empty()) continue;
        // RHS must start with a digit to be a housenumber.
        if (!std::isdigit(static_cast<unsigned char>(rhs[0]))) continue;
        // LHS must begin with a recognised unit-prefix word; if the
        // prefix word isn't there, the separator is most likely part
        // of a range / fraction housenumber (`12-14`, `12/14`) and we
        // shouldn't split.
        if (!starts_with_unit_prefix(lhs)) continue;
        unit_out = std::move(lhs);
        housenumber_out = std::move(rhs);
        return;
    }

    // No split — passthrough.
    housenumber_out = std::move(s);
}

// --- Add an address point ---

static uint64_t addr_count_total = 0;

// Low-level emitter. Callers are expected to have done all tag
// resolution / normalisation; this just interns and indexes.
static void add_addr_point_full(double lat, double lng,
                                const char* housenumber,
                                const char* street_or_place,
                                const char* unit,
                                const char* floor,
                                const char* parent,
                                uint8_t flags) {
    uint32_t addr_id = checked_u32(addr_points.size(), "addr_points id");
    AddrPoint pt{};
    pt.lat = static_cast<float>(lat);
    pt.lng = static_cast<float>(lng);
    pt.housenumber_id = strings.intern(housenumber);
    pt.street_or_place_id = strings.intern(street_or_place);
    pt.unit_id   = (unit   && *unit)   ? strings.intern(unit)   : 0;
    pt.floor_id  = (floor  && *floor)  ? strings.intern(floor)  : 0;
    pt.parent_place_id = (parent && *parent) ? strings.intern(parent) : 0;
    pt.flags = flags;
    addr_points.push_back(pt);

    S2CellId cell = point_to_cell(lat, lng);
    cell_to_addrs[cell.id()].push_back(addr_id);

    addr_count_total++;
    if (addr_count_total % 1000000 == 0) {
        std::cerr << "Collected " << addr_count_total / 1000000 << "M addresses..." << std::endl;
    }
}

// Pull every relevant `addr:*` tag from an OSM entity and emit at most
// one AddrPoint. Returns true when a point was emitted (caller can
// increment its building/address counter), false when the entity had
// no usable address signal.
//
// Recognised tags (with priority/fallback rules):
//
//   housenumber slot:
//     1. `addr:housenumber` — raw string, runs through
//        normalise_housenumber to peel any apartment prefix.
//     2. CZ/SK fallback: `addr:conscriptionnumber` + `addr:streetnumber`
//        combined with '/' (the local convention).
//     3. `addr:full` overlay — set FLAG_IS_HOUSENAME so forward search
//        skips numeric matching for this row.
//     4. `addr:housename` overlay — same flag.
//
//   street/place slot:
//     1. `addr:street` (default; FLAG_ADDR_PLACE clear).
//     2. `addr:place` (FLAG_ADDR_PLACE set; e.g. DE/AT/CH villages
//        whose convention is "12 / Kleindorf" rather than a street).
//     If neither is present we cannot emit (associatedStreet
//     propagation is wired in commit 5).
//
//   unit slot: `addr:unit` || `addr:flat` || `addr:door`. Otherwise
//     filled from any apartment prefix peeled out of the housenumber.
//
//   floor slot: `addr:floor` || `addr:level`.
//
//   parent_place slot: `addr:city` || `addr:suburb` ||
//     `addr:locality` || `addr:state`. Forward indexer prefers this
//     interned name over a geometric find_admin() lookup when set.
template <typename Tags>
static bool process_address_tags(double lat, double lng, const Tags& tags,
                                  const char* assoc_street_fallback = nullptr) {
    const char* hn_raw    = tags["addr:housenumber"];
    const char* full_tag  = tags["addr:full"];
    const char* housename = tags["addr:housename"];
    const char* street    = tags["addr:street"];
    const char* place     = tags["addr:place"];
    const char* unit      = tags["addr:unit"];
    if (!unit) unit       = tags["addr:flat"];
    if (!unit) unit       = tags["addr:door"];
    const char* floor_tag = tags["addr:floor"];
    if (!floor_tag) floor_tag = tags["addr:level"];
    const char* parent    = tags["addr:city"];
    if (!parent) parent   = tags["addr:suburb"];
    if (!parent) parent   = tags["addr:locality"];
    if (!parent) parent   = tags["addr:state"];

    // CZ/SK conscription/street number compose. The local convention
    // writes the conscription number first, then a slash, then the
    // street number — so a building shows `123/45` even though the
    // house faces 45 on the street. Reproduce the convention here so
    // /reverse renders something a Czech reader would recognise.
    std::string composed_hn;
    if (!hn_raw || !*hn_raw) {
        const char* consc = tags["addr:conscriptionnumber"];
        const char* sn    = tags["addr:streetnumber"];
        if (consc && *consc && sn && *sn) {
            composed_hn = std::string(consc) + "/" + sn;
            hn_raw = composed_hn.c_str();
        } else if (consc && *consc) {
            hn_raw = consc;
        } else if (sn && *sn) {
            hn_raw = sn;
        }
    }

    // Resolve the primary attached entity.
    uint8_t flags = 0;
    const char* primary = nullptr;
    if (street && *street) {
        primary = street;
    } else if (place && *place) {
        primary = place;
        flags |= FLAG_ADDR_PLACE;
    } else if (assoc_street_fallback && *assoc_street_fallback) {
        // associatedStreet relation supplies the street name when the
        // entity itself doesn't carry an addr:street tag (common in
        // DE/AT/CH; the relation's `name` is the canonical street
        // name per OSM convention).
        primary = assoc_street_fallback;
    } else {
        // Without a street or place attachment we have no meaningful
        // way to disambiguate the address from other addresses sharing
        // the same housenumber within the same admin polygon. Skip.
        return false;
    }

    // Resolve the housenumber slot. Numeric `addr:housenumber` wins,
    // then `addr:full`, then `addr:housename` — the latter two carry
    // FLAG_IS_HOUSENAME so the forward path skips numeric matching.
    std::string hn_norm, unit_from_hn;
    if (hn_raw && *hn_raw) {
        normalise_housenumber(hn_raw, hn_norm, unit_from_hn);
        if (hn_norm.empty()) return false;
    } else if (full_tag && *full_tag) {
        hn_norm = full_tag;
        flags |= FLAG_IS_HOUSENAME;
    } else if (housename && *housename) {
        hn_norm = housename;
        flags |= FLAG_IS_HOUSENAME;
    } else {
        return false;
    }

    // Tagged unit takes precedence over the prefix peeled out of the
    // housenumber — the OSM data model says addr:unit is authoritative
    // when present.
    const char* unit_final = (unit && *unit)
        ? unit
        : (!unit_from_hn.empty() ? unit_from_hn.c_str() : nullptr);

    add_addr_point_full(lat, lng,
                        hn_norm.c_str(), primary,
                        unit_final, floor_tag, parent,
                        flags);
    return true;
}

// --- Add an admin polygon ---

// Returns the `poly_id` written into admin_polygons.bin (i.e. the index
// the runtime will use), or UINT32_MAX when the polygon was skipped.
// Callers can pass the id plus the source OSM area's tag list into
// `collect_i18n_names` to capture localized name:xx.
// Approximate polygon area in km², projecting lat/lng into a local
// equal-distance grid centred on the polygon's centroid. Used only for
// the small-country vertex-cap heuristic in `add_admin_polygon` — it's
// not an exact spheroidal area, but the relative ordering across
// countries is what matters and the local projection is accurate to a
// few percent at country scale (the cosine-of-latitude simplification
// stops being useful at continental scale, which is precisely where
// the cap doesn't bite anyway).
static float polygon_area_km2(const std::vector<std::pair<double,double>>& vertices) {
    size_t n = vertices.size();
    if (n < 3) return 0.0f;

    double sum_lat = 0.0;
    for (const auto& [lat, lng] : vertices) sum_lat += lat;
    double centroid_lat = sum_lat / static_cast<double>(n);
    double cos_lat = std::cos(centroid_lat * M_PI / 180.0);
    constexpr double kKmPerDegLat = 111.32;

    bool crosses_antimeridian = false;
    for (size_t i = 0; i < n; i++) {
        size_t j = (i + 1) % n;
        if (std::fabs(vertices[i].second - vertices[j].second) > 180.0) {
            crosses_antimeridian = true;
            break;
        }
    }
    auto lng_at = [&](size_t i) -> double {
        double lng = vertices[i].second;
        if (crosses_antimeridian && lng < 0.0) lng += 360.0;
        return lng;
    };

    double area = 0.0;
    for (size_t i = 0; i < n; i++) {
        size_t j = (i + 1) % n;
        double xi = lng_at(i) * cos_lat * kKmPerDegLat;
        double yi = vertices[i].first * kKmPerDegLat;
        double xj = lng_at(j) * cos_lat * kKmPerDegLat;
        double yj = vertices[j].first * kKmPerDegLat;
        area += xi * yj - xj * yi;
    }
    return static_cast<float>(std::fabs(area) / 2.0);
}

static uint32_t add_admin_polygon(const std::vector<std::pair<double,double>>& vertices,
                                   const char* name, uint8_t admin_level,
                                   const char* country_code,
                                   uint8_t importance) {
    // Vertex cap scaled by admin_level, with a special bump for small
    // countries at level 2. Country borders need high fidelity because
    // reverse-geocode failures cluster within a few km of international
    // borders — at 8000 vertices a small country like Belgium drifts
    // hundreds of metres in places, putting query points on the wrong
    // side. Vertex density (vertices per km of border) is what actually
    // matters; small countries have less border to spend the budget on,
    // so a flat 8000 cap shortchanges them relative to large countries
    // that still get good per-km fidelity at 8000. Bumping ≤500K km²
    // countries to 16000 (and ≤100K to 32000) closes the BE/NL/LU/CH
    // border-bleed failures observed in bench-accuracy. The hard cap
    // of 32000 stays well under uint16_t's 65535 vertex_count limit.
    size_t max_vertices;
    if (admin_level <= 2) {
        float area_km2 = polygon_area_km2(vertices);
        if (area_km2 <= 100000.0f)      max_vertices = 32000;  // BE/NL/CH/LU
        else if (area_km2 <= 500000.0f) max_vertices = 16000;  // DE/IT/GB/PL/JP
        else                            max_vertices = 8000;   // FR/ES/RU/US/...
    }
    else if (admin_level <= 4) max_vertices = 3000;  // states/provinces
    else if (admin_level <= 6) max_vertices = 1500;  // counties/regions
    else                       max_vertices = 500;   // cities/districts/suburbs
    auto simplified = simplify_polygon(vertices, max_vertices);
    if (simplified.size() < 3) return UINT32_MAX;

    uint32_t poly_id = checked_u32(admin_polygons.size(), "admin_polygon id");
    uint32_t vertex_offset = checked_u32(admin_vertices.size(), "admin_vertices offset");

    for (const auto& [lat, lng] : simplified) {
        admin_vertices.push_back({static_cast<float>(lat), static_cast<float>(lng)});
    }

    AdminPolygon poly{};
    poly.vertex_offset = vertex_offset;
    poly.vertex_count = static_cast<uint16_t>(std::min(simplified.size(), size_t(65535)));
    poly.name_id = strings.intern(name);
    poly.admin_level = admin_level;
    poly.importance = importance;
    poly.area = polygon_area(simplified);
    poly.country_code = (country_code && country_code[0] && country_code[1])
        ? static_cast<uint16_t>((country_code[0] << 8) | country_code[1])
        : 0;
    admin_polygons.push_back(poly);

    // S2 cell coverage (high bit marks interior cells)
    auto cell_ids = cover_polygon(simplified);
    for (const auto& [cell_id, is_interior] : cell_ids) {
        uint32_t entry = is_interior ? (poly_id | INTERIOR_FLAG) : poly_id;
        cell_to_admin[cell_id.id()].push_back(entry);
    }
    return poly_id;
}

// --- OSM handler (pass 2) ---

class BuildHandler : public osmium::handler::Handler {
public:
    void node(const osmium::Node& node) {
        if (!node.location().valid()) return;
        const double lat = node.location().lat();
        const double lng = node.location().lon();

        // place=* locality features (skip if no name — unnameable places are useless)
        const char* place = node.tags()["place"];
        if (place) {
            uint8_t rank = place_rank(place);
            if (rank > 0) {
                const char* name = node.tags()["name"];
                uint8_t importance = compute_place_importance(node.tags());
                uint32_t place_id = add_place_point(lat, lng, rank, importance, name);
                if (place_id != UINT32_MAX) {
                    collect_i18n_names(node.tags(), ENTITY_PLACE, place_id);
                    maybe_emit_english_exonym(node.tags(), ENTITY_PLACE, place_id);
                }
            }
        }

        // POI emission. Independent of address/place — a single node
        // can legitimately be all three (e.g. amenity=cafe + name= +
        // addr:housenumber). try_emit_poi gates on `name` and the
        // amenity/shop/etc whitelist; returns silently for non-POIs.
        try_emit_poi(lat, lng, node.tags());

        // process_address_tags handles every accepted addr:* shape
        // (street, place, full, housename, plus unit/floor/parent
        // capture and CZ/SK conscription/street number compose) and
        // returns false when the entity has no usable address signal.
        // The associatedStreet fallback is consulted when the entity
        // itself carries no addr:street/addr:place — populated in
        // the pre-pass before pass 2 runs.
        //
        // Copy the interned string into a stack-owned std::string
        // before passing the pointer down: process_address_tags
        // reaches add_addr_point_full which calls strings.intern() on
        // unrelated strings, and any of those calls may grow the
        // strings.data() vector and invalidate a raw pointer into it.
        std::string assoc_buf;
        const char* assoc = nullptr;
        auto it = node_to_assoc_street.find(static_cast<int64_t>(node.id()));
        if (it != node_to_assoc_street.end()) {
            assoc_buf = strings.data().data() + it->second;
            assoc = assoc_buf.c_str();
        }
        process_address_tags(lat, lng, node.tags(), assoc);
    }

    void way(const osmium::Way& way) {
        // Address interpolation
        const char* interpolation = way.tags()["addr:interpolation"];
        if (interpolation) {
            process_interpolation_way(way, interpolation);
            return;
        }

        // Building addresses — accept any address-shaped tag set, not
        // just (housenumber + street). process_address_tags decides
        // what to emit (or to skip) and what to set on flags.
        if (way.tags()["addr:housenumber"] || way.tags()["addr:full"] ||
            way.tags()["addr:housename"] || way.tags()["addr:conscriptionnumber"] ||
            way.tags()["addr:streetnumber"]) {
            process_building_address(way);
        }

        // place=* on a closed way (suburb/town polygon) — use centroid as
        // the representative point. Non-closed ways are unusual for
        // place tags but we handle them the same way.
        // POIs on linear ways (e.g. an `aeroway=runway` with a name)
        // also emit here at the centroid; closed-way POIs go through
        // the area() handler instead.
        const char* place = way.tags()["place"];
        const char* way_name = way.tags()["name"];
        if ((place && place_rank(place) > 0 && way_name && *way_name) || way_name) {
            const auto& wnodes = way.nodes();
            double sum_lat = 0, sum_lng = 0;
            int valid = 0;
            for (const auto& nr : wnodes) {
                if (!nr.location().valid()) continue;
                sum_lat += nr.location().lat();
                sum_lng += nr.location().lon();
                valid++;
            }
            if (valid > 0) {
                double clat = sum_lat / valid;
                double clng = sum_lng / valid;
                if (place && way_name && *way_name) {
                    uint8_t rank = place_rank(place);
                    if (rank > 0) {
                        uint8_t importance = compute_place_importance(way.tags());
                        uint32_t place_id = add_place_point(clat, clng, rank, importance, way_name);
                        if (place_id != UINT32_MAX) {
                            collect_i18n_names(way.tags(), ENTITY_PLACE, place_id);
                            maybe_emit_english_exonym(way.tags(), ENTITY_PLACE, place_id);
                        }
                    }
                }
                // POI emission for ways. The MultipolygonManager only
                // routes relations of type=multipolygon/boundary
                // through area() — closed ways are NOT auto-converted.
                // So a closed-way `amenity=cafe` (the common shape
                // for buildings tagged as amenities) only ever fires
                // way(); skipping closed ways here would drop the
                // bulk of POIs. The rare double-emit case (a way
                // that's both tagged with a POI key AND is a member
                // of a multipolygon relation) is acceptable.
                if (way_name && *way_name) {
                    try_emit_poi(clat, clng, way.tags());
                }
            }
        }

        // Highway ways
        const char* highway = way.tags()["highway"];
        if (highway && is_included_highway(highway)) {
            const char* name = way.tags()["name"];
            if (name) {
                process_highway(way, name);
            }
        }
    }

    void area(const osmium::Area& area) {
        const char* boundary = area.tags()["boundary"];
        const char* place_tag = area.tags()["place"];

        bool is_admin = boundary && std::strcmp(boundary, "administrative") == 0;
        bool is_postal = boundary && std::strcmp(boundary, "postal_code") == 0;

        // Areas tagged `place=*` (typically on admin relations for
        // major cities — Aurora IL, Cornwall ON, Münster DE,
        // Saint-Eustache QC, Griffith NSW) need to land in
        // place_points.bin so /search can find them by name. They
        // also legitimately land in admin_polygons.bin when they
        // also carry boundary=administrative — both are correct,
        // they answer different questions (reverse vs forward).
        // Without this, large cities tagged on relations rather
        // than as separate place=city nodes are missing from
        // forward search entirely.
        bool has_useful_place = place_tag && place_rank(place_tag) > 0;

        // POI tag detection on the area itself. Areas tagged with
        // amenity=university, leisure=park, tourism=zoo, etc. are
        // legitimate POIs even when they carry no boundary tag — we
        // emit them at the polygon centroid so /search and /reverse
        // can find them. Computed lazily below to avoid the centroid
        // walk for the common case of plain admin polygons.
        bool needs_centroid = has_useful_place;
        // Pre-compute the centroid once if we'll need it for either
        // place or POI emission.
        double cent_lat = 0.0, cent_lng = 0.0;
        bool centroid_valid = false;
        auto compute_centroid = [&]() {
            if (centroid_valid) return;
            double sum_lat = 0.0, sum_lng = 0.0;
            int valid = 0;
            for (const auto& outer_ring : area.outer_rings()) {
                for (const auto& nr : outer_ring) {
                    if (nr.location().valid()) {
                        sum_lat += nr.location().lat();
                        sum_lng += nr.location().lon();
                        valid++;
                    }
                }
            }
            if (valid > 0) {
                cent_lat = sum_lat / valid;
                cent_lng = sum_lng / valid;
                centroid_valid = true;
            }
        };

        // Area-as-POI: any of the POI keys present (subject to the
        // same name-required + railway-track-excluded gates as for
        // node POIs).
        const char* area_name = area.tags()["name"];
        bool maybe_poi = area_name && *area_name;
        if (maybe_poi) {
            compute_centroid();
            if (centroid_valid) {
                try_emit_poi(cent_lat, cent_lng, area.tags());
            }
        }

        if (!is_admin && !is_postal && !has_useful_place) return;

        // Place-point emission for areas with a place=* tag. Done
        // FIRST so we always emit even if downstream admin-polygon
        // checks bail (e.g., admin_level out of range).
        if (has_useful_place) {
            const char* pname = area.tags()["name"];
            if (pname && *pname) {
                if (needs_centroid) compute_centroid();
                if (centroid_valid) {
                    uint8_t prank = place_rank(place_tag);
                    uint8_t pimp = compute_place_importance(area.tags());
                    uint32_t place_id = add_place_point(
                        cent_lat, cent_lng, prank, pimp, pname);
                    if (place_id != UINT32_MAX) {
                        collect_i18n_names(area.tags(), ENTITY_PLACE, place_id);
                        maybe_emit_english_exonym(area.tags(), ENTITY_PLACE, place_id);
                    }
                }
            }
        }

        // Admin / postal polygon emission only fires if the area
        // has the appropriate boundary tag.
        if (!is_admin && !is_postal) return;

        uint8_t admin_level = 0;
        if (is_admin) {
            const char* level_str = area.tags()["admin_level"];
            if (!level_str) return;
            admin_level = static_cast<uint8_t>(std::atoi(level_str));
            if (admin_level < 2 || admin_level > 10) return;
        } else {
            admin_level = 11; // use 11 for postal codes
        }

        const char* name = area.tags()["name"];
        if (!name && is_admin) return;

        // For postal codes, use postal_code tag as name
        std::string name_str;
        if (is_postal) {
            const char* postal_code = area.tags()["postal_code"];
            if (!postal_code) postal_code = name;
            if (!postal_code) return;
            name_str = postal_code;
        } else {
            name_str = name;
        }

        // Extract country code for level 2 boundaries
        const char* country_code = (admin_level == 2)
            ? area.tags()["ISO3166-1:alpha2"]
            : nullptr;
        // Extract outer ring vertices. Inner rings (holes) are counted
        // but not written to the index: the on-disk format has no
        // hole-exclusion flag, and PIP'ing a point that lies inside a
        // hole would currently return "inside the outer polygon" — a
        // wrong admin attribution.
        //
        // The practical impact is small in almost all cases:
        //  1. Enclave countries (Lesotho inside ZA, Vatican inside IT)
        //     have their own admin_level=2 polygon with a smaller area,
        //     and the reader's area-ranking already prefers the smaller
        //     polygon. So enclaves resolve correctly despite the hole.
        //  2. Pure geographic holes (a national-park polygon with a
        //     valley cut out) can still mis-attribute queries inside
        //     the hole. This is rare and we accept it as a known bug;
        //     fixing it requires a format bump (either store holes as
        //     a separate file or flag them with a new field).
        //
        // The inner-ring count is printed in the summary so an operator
        // can tell how much of their build is affected.
        //
        // Pre-pass to materialise outer-ring vertex sets + decide
        // whether to collapse them. Cities like Greensboro NC have a
        // single boundary relation with 41 outer rings (annexation
        // parcels + ETJ + city limits), each currently written as a
        // separate AdminPolygon with the same name + importance.
        // Forward search for "Greensboro" then sees 41 same-score
        // candidates scattered across the metro and the bias re-rank
        // can pick a fragment over the canonical city polygon. Collapse
        // to the largest ring when admin_level >= 4 AND there are more
        // than `kCollapseRingThreshold` rings — that captures
        // pathological proliferation while preserving legitimate
        // multi-island geometry (Indonesia at admin_level=2 is
        // excluded by the level gate; Hawaii Maui County at
        // admin_level=6 with 3 islands stays under the threshold).
        std::vector<std::vector<std::pair<double,double>>> outer_vertex_sets;
        for (const auto& outer_ring : area.outer_rings()) {
            std::vector<std::pair<double,double>> vertices;
            for (const auto& node_ref : outer_ring) {
                if (node_ref.location().valid()) {
                    vertices.emplace_back(node_ref.location().lat(), node_ref.location().lon());
                }
            }
            if (vertices.size() >= 3) {
                outer_vertex_sets.push_back(std::move(vertices));
            }
            for (const auto& inner_ring : area.inner_rings(outer_ring)) {
                (void)inner_ring;
                inner_ring_count_++;
            }
        }

        constexpr size_t kCollapseRingThreshold = 10;
        if (admin_level >= 4 && outer_vertex_sets.size() > kCollapseRingThreshold) {
            // Keep only the largest ring by polygon_area.
            size_t largest_idx = 0;
            float largest_area = polygon_area(outer_vertex_sets[0]);
            for (size_t i = 1; i < outer_vertex_sets.size(); ++i) {
                float a = polygon_area(outer_vertex_sets[i]);
                if (a > largest_area) {
                    largest_area = a;
                    largest_idx = i;
                }
            }
            std::vector<std::vector<std::pair<double,double>>> collapsed;
            collapsed.push_back(std::move(outer_vertex_sets[largest_idx]));
            outer_vertex_sets = std::move(collapsed);
            collapsed_admin_count_++;
        }

        for (const auto& vertices : outer_vertex_sets) {
            if (vertices.size() >= 3) {
                uint8_t importance = compute_place_importance(area.tags());
                uint32_t poly_id = add_admin_polygon(
                    vertices, name_str.c_str(), admin_level, country_code,
                    importance);
                if (poly_id != UINT32_MAX) {
                    collect_i18n_names(area.tags(), ENTITY_ADMIN, poly_id);
                    maybe_emit_english_exonym(area.tags(), ENTITY_ADMIN, poly_id);
                }
            }
        }

        admin_count_++;
        if (admin_count_ % 10000 == 0) {
            std::cerr << "Processed " << admin_count_ / 1000 << "K admin boundaries..." << std::endl;
        }
    }

    uint64_t way_count() const { return way_count_; }
    uint64_t building_addr_count() const { return building_addr_count_; }
    uint64_t interp_count() const { return interp_count_; }
    uint64_t admin_count() const { return admin_count_; }
    uint64_t inner_ring_count() const { return inner_ring_count_; }
    uint64_t collapsed_admin_count() const { return collapsed_admin_count_; }

private:
    uint64_t way_count_ = 0;
    uint64_t building_addr_count_ = 0;
    uint64_t interp_count_ = 0;
    uint64_t admin_count_ = 0;
    uint64_t inner_ring_count_ = 0;
    uint64_t collapsed_admin_count_ = 0;

    void process_building_address(const osmium::Way& way) {
        const auto& wnodes = way.nodes();
        if (wnodes.empty()) return;

        double sum_lat = 0, sum_lng = 0;
        int valid = 0;
        for (const auto& nr : wnodes) {
            if (!nr.location().valid()) continue;
            sum_lat += nr.location().lat();
            sum_lng += nr.location().lon();
            valid++;
        }
        if (valid == 0) return;

        // See node() for the rationale on copying into a std::string
        // before passing the pointer — strings.intern() inside
        // add_addr_point_full can invalidate a raw pointer into
        // strings.data().
        std::string assoc_buf;
        const char* assoc = nullptr;
        auto it = way_to_assoc_street.find(static_cast<int64_t>(way.id()));
        if (it != way_to_assoc_street.end()) {
            assoc_buf = strings.data().data() + it->second;
            assoc = assoc_buf.c_str();
        }
        if (process_address_tags(sum_lat / valid, sum_lng / valid, way.tags(), assoc)) {
            building_addr_count_++;
        }
    }

    void process_interpolation_way(const osmium::Way& way, const char* interpolation) {
        const auto& wnodes = way.nodes();
        if (wnodes.size() < 2) return;

        for (const auto& nr : wnodes) {
            if (!nr.location().valid()) return;
        }

        const char* street = way.tags()["addr:street"];
        if (!street) return;

        // Canonical addr:interpolation values we honour:
        //   "even" → step 2, even numbers only (reader uses type 1)
        //   "odd"  → step 2, odd numbers only  (reader uses type 2)
        //   "all"  → step 1, every number      (reader uses type 0, default)
        //   missing → step 1 (inferred, treated as "all")
        //
        // We reject alphabetic schemes (letter-range interpolation like
        // "A"–"F") and arbitrary numeric-step values (e.g. "3") because
        // the on-disk format has no way to carry them. Silently treating
        // them as "all" would emit wrong house-numbers along the edge
        // — better to drop the interpolation way than lie about its
        // range.
        uint8_t interp_type = 0;
        if (std::strcmp(interpolation, "even") == 0) interp_type = 1;
        else if (std::strcmp(interpolation, "odd") == 0) interp_type = 2;
        else if (std::strcmp(interpolation, "all") == 0) interp_type = 0;
        else {
            // Unknown scheme (alphabetic, numeric-step, typo). Skip.
            return;
        }

        uint32_t interp_id = checked_u32(interp_ways.size(), "interp_ways id");
        uint32_t node_offset = checked_u32(interp_nodes.size(), "interp_nodes offset");

        for (const auto& nr : wnodes) {
            interp_nodes.push_back({
                static_cast<float>(nr.location().lat()),
                static_cast<float>(nr.location().lon())
            });
        }

        InterpWay iw{};
        iw.node_offset = node_offset;
        iw.node_count = static_cast<uint8_t>(std::min(wnodes.size(), size_t(255)));
        iw.street_id = strings.intern(street);
        iw.start_number = 0;
        iw.end_number = 0;
        iw.interpolation = interp_type;
        interp_ways.push_back(iw);

        std::unordered_set<uint64_t> interp_cells;
        for (size_t i = 0; i + 1 < wnodes.size(); i++) {
            double lat1 = wnodes[i].location().lat();
            double lng1 = wnodes[i].location().lon();
            double lat2 = wnodes[i + 1].location().lat();
            double lng2 = wnodes[i + 1].location().lon();

            auto cell_ids = cover_edge(lat1, lng1, lat2, lng2);
            for (const auto& cell_id : cell_ids) {
                interp_cells.insert(cell_id.id());
            }
        }
        for (uint64_t cell_id : interp_cells) {
            cell_to_interps[cell_id].push_back(interp_id);
        }

        interp_count_++;
    }

    void process_highway(const osmium::Way& way, const char* name) {
        const auto& wnodes = way.nodes();
        if (wnodes.size() < 2) return;

        for (const auto& nr : wnodes) {
            if (!nr.location().valid()) return;
        }

        uint32_t way_id = checked_u32(ways.size(), "street_ways id");
        uint32_t node_offset = checked_u32(street_nodes.size(), "street_nodes offset");

        for (const auto& nr : wnodes) {
            street_nodes.push_back({
                static_cast<float>(nr.location().lat()),
                static_cast<float>(nr.location().lon())
            });
        }

        WayHeader header{};
        header.node_offset = node_offset;
        header.node_count = static_cast<uint8_t>(std::min(wnodes.size(), size_t(255)));
        header.name_id = strings.intern(name);
        ways.push_back(header);

        std::unordered_set<uint64_t> way_cells;
        for (size_t i = 0; i + 1 < wnodes.size(); i++) {
            double lat1 = wnodes[i].location().lat();
            double lng1 = wnodes[i].location().lon();
            double lat2 = wnodes[i + 1].location().lat();
            double lng2 = wnodes[i + 1].location().lon();

            auto cell_ids = cover_edge(lat1, lng1, lat2, lng2);
            for (const auto& cell_id : cell_ids) {
                way_cells.insert(cell_id.id());
            }
        }
        for (uint64_t cell_id : way_cells) {
            cell_to_ways[cell_id].push_back(way_id);
        }

        way_count_++;
        if (way_count_ % 1000000 == 0) {
            std::cerr << "Processed " << way_count_ / 1000000 << "M street ways..." << std::endl;
        }
    }
};

// --- Resolve interpolation way endpoint house numbers ---

static void resolve_interpolation_endpoints() {
    struct CoordKey {
        int32_t lat;
        int32_t lng;
        bool operator==(const CoordKey& o) const { return lat == o.lat && lng == o.lng; }
    };
    struct CoordHash {
        size_t operator()(const CoordKey& k) const {
            return std::hash<int64_t>()(((int64_t)k.lat << 32) | (uint32_t)k.lng);
        }
    };

    // Planet-scale: ~1B addr_points hashed by quantised coord. The
    // segmented variant prevents the single-rehash blowup that would
    // happen on a flat hashmap.
    ankerl::unordered_dense::segmented_map<CoordKey, uint32_t, CoordHash> addr_by_coord;
    for (uint32_t i = 0; i < addr_points.size(); i++) {
        CoordKey key{
            static_cast<int32_t>(addr_points[i].lat * 100000),
            static_cast<int32_t>(addr_points[i].lng * 100000)
        };
        addr_by_coord[key] = i;
    }

    uint32_t resolved = 0;
    for (auto& iw : interp_ways) {
        if (iw.node_count < 2) continue;

        const auto& start = interp_nodes[iw.node_offset];
        CoordKey start_key{
            static_cast<int32_t>(start.lat * 100000),
            static_cast<int32_t>(start.lng * 100000)
        };
        auto it_start = addr_by_coord.find(start_key);

        const auto& end = interp_nodes[iw.node_offset + iw.node_count - 1];
        CoordKey end_key{
            static_cast<int32_t>(end.lat * 100000),
            static_cast<int32_t>(end.lng * 100000)
        };
        auto it_end = addr_by_coord.find(end_key);

        if (it_start != addr_by_coord.end()) {
            const char* hn = strings.data().data() + addr_points[it_start->second].housenumber_id;
            iw.start_number = parse_house_number(hn);
        }
        if (it_end != addr_by_coord.end()) {
            const char* hn = strings.data().data() + addr_points[it_end->second].housenumber_id;
            iw.end_number = parse_house_number(hn);
        }

        if (iw.start_number > 0 && iw.end_number > 0) resolved++;
    }

    std::cerr << "Resolved " << resolved << "/" << interp_ways.size()
              << " interpolation ways" << std::endl;
}

// --- Deduplicate IDs per cell ---

template<typename Map>
static void deduplicate(Map& cell_map) {
    for (auto& [cell_id, ids] : cell_map) {
        std::sort(ids.begin(), ids.end());
        ids.erase(std::unique(ids.begin(), ids.end()), ids.end());
    }
}

// --- Write cell index ---

static const uint32_t NO_DATA = 0xFFFFFFFFu;

// Open a file for writing with exceptions enabled. An `ofstream` that
// silently fails on disk-full is not an uptime-compatible default; every
// caller must learn about the failure.
static std::ofstream open_out(const std::string& path) {
    std::ofstream f;
    f.exceptions(std::ios::failbit | std::ios::badbit);
    f.open(path, std::ios::binary | std::ios::trunc);
    return f;
}

// Atomic-write helper. Every output file is written to `<final>.tmp`
// and only renamed into place after the entire build succeeds, so a
// killed build or a mid-write exception leaves the live index files
// untouched. The runtime reader also rejects length-truncated files
// (see server/src/lib.rs::mmap_records), but the best defence is
// preventing the torn file from ever becoming the live path.
class IndexWriter {
public:
    explicit IndexWriter(std::string dir) : dir_(std::move(dir)) {}

    // Returns the .tmp path to write to. The <tmp, final> pair is
    // tracked so commit_all() can rename it at the end.
    std::string tmp_for(const std::string& name) {
        std::string final_path = dir_ + "/" + name;
        std::string tmp_path = final_path + ".tmp";
        pending_.emplace_back(tmp_path, final_path);
        return tmp_path;
    }

    // Rename every tmp → final. Called once at the end of write_index
    // after every byte has been flushed successfully. POSIX rename()
    // is atomic on same-filesystem paths, so partial commits can only
    // happen if the filesystem itself fails.
    void commit_all() {
        for (const auto& [tmp, final_path] : pending_) {
            std::filesystem::rename(tmp, final_path);
        }
        pending_.clear();
    }

    // Destructor rollback: remove any .tmp files that weren't committed.
    // Runs during exception unwinding, so never throws.
    ~IndexWriter() {
        std::error_code ec;
        for (const auto& [tmp, _] : pending_) {
            std::filesystem::remove(tmp, ec);
        }
    }

    IndexWriter(const IndexWriter&) = delete;
    IndexWriter& operator=(const IndexWriter&) = delete;

private:
    std::string dir_;
    std::vector<std::pair<std::string, std::string>> pending_;
};

// Convenience: open an AtomicFile stream scoped to a single write block.
// Caller passes `iw.tmp_for("foo.bin")`; the stream writes to foo.bin.tmp.
static std::ofstream open_tmp_out(IndexWriter& iw, const std::string& name) {
    return open_out(iw.tmp_for(name));
}

// Write entries file and return offset map. Uses u64 for `current` so
// crossing the 4 GB mark on planet-scale street_entries raises a clean
// overflow error instead of wrapping silently.
static ankerl::unordered_dense::map<uint64_t, uint32_t> write_entries(
    IndexWriter& iw,
    const std::string& name,
    const std::vector<uint64_t>& sorted_cells,
    const cell_map<std::vector<uint32_t>>& cells
) {
    ankerl::unordered_dense::map<uint64_t, uint32_t> offsets;
    std::ofstream f = open_tmp_out(iw, name);
    uint64_t current = 0;
    for (uint64_t cell_id : sorted_cells) {
        auto it = cells.find(cell_id);
        if (it == cells.end()) continue;
        offsets[cell_id] = checked_u32(current, (name + " cell offset").c_str());
        uint16_t count = static_cast<uint16_t>(std::min(it->second.size(), size_t(65535)));
        f.write(reinterpret_cast<const char*>(&count), sizeof(count));
        f.write(reinterpret_cast<const char*>(it->second.data()), it->second.size() * sizeof(uint32_t));
        current += sizeof(uint16_t) + it->second.size() * sizeof(uint32_t);
    }
    return offsets;
}

static void write_cell_index(
    IndexWriter& iw,
    const std::string& cells_name,
    const std::string& entries_name,
    const cell_map<std::vector<uint32_t>>& cells
) {
    std::vector<std::pair<uint64_t, std::vector<uint32_t>>> sorted(
        cells.begin(), cells.end());
    std::sort(sorted.begin(), sorted.end());

    {
        std::ofstream f = open_tmp_out(iw, cells_name);
        uint64_t current_offset = 0;
        for (const auto& [cell_id, ids] : sorted) {
            uint32_t offset_u32 = checked_u32(current_offset, (cells_name + " cell offset").c_str());
            f.write(reinterpret_cast<const char*>(&cell_id), sizeof(cell_id));
            f.write(reinterpret_cast<const char*>(&offset_u32), sizeof(offset_u32));
            current_offset += sizeof(uint16_t) + ids.size() * sizeof(uint32_t);
        }
    }

    {
        std::ofstream f = open_tmp_out(iw, entries_name);
        for (const auto& [cell_id, ids] : sorted) {
            (void)cell_id;
            uint16_t count = static_cast<uint16_t>(std::min(ids.size(), size_t(65535)));
            f.write(reinterpret_cast<const char*>(&count), sizeof(count));
            f.write(reinterpret_cast<const char*>(ids.data()), ids.size() * sizeof(uint32_t));
        }
    }
}

// --- Sort addr_points by S2 cell to make per-cell ID ranges
//     cache-contiguous on the read path ---
//
// `cell_to_addrs[c] = [ids]` is a per-cell list of indices into
// `addr_points.bin`. Currently those indices reflect ingestion order
// of the input PBF, which is essentially random with respect to
// geography. The runtime's hot path is `Index::query_geo`: walk 9
// cells, dereference each id into `all_points[id]`. Random ids = a
// pointer chase across the whole 64 MB+ addr_points file, every read
// missing L2 cache.
//
// Sorting addr_points by S2 cell makes the per-cell id ranges
// contiguous (and cache-line friendly): the 9 cells the runtime walks
// touch ~9 small ranges of adjacent records instead of 9 random
// scatters across the file. Estimated 10–30 % off /reverse on dense
// urban queries; bigger on cold caches.
//
// Memory cost: a remap vector and a duplicate addr_points array, both
// `addr_points.size()` long. Planet sees ~1 B addresses → ~32 GB peak
// during this step; the builder pipeline already sizes for ≥256 GB
// (see Packer config).
static void sort_addr_points_by_cell() {
    if (addr_points.empty()) return;

    std::vector<uint64_t> sorted_cells;
    sorted_cells.reserve(cell_to_addrs.size());
    for (const auto& [c, _] : cell_to_addrs) sorted_cells.push_back(c);
    std::sort(sorted_cells.begin(), sorted_cells.end());

    // Build the old_id → new_id remap by walking cells in sorted order.
    std::vector<uint32_t> remap(addr_points.size(), UINT32_MAX);
    uint32_t next_new_id = 0;
    for (uint64_t c : sorted_cells) {
        auto it = cell_to_addrs.find(c);
        if (it == cell_to_addrs.end()) continue;
        for (uint32_t old_id : it->second) {
            if (old_id >= remap.size()) continue;
            if (remap[old_id] == UINT32_MAX) {
                remap[old_id] = next_new_id++;
            }
        }
    }
    // Any addr_point not referenced by any cell (shouldn't happen given
    // the ingestion path, but guard against it) goes to the tail in
    // ingestion order so we don't drop records or corrupt indexing.
    for (uint32_t i = 0; i < remap.size(); i++) {
        if (remap[i] == UINT32_MAX) {
            remap[i] = next_new_id++;
        }
    }

    // Materialise the new addr_points layout.
    std::vector<AddrPoint> reordered(addr_points.size());
    for (uint32_t old_id = 0; old_id < addr_points.size(); old_id++) {
        reordered[remap[old_id]] = addr_points[old_id];
    }
    addr_points = std::move(reordered);

    // Rewrite cell_to_addrs to point at the new ids and re-sort within
    // each cell so the entries file's per-cell run is monotonic.
    for (auto& [_, ids] : cell_to_addrs) {
        for (auto& id : ids) id = remap[id];
        std::sort(ids.begin(), ids.end());
    }

    std::cerr << "Sorted " << addr_points.size()
              << " addr_points by S2 cell for cache locality" << std::endl;
}

// --- Write all index files ---

static void write_index(const std::string& output_dir) {
    // Everything is written to <name>.bin.tmp and renamed into place
    // only after every output succeeds. A mid-build crash / OOM / SIGKILL
    // therefore never leaves a half-written file as the live index.
    IndexWriter iw(output_dir);

    // Merged geo cell index for streets, addresses, and interpolation
    std::set<uint64_t> all_geo_cells;
    for (const auto& [id, _] : cell_to_ways) all_geo_cells.insert(id);
    for (const auto& [id, _] : cell_to_addrs) all_geo_cells.insert(id);
    for (const auto& [id, _] : cell_to_interps) all_geo_cells.insert(id);
    std::vector<uint64_t> sorted_geo_cells(all_geo_cells.begin(), all_geo_cells.end());

    auto street_offsets = write_entries(iw, "street_entries.bin", sorted_geo_cells, cell_to_ways);
    auto addr_offsets   = write_entries(iw, "addr_entries.bin",   sorted_geo_cells, cell_to_addrs);
    auto interp_offsets = write_entries(iw, "interp_entries.bin", sorted_geo_cells, cell_to_interps);

    {
        std::ofstream f = open_tmp_out(iw, "geo_cells.bin");
        for (uint64_t cell_id : sorted_geo_cells) {
            f.write(reinterpret_cast<const char*>(&cell_id), sizeof(cell_id));
            auto write_offset = [&](const ankerl::unordered_dense::map<uint64_t, uint32_t>& offsets) {
                auto it = offsets.find(cell_id);
                uint32_t offset = (it != offsets.end()) ? it->second : NO_DATA;
                f.write(reinterpret_cast<const char*>(&offset), sizeof(offset));
            };
            write_offset(street_offsets);
            write_offset(addr_offsets);
            write_offset(interp_offsets);
        }
    }

    std::cerr << "geo index: " << sorted_geo_cells.size() << " cells ("
              << ways.size() << " ways, "
              << addr_points.size() << " addrs, "
              << interp_ways.size() << " interps)" << std::endl;

    write_cell_index(iw, "admin_cells.bin", "admin_entries.bin", cell_to_admin);
    std::cerr << "admin index: " << cell_to_admin.size() << " cells, " << admin_polygons.size() << " polygons" << std::endl;

    write_cell_index(iw, "place_cells.bin", "place_entries.bin", cell_to_places);
    std::cerr << "place index: " << cell_to_places.size() << " cells, " << place_points.size() << " points" << std::endl;

    {
        std::ofstream f = open_tmp_out(iw, "place_points.bin");
        f.write(reinterpret_cast<const char*>(place_points.data()), place_points.size() * sizeof(PlacePoint));
    }

    // POI files. The Rust runtime treats `poi_points.bin` as optional
    // — older indexes built before commit 4 simply don't have it and
    // /reverse degrades to the address-only response shape.
    write_cell_index(iw, "poi_cells.bin", "poi_entries.bin", cell_to_pois);
    std::cerr << "poi index: " << cell_to_pois.size() << " cells, " << poi_points.size() << " points" << std::endl;
    {
        std::ofstream f = open_tmp_out(iw, "poi_points.bin");
        f.write(reinterpret_cast<const char*>(poi_points.data()),
                poi_points.size() * sizeof(PoiPoint));
    }

    // Localized / alternate names — sorted by
    // (entity_type, entity_id, alias_type, lang_code) so the runtime
    // does a single binary search per reverse query and a single
    // contiguous walk per forward-index alternates pull. Leave the
    // file as zero bytes when no aliases were collected — the runtime
    // treats missing/empty as "no i18n available".
    {
        std::sort(i18n_names.begin(), i18n_names.end(), [](const I18nName& a, const I18nName& b) {
            if (a.entity_type != b.entity_type) return a.entity_type < b.entity_type;
            if (a.entity_id != b.entity_id) return a.entity_id < b.entity_id;
            if (a.alias_type != b.alias_type) return a.alias_type < b.alias_type;
            if (a.lang_code != b.lang_code) return a.lang_code < b.lang_code;
            return a.name_id < b.name_id;
        });
        // Collapse only fully-duplicate records (same type, id,
        // alias_type, lang AND name_id). The name_id check is required
        // because alt_name is multi-valued (split on ';') so a single
        // entity legitimately emits N records all with
        // alias_type=ALIAS_ALT, lang_code=0 but distinct values;
        // dropping name_id from the key would silently keep only one
        // variant. alias_type is part of the key so e.g. a
        // `short_name:en` and a `name:en` for the same entity don't
        // collapse.
        i18n_names.erase(std::unique(i18n_names.begin(), i18n_names.end(),
            [](const I18nName& a, const I18nName& b) {
                return a.entity_type == b.entity_type
                    && a.entity_id == b.entity_id
                    && a.alias_type == b.alias_type
                    && a.lang_code == b.lang_code
                    && a.name_id == b.name_id;
            }), i18n_names.end());
        std::ofstream f = open_tmp_out(iw, "i18n_names.bin");
        f.write(reinterpret_cast<const char*>(i18n_names.data()),
                i18n_names.size() * sizeof(I18nName));
        std::cerr << "i18n names: " << i18n_names.size() << " (alias entries)" << std::endl;
    }

    {
        std::ofstream f = open_tmp_out(iw, "street_ways.bin");
        f.write(reinterpret_cast<const char*>(ways.data()), ways.size() * sizeof(WayHeader));
    }
    {
        std::ofstream f = open_tmp_out(iw, "street_nodes.bin");
        f.write(reinterpret_cast<const char*>(street_nodes.data()), street_nodes.size() * sizeof(NodeCoord));
    }
    {
        std::ofstream f = open_tmp_out(iw, "addr_points.bin");
        f.write(reinterpret_cast<const char*>(addr_points.data()), addr_points.size() * sizeof(AddrPoint));
    }
    {
        std::ofstream f = open_tmp_out(iw, "interp_ways.bin");
        f.write(reinterpret_cast<const char*>(interp_ways.data()), interp_ways.size() * sizeof(InterpWay));
    }
    {
        std::ofstream f = open_tmp_out(iw, "interp_nodes.bin");
        f.write(reinterpret_cast<const char*>(interp_nodes.data()), interp_nodes.size() * sizeof(NodeCoord));
    }
    {
        std::ofstream f = open_tmp_out(iw, "admin_polygons.bin");
        f.write(reinterpret_cast<const char*>(admin_polygons.data()), admin_polygons.size() * sizeof(AdminPolygon));
    }
    {
        std::ofstream f = open_tmp_out(iw, "admin_vertices.bin");
        f.write(reinterpret_cast<const char*>(admin_vertices.data()), admin_vertices.size() * sizeof(NodeCoord));
    }
    {
        std::ofstream f = open_tmp_out(iw, "strings.bin");
        f.write(strings.data().data(), strings.data().size());
        std::cerr << "strings.bin: " << strings.data().size() << " bytes" << std::endl;
    }

    // Every .tmp has been written successfully; atomically swap them
    // into the live names. Destructor would remove them if this throws.
    iw.commit_all();

    // manifest_reverse.json — written *after* commit_all so a partial
    // build never publishes a manifest claiming success. Mirrors the
    // Rust-side `manifest::write` helper. Operators read these files
    // before / after a rebuild to verify the new binary actually
    // changed the data instead of burning a multi-hour rebuild on a
    // binary that has the same code as the previous one. The macros
    // are defined in git_version.h, regenerated at every build.
    {
        const std::string manifest_path = output_dir + "/manifest_reverse.json";
        std::ofstream f(manifest_path);
        const auto unix_now = std::chrono::duration_cast<std::chrono::seconds>(
            std::chrono::system_clock::now().time_since_epoch()).count();
        const std::string dirty_raw = GEOCODER_GIT_DIRTY;
        const char* dirty_json = (dirty_raw == "true") ? "true" : "false";
        const char* dirty_known = (dirty_raw == "unknown") ? "false" : "true";
        f << "{\n"
          << "  \"tool\": \"reverse\",\n"
          << "  \"git_sha\": \"" << GEOCODER_GIT_SHA << "\",\n"
          << "  \"git_dirty\": " << dirty_json << ",\n"
          << "  \"git_dirty_known\": " << dirty_known << ",\n"
          << "  \"built_at_unix\": " << unix_now << ",\n"
          << "  \"counts\": {\n"
          << "    \"place_points\": " << place_points.size() << ",\n"
          << "    \"poi_points\": " << poi_points.size() << ",\n"
          << "    \"street_ways\": " << ways.size() << ",\n"
          << "    \"addr_points\": " << addr_points.size() << ",\n"
          << "    \"interp_ways\": " << interp_ways.size() << ",\n"
          << "    \"admin_polygons\": " << admin_polygons.size() << ",\n"
          << "    \"i18n_names\": " << i18n_names.size() << ",\n"
          << "    \"geo_cells\": " << sorted_geo_cells.size() << ",\n"
          << "    \"admin_cells\": " << cell_to_admin.size() << ",\n"
          << "    \"place_cells\": " << cell_to_places.size() << ",\n"
          << "    \"poi_cells\": " << cell_to_pois.size() << "\n"
          << "  }\n"
          << "}\n";
        if (!f) {
            std::cerr << "warning: failed to write " << manifest_path << std::endl;
        } else {
            std::cerr << "wrote " << manifest_path << std::endl;
        }
    }
}

// --- Main ---

static int run_build(int argc, char* argv[]);

int main(int argc, char* argv[]) {
    // Catch-all top-level handler. A stray osmium::pbf_error /
    // std::bad_alloc / IO exception otherwise becomes
    // "terminate called after throwing..." with no context and
    // exit status 134; operators need a grep-friendly line.
    try {
        return run_build(argc, argv);
    } catch (const std::exception& e) {
        std::cerr << "build failed: " << e.what() << std::endl;
        return 2;
    } catch (...) {
        std::cerr << "build failed: unknown exception" << std::endl;
        return 2;
    }
}

static int run_build(int argc, char* argv[]) {
    if (argc < 3) {
        std::cerr << "Usage: build-index <output-dir> <input.osm.pbf> [input2.osm.pbf ...] [--street-level N] [--admin-level N]" << std::endl;
        return 1;
    }

    std::string output_dir = argv[1];
    std::vector<std::string> input_files;
    for (int i = 2; i < argc; i++) {
        std::string arg = argv[i];
        if (arg == "--street-level" && i + 1 < argc) {
            kStreetCellLevel = std::atoi(argv[++i]);
        } else if (arg == "--admin-level" && i + 1 < argc) {
            kAdminCellLevel = std::atoi(argv[++i]);
        } else {
            input_files.push_back(arg);
        }
    }

    // RAII timing — prints "[stage] <name>: <Xs>" when the Stage
    // object goes out of scope. Used to surface per-phase wallclock
    // so the next perf round has data; format is grep-friendly so
    // CI can scrape the output.
    using clk = std::chrono::steady_clock;
    struct Stage {
        const char* name;
        clk::time_point start;
        explicit Stage(const char* n) : name(n), start(clk::now()) {}
        ~Stage() {
            const double s = std::chrono::duration<double>(clk::now() - start).count();
            std::cerr << "[stage] " << name << ": " << s << "s" << std::endl;
        }
    };
    const auto t_total = clk::now();

    BuildHandler handler;

    for (const auto& input_file : input_files) {
        std::cerr << "Processing " << input_file << "..." << std::endl;

        // --- Pass 1: collect relation members for multipolygon assembly,
        // and capture associatedStreet relation memberships in the same
        // sweep (cheaper than a separate read).
        std::cerr << "  Pass 1: scanning relations..." << std::endl;

        osmium::area::Assembler::config_type assembler_config;
        osmium::area::MultipolygonManager<osmium::area::Assembler> mp_manager{assembler_config};

        // Inline handler for associatedStreet relations. Records
        // (member node/way id) → interned street name id in the
        // shared `node_to_assoc_street` / `way_to_assoc_street` maps.
        struct AssocStreetCollector : public osmium::handler::Handler {
            uint64_t relations_seen = 0;
            void relation(const osmium::Relation& rel) {
                const char* type = rel.tags()["type"];
                if (!type || std::strcmp(type, "associatedStreet") != 0) return;
                const char* street_name = rel.tags()["name"];
                if (!street_name || !*street_name) return;
                uint32_t name_id = strings.intern(street_name);
                relations_seen++;
                for (const auto& member : rel.members()) {
                    const char* role = member.role();
                    if (!role || std::strcmp(role, "house") != 0) continue;
                    if (member.type() == osmium::item_type::node) {
                        node_to_assoc_street.emplace(static_cast<int64_t>(member.ref()), name_id);
                    } else if (member.type() == osmium::item_type::way) {
                        way_to_assoc_street.emplace(static_cast<int64_t>(member.ref()), name_id);
                    }
                }
            }
        } assoc_collector;

        {
            Stage _s{"pass1_relations"};
            osmium::io::Reader reader1{input_file, osmium::osm_entity_bits::relation};
            osmium::apply(reader1, assoc_collector, mp_manager);
            reader1.close();
            mp_manager.prepare_for_lookup();
            std::cerr << "  associatedStreet: " << assoc_collector.relations_seen
                      << " relations, " << node_to_assoc_street.size()
                      << " node members, " << way_to_assoc_street.size()
                      << " way members" << std::endl;
        }

        // --- Pass 2: process all data ---
        std::cerr << "  Pass 2: processing nodes, ways, and areas..." << std::endl;

        using index_type = osmium::index::map::SparseFileArray<
            osmium::unsigned_object_id_type, osmium::Location>;
        using location_handler_type = osmium::handler::NodeLocationsForWays<index_type>;

        std::string tmp_path = output_dir + "/node_locations.tmp";
        int fd = open(tmp_path.c_str(), O_RDWR | O_CREAT | O_TRUNC, 0600);
        if (fd == -1) {
            throw std::runtime_error("open " + tmp_path + ": " + std::strerror(errno));
        }
        // RAII cleanup: close fd + remove temp file even if osmium::apply
        // throws. Without this, a partial build leaks the tmp file and
        // (on some filesystems) the fd, blocking the next build run.
        struct NodeLocationsCleanup {
            int fd;
            std::string path;
            ~NodeLocationsCleanup() {
                if (fd >= 0) close(fd);
                std::error_code ec;
                std::filesystem::remove(path, ec);
            }
        } cleanup{fd, tmp_path};

        index_type index{fd};
        location_handler_type location_handler{index};

        osmium::io::Reader reader2{input_file};

        {
            Stage _s{"pass2_ingest"};
            osmium::apply(reader2, location_handler, handler, mp_manager.handler([&handler](osmium::memory::Buffer&& buffer) {
                osmium::apply(buffer, handler);
            }));
            reader2.close();
        }
    }

    std::cerr << "Done reading:" << std::endl;
    std::cerr << "  " << handler.way_count() << " street ways" << std::endl;
    std::cerr << "  " << addr_count_total << " address points ("
              << handler.building_addr_count() << " from buildings)" << std::endl;
    std::cerr << "  " << handler.interp_count() << " interpolation ways" << std::endl;
    std::cerr << "  " << handler.admin_count() << " admin/postcode boundaries ("
              << admin_polygons.size() << " polygon rings)" << std::endl;
    if (handler.inner_ring_count() > 0) {
        std::cerr << "  " << handler.inner_ring_count()
                  << " inner rings (holes) seen; not indexed — area ranking"
                  << " resolves enclaves, pure hole attribution is a known"
                  << " limitation" << std::endl;
    }
    if (handler.collapsed_admin_count() > 0) {
        std::cerr << "  " << handler.collapsed_admin_count()
                  << " admin areas collapsed to largest outer ring (>10"
                  << " rings, admin_level >= 4); fixes Greensboro-style"
                  << " name proliferation in forward search" << std::endl;
    }
    std::cerr << "  " << place_count_total << " place=* points" << std::endl;
    std::cerr << "  " << poi_count_total << " POIs" << std::endl;

    std::cerr << "Resolving interpolation endpoints..." << std::endl;
    {
        Stage _s{"resolve_interp"};
        resolve_interpolation_endpoints();
    }

    std::cerr << "Deduplicating..." << std::endl;
    {
        Stage _s{"dedup"};
        deduplicate(cell_to_ways);
        deduplicate(cell_to_addrs);
        deduplicate(cell_to_interps);
        deduplicate(cell_to_admin);
        deduplicate(cell_to_places);
    }

    {
        Stage _s{"sort_addr_points"};
        // Reorder addr_points so that per-cell ids are contiguous in the
        // file — cache-locality win on the reverse-geocode hot path.
        sort_addr_points_by_cell();
    }

    std::cerr << "Writing index files to " << output_dir << "..." << std::endl;
    {
        Stage _s{"write_index"};
        write_index(output_dir);
    }

    const double total = std::chrono::duration<double>(clk::now() - t_total).count();
    std::cerr << "[stage] total: " << total << "s" << std::endl;
    std::cerr << "Done." << std::endl;
    return 0;
}
