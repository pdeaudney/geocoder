#include <algorithm>
#include <cmath>
#include <cstdint>
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

// --- Binary format structs ---

struct WayHeader {
    uint32_t node_offset;
    uint8_t node_count;
    uint32_t name_id;
};

struct AddrPoint {
    float lat;
    float lng;
    uint32_t housenumber_id;
    uint32_t street_id;
};

struct InterpWay {
    uint32_t node_offset;
    uint8_t node_count;
    uint32_t street_id;
    uint32_t start_number;
    uint32_t end_number;
    uint8_t interpolation;
};

struct AdminPolygon {
    uint32_t vertex_offset;
    uint16_t vertex_count;
    uint32_t name_id;
    uint8_t admin_level;
    float area;
    uint16_t country_code;
};

struct NodeCoord {
    float lat;
    float lng;
};

// place=* point feature (city/town/village/suburb/hamlet) — used as a
// fallback locality when admin boundaries don't cover an area.
struct PlacePoint {
    float lat;
    float lng;
    uint32_t name_id;
    uint8_t rank;         // Nominatim-style address rank: 16=city/town, 19=suburb, 20=hamlet
    uint8_t _pad[3];
};

// Per-entity localized name. Sorted by (entity_type, entity_id, lang_code)
// so the runtime does a single binary search per reverse query.
//
// entity_type: 0 = admin polygon (entity_id = index into admin_polygons.bin)
//              1 = place point  (entity_id = index into place_points.bin)
// lang_code:   packed 2-char lowercase ASCII ("en" = 'e' | ('n'<<8)).
//              OSM uses keys like name:en, name:fr. We accept anything
//              matching `^name:[a-z]{2}$`; anything richer (name:zh-Hant,
//              name:en-AU) is skipped for MVP — covers 95% of tagged data.
struct I18nName {
    uint8_t entity_type;
    uint8_t _pad0;
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
static_assert(sizeof(AddrPoint)    == 16, "on-disk layout drift: AddrPoint");
static_assert(sizeof(InterpWay)    == 24, "on-disk layout drift: InterpWay");
static_assert(sizeof(AdminPolygon) == 24, "on-disk layout drift: AdminPolygon");
static_assert(sizeof(NodeCoord)    == 8,  "on-disk layout drift: NodeCoord");
static_assert(sizeof(PlacePoint)   == 16, "on-disk layout drift: PlacePoint");
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
    std::unordered_map<std::string, uint32_t> index_;
    std::vector<char> data_;
};

// --- Collected data ---

static StringPool strings;

// Streets
static std::vector<WayHeader> ways;
static std::vector<NodeCoord> street_nodes;
static std::unordered_map<uint64_t, std::vector<uint32_t>> cell_to_ways;

// Addresses
static std::vector<AddrPoint> addr_points;
static std::unordered_map<uint64_t, std::vector<uint32_t>> cell_to_addrs;

// Interpolation
static std::vector<InterpWay> interp_ways;
static std::vector<NodeCoord> interp_nodes;
static std::unordered_map<uint64_t, std::vector<uint32_t>> cell_to_interps;

// Admin boundaries
static std::vector<AdminPolygon> admin_polygons;
static std::vector<NodeCoord> admin_vertices;
static std::unordered_map<uint64_t, std::vector<uint32_t>> cell_to_admin;

// Place=* points (nodes and way centroids tagged as city/town/suburb/etc)
static std::vector<PlacePoint> place_points;
static std::unordered_map<uint64_t, std::vector<uint32_t>> cell_to_places;
static uint64_t place_count_total = 0;

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

// Approximate polygon area in square degrees
static float polygon_area(const std::vector<std::pair<double,double>>& vertices) {
    double area = 0;
    size_t n = vertices.size();
    for (size_t i = 0; i < n; i++) {
        size_t j = (i + 1) % n;
        area += vertices[i].first * vertices[j].second;
        area -= vertices[j].first * vertices[i].second;
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
// --- Localized names (`name:<lang>`) ---

// Capture `name:xx` tags (xx = 2 ASCII letters) on the given OSM entity
// and append them to the global i18n_names table. `entity_type` is 0 for
// admin polygons, 1 for place points. `entity_id` must be the same index
// the runtime uses to look up the entity (i.e. admin_polygons.size() -1
// or place_points.size() - 1 depending on type, captured by the caller
// before it increments).
template <typename Tags>
static void collect_i18n_names(const Tags& tags, uint8_t entity_type, uint32_t entity_id) {
    for (const auto& tag : tags) {
        const char* k = tag.key();
        if (!k || std::strncmp(k, "name:", 5) != 0) {
            continue;
        }
        const char* suffix = k + 5;
        // Accept only plain 2-letter lowercase lang codes. Anything richer
        // (zh-Hant, en-AU, name:left:en, etc.) is skipped for MVP.
        if (!(suffix[0] >= 'a' && suffix[0] <= 'z' &&
              suffix[1] >= 'a' && suffix[1] <= 'z' &&
              suffix[2] == '\0')) {
            continue;
        }
        const char* value = tag.value();
        if (!value || !*value) {
            continue;
        }
        // Pack the two-letter code into a u16 with 'a' in the low byte —
        // matches what the Rust runtime expects ("en" → 'e' | ('n'<<8)).
        uint16_t lang_code = static_cast<uint16_t>(suffix[0])
            | (static_cast<uint16_t>(suffix[1]) << 8);

        I18nName rec{};
        rec.entity_type = entity_type;
        rec.lang_code = lang_code;
        rec.entity_id = entity_id;
        rec.name_id = strings.intern(value);
        i18n_names.push_back(rec);
        i18n_count_total++;
    }
}

static uint8_t place_rank(const char* place) {
    if (!place) return 0;
    if (std::strcmp(place, "city") == 0) return 16;
    if (std::strcmp(place, "town") == 0) return 16;
    if (std::strcmp(place, "village") == 0) return 16;
    if (std::strcmp(place, "suburb") == 0) return 19;
    if (std::strcmp(place, "hamlet") == 0) return 20;
    return 0;
}

// Returns the `place_id` the point was assigned to, or UINT32_MAX when
// the point was dropped. Callers can pass that id plus the feature's
// OSM tag list into `collect_i18n_names` to capture localized name:xx.
static uint32_t add_place_point(double lat, double lng, uint8_t rank, const char* name) {
    if (!name || !*name) return UINT32_MAX;
    uint32_t place_id = checked_u32(place_points.size(), "place_points id");
    place_points.push_back({
        static_cast<float>(lat),
        static_cast<float>(lng),
        strings.intern(name),
        rank,
        {0, 0, 0},
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

static uint32_t parse_house_number(const char* s) {
    if (!s) return 0;
    uint32_t n = 0;
    while (*s >= '0' && *s <= '9') {
        n = n * 10 + (*s - '0');
        s++;
    }
    return n;
}

// --- Add an address point ---

static uint64_t addr_count_total = 0;

static void add_addr_point(double lat, double lng, const char* housenumber, const char* street) {
    uint32_t addr_id = checked_u32(addr_points.size(), "addr_points id");
    addr_points.push_back({
        static_cast<float>(lat),
        static_cast<float>(lng),
        strings.intern(housenumber),
        strings.intern(street)
    });

    S2CellId cell = point_to_cell(lat, lng);
    cell_to_addrs[cell.id()].push_back(addr_id);

    addr_count_total++;
    if (addr_count_total % 1000000 == 0) {
        std::cerr << "Collected " << addr_count_total / 1000000 << "M addresses..." << std::endl;
    }
}

// --- Add an admin polygon ---

// Returns the `poly_id` written into admin_polygons.bin (i.e. the index
// the runtime will use), or UINT32_MAX when the polygon was skipped.
// Callers can pass the id plus the source OSM area's tag list into
// `collect_i18n_names` to capture localized name:xx.
static uint32_t add_admin_polygon(const std::vector<std::pair<double,double>>& vertices,
                                   const char* name, uint8_t admin_level,
                                   const char* country_code) {
    // Simplify large polygons
    auto simplified = simplify_polygon(vertices, 500);
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

        // place=* locality features (skip if no name — unnameable places are useless)
        const char* place = node.tags()["place"];
        if (place) {
            uint8_t rank = place_rank(place);
            if (rank > 0) {
                const char* name = node.tags()["name"];
                uint32_t place_id = add_place_point(
                    node.location().lat(), node.location().lon(), rank, name);
                if (place_id != UINT32_MAX) {
                    collect_i18n_names(node.tags(), /*type=*/1, place_id);
                }
            }
        }

        const char* housenumber = node.tags()["addr:housenumber"];
        if (!housenumber) return;
        const char* street = node.tags()["addr:street"];
        if (!street) return;

        add_addr_point(node.location().lat(), node.location().lon(), housenumber, street);
    }

    void way(const osmium::Way& way) {
        // Address interpolation
        const char* interpolation = way.tags()["addr:interpolation"];
        if (interpolation) {
            process_interpolation_way(way, interpolation);
            return;
        }

        // Building addresses
        const char* housenumber = way.tags()["addr:housenumber"];
        if (housenumber) {
            const char* street = way.tags()["addr:street"];
            if (street) {
                process_building_address(way, housenumber, street);
            }
        }

        // place=* on a closed way (suburb/town polygon) — use centroid as
        // the representative point. Non-closed ways are unusual for
        // place tags but we handle them the same way.
        const char* place = way.tags()["place"];
        if (place) {
            uint8_t rank = place_rank(place);
            if (rank > 0) {
                const char* name = way.tags()["name"];
                if (name && *name) {
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
                        uint32_t place_id = add_place_point(
                            sum_lat / valid, sum_lng / valid, rank, name);
                        if (place_id != UINT32_MAX) {
                            collect_i18n_names(way.tags(), /*type=*/1, place_id);
                        }
                    }
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
        if (!boundary) return;

        bool is_admin = (std::strcmp(boundary, "administrative") == 0);
        bool is_postal = (std::strcmp(boundary, "postal_code") == 0);
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
        // Extract outer ring vertices
        for (const auto& outer_ring : area.outer_rings()) {
            std::vector<std::pair<double,double>> vertices;
            for (const auto& node_ref : outer_ring) {
                if (node_ref.location().valid()) {
                    vertices.emplace_back(node_ref.location().lat(), node_ref.location().lon());
                }
            }
            if (vertices.size() >= 3) {
                uint32_t poly_id = add_admin_polygon(
                    vertices, name_str.c_str(), admin_level, country_code);
                if (poly_id != UINT32_MAX) {
                    collect_i18n_names(area.tags(), /*type=*/0, poly_id);
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

private:
    uint64_t way_count_ = 0;
    uint64_t building_addr_count_ = 0;
    uint64_t interp_count_ = 0;
    uint64_t admin_count_ = 0;

    void process_building_address(const osmium::Way& way, const char* housenumber, const char* street) {
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

        add_addr_point(sum_lat / valid, sum_lng / valid, housenumber, street);
        building_addr_count_++;
    }

    void process_interpolation_way(const osmium::Way& way, const char* interpolation) {
        const auto& wnodes = way.nodes();
        if (wnodes.size() < 2) return;

        for (const auto& nr : wnodes) {
            if (!nr.location().valid()) return;
        }

        const char* street = way.tags()["addr:street"];
        if (!street) return;

        uint8_t interp_type = 0;
        if (std::strcmp(interpolation, "even") == 0) interp_type = 1;
        else if (std::strcmp(interpolation, "odd") == 0) interp_type = 2;

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

    std::unordered_map<CoordKey, uint32_t, CoordHash> addr_by_coord;
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
static std::unordered_map<uint64_t, uint32_t> write_entries(
    IndexWriter& iw,
    const std::string& name,
    const std::vector<uint64_t>& sorted_cells,
    const std::unordered_map<uint64_t, std::vector<uint32_t>>& cell_map
) {
    std::unordered_map<uint64_t, uint32_t> offsets;
    std::ofstream f = open_tmp_out(iw, name);
    uint64_t current = 0;
    for (uint64_t cell_id : sorted_cells) {
        auto it = cell_map.find(cell_id);
        if (it == cell_map.end()) continue;
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
    const std::unordered_map<uint64_t, std::vector<uint32_t>>& cell_map
) {
    std::vector<std::pair<uint64_t, std::vector<uint32_t>>> sorted(
        cell_map.begin(), cell_map.end());
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
            auto write_offset = [&](const std::unordered_map<uint64_t, uint32_t>& offsets) {
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

    // Localized names — sorted by (entity_type, entity_id, lang_code) so
    // the runtime does a single binary search per reverse query. Leave
    // the file as zero bytes when no name:xx tags exist — the runtime
    // treats missing/empty as "no i18n available".
    {
        std::sort(i18n_names.begin(), i18n_names.end(), [](const I18nName& a, const I18nName& b) {
            if (a.entity_type != b.entity_type) return a.entity_type < b.entity_type;
            if (a.entity_id != b.entity_id) return a.entity_id < b.entity_id;
            return a.lang_code < b.lang_code;
        });
        // Collapse duplicate (type, id, lang) triples — OSM occasionally
        // repeats tags (e.g. name:en on both an area and its relation);
        // keep the first which is stable under our order.
        i18n_names.erase(std::unique(i18n_names.begin(), i18n_names.end(),
            [](const I18nName& a, const I18nName& b) {
                return a.entity_type == b.entity_type
                    && a.entity_id == b.entity_id
                    && a.lang_code == b.lang_code;
            }), i18n_names.end());
        std::ofstream f = open_tmp_out(iw, "i18n_names.bin");
        f.write(reinterpret_cast<const char*>(i18n_names.data()),
                i18n_names.size() * sizeof(I18nName));
        std::cerr << "i18n names: " << i18n_names.size() << " (name:xx entries)" << std::endl;
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

    BuildHandler handler;

    for (const auto& input_file : input_files) {
        std::cerr << "Processing " << input_file << "..." << std::endl;

        // --- Pass 1: collect relation members for multipolygon assembly ---
        std::cerr << "  Pass 1: scanning relations..." << std::endl;

        osmium::area::Assembler::config_type assembler_config;
        osmium::area::MultipolygonManager<osmium::area::Assembler> mp_manager{assembler_config};

        {
            osmium::io::Reader reader1{input_file, osmium::osm_entity_bits::relation};
            osmium::apply(reader1, mp_manager);
            reader1.close();
            mp_manager.prepare_for_lookup();
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

        osmium::apply(reader2, location_handler, handler, mp_manager.handler([&handler](osmium::memory::Buffer&& buffer) {
            osmium::apply(buffer, handler);
        }));
        reader2.close();
    }

    std::cerr << "Done reading:" << std::endl;
    std::cerr << "  " << handler.way_count() << " street ways" << std::endl;
    std::cerr << "  " << addr_count_total << " address points ("
              << handler.building_addr_count() << " from buildings)" << std::endl;
    std::cerr << "  " << handler.interp_count() << " interpolation ways" << std::endl;
    std::cerr << "  " << handler.admin_count() << " admin/postcode boundaries ("
              << admin_polygons.size() << " polygon rings)" << std::endl;
    std::cerr << "  " << place_count_total << " place=* points" << std::endl;

    std::cerr << "Resolving interpolation endpoints..." << std::endl;
    resolve_interpolation_endpoints();

    std::cerr << "Deduplicating..." << std::endl;
    deduplicate(cell_to_ways);
    deduplicate(cell_to_addrs);
    deduplicate(cell_to_interps);
    deduplicate(cell_to_admin);
    deduplicate(cell_to_places);

    std::cerr << "Writing index files to " << output_dir << "..." << std::endl;
    write_index(output_dir);

    std::cerr << "Done." << std::endl;
    return 0;
}
