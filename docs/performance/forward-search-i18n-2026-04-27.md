# Forward search: i18n endonyms + Saint/Mount/Fort abbreviation fold

Date: 2026-04-27
Status: implemented; planet rebuild required to take effect at
worldwide scale.

## Why

The bench-accuracy run on the planet GB10 build
(`docs/performance/load-test-planet-2026-04-27.md`) surfaced two
classes of `/search` and `/autocomplete` failures that hit real
users.

### 1. Endonym/exonym mismatch (HIGHEST IMPACT)

English-speaking users querying non-English-named cities got **zero
results** because the forward-search builders only consulted the
canonical OSM `name` tag, ignoring the `name:xx` translations the
C++ indexer was already extracting into `i18n_names.bin`.

| Query | OSM canonical | Pre-fix | Post-fix |
|---|---|---|---|
| `Cologne` | `Köln` (DE) | no result | Köln |
| `The Hague` | `Den Haag` (NL) | no result | Den Haag |
| `Munich` | `München` (DE) | no result | München |
| `Vienna` | `Wien` (AT) | no result | Wien |
| `Florence` | `Firenze` (IT) | no result | Firenze |

`i18n_names.bin` already exists on disk (~4.3 M entries on planet,
sorted by `(entity_type, entity_id, lang_code)`) but only the
runtime `/reverse?lang=` path was using it. Both the autocomplete
FST builder and the Tantivy forward index builder ran blind to
alternate names.

### 2. Saint ↔ St abbreviation mismatch

`Saint Kilda` returned nothing on an index whose canonical entry is
`St Kilda`. Same pattern for `Mount Pleasant` / `Mt Pleasant` and
`Fort Worth` / `Ft Worth`. The previous tokenisers deliberately
left `St` literal (the comment in `forward.rs` flagged the Street/
Saint ambiguity), and there was no shared Saint↔St fold.

## What changed

Two coordinated fixes, shipped together so a single planet rebuild
on the GB10 picks up both.

### Fix 1 — i18n alternates fed into both forward-search builders

- **`server/src/i18n.rs`**: new `I18nNames::alternates_for(et, eid)`
  that range-walks the existing on-disk array in O(log N + k). No
  new on-disk index.
- **`server/src/bin/build_autocomplete_fst.rs`**: per place point,
  iterates `idx.i18n_names.alternates_for(ENTITY_PLACE, place_id)`
  and emits each alternate as an additional FST key pointing at the
  same `AutocompleteEntry`. One entry, N keys — keeps the entries
  vec lean while making every tagged language searchable.
- **`server/src/forward.rs`** (`tantivy_doc`): concatenates the
  alternate names into the indexed `name` field. Tantivy's
  `SimpleTokenizer + AsciiFoldingFilter + LowerCaser` chain
  tokenises the joined string into a flat token bag, so a BM25
  query for `the hague` matches via the `hague` token.

Streets are not expanded — the C++ builder only writes
`i18n_names.bin` entries for admin polygons (entity_type=0) and
place points (entity_type=1), not ways. The code path for streets
passes an empty alternate slice for shape parity.

### Fix 2 — `fold_place_abbreviations` shared helper

- **`server/src/autocomplete.rs`**: new `fold_place_abbreviations`
  helper. Folds `saint / sainte / st / ste → st`, `mount / mt → mt`,
  `fort / ft → ft`. Whole-word, position-aware: only the LEADING
  tokens of a multi-token phrase are folded; the trailing token is
  left literal so OSM's `Main St` (= Street) doesn't get
  mis-tokenised as Saint.
- Wired into all four sites that must agree byte-for-byte on the
  normalised key:
  - `autocomplete::normalise_prefix` (FST query)
  - `bin/build_autocomplete_fst::normalise_fst_key` (FST build)
  - `forward::canonicalise_phrase` (Tantivy build)
  - `forward::tokenize_user_input` (Tantivy query)

## Symmetry invariant

Drift between any pair of the four sites silently breaks the
Saint↔St mapping at scale. Pinned via
`server/tests/abbrev_symmetry.rs` — 9 tests covering positive cases
(Saint/Sainte/St/Ste collapse), negative cases (`Main St` stays
literal — St is the trailing token, meaning Street), and
build-vs-query equivalence on representative inputs. Any future
edit to one of the four sites that drifts will fail the test.

## Index size impact

User-prioritised: accuracy over data size. Budget signed off at
20–25% growth.

| Index | Pre-fix | Post-fix (expected) |
|---|---|---|
| Autocomplete FST (planet) | ~190 MB | ~240 MB (+25%) |
| Tantivy forward (planet) | ~2-3 GB | ~2.5-3.7 GB (+15-25%) |
| Build wallclock | 13.5 h | +small (extra range walks) |
| Query latency | ~1 ms p99 | unchanged |

Numbers will be re-measured against the next GB10 rebuild and
filed back into this doc.

## Expected bench-accuracy delta

| Scenario | Pre-fix | Expected post-fix |
|---|---|---|
| Reverse | 98.8 % | 98.8 % (unchanged — no path touched) |
| Autocomplete | ~99 % | ~99 % (most autocomplete inputs are prefixes of canonical names already) |
| Search | 81 % | ~94 % |
| **Overall** | ~93 % | **~96 %** |

Validated end-to-end against the AU smoke build before merge; the
planet number lands when the GB10 finishes its rebuild.

## Out of scope

Captured in `TODO.md` at repo root:

- Same-name disambiguation (`Münster`, `Cambridge`) — geocoder
  returns one valid answer; multi-result fix is invasive.
- Border-precision reverse failures — structural, admin polygon
  vertex density.
- Generalised transliteration (Cyrillic, CJK, Arabic without
  explicit `name:en`).
- Bench-fixture quality (Geonames neighborhoods that aren't OSM
  places).
- Embedding / vector-based search — deferred in favour of the
  deterministic OSM `name:xx` approach.

## For future maintainers

If you change ANY of the four normalisation sites listed above:

1. Run `cargo test -p query-server abbrev_symmetry` first. The
   property test will catch drift between build and query before
   it ships.
2. The position rule (Saint/Mount/Fort fold ONLY at leading
   tokens) is load-bearing. If you broaden it to trailing tokens
   you'll regress queries like `Main St` (where `St` means Street).
3. The `i18n_names.bin` schema is owned by the C++ indexer
   (`builder/src/build_index.cpp:99`). Any change there requires
   coordinating with the Rust read-path; the on-disk record
   layout is pinned at 16 bytes by `struct_layout` tests.
4. The Tantivy `name` field aggregates canonical + alternates by
   space-joining canonicalised forms. If you change the tokenizer
   chain in `register_tokenizer`, re-verify that joined alternate
   names still tokenise correctly.
