# Deferred follow-ups

Surfaced by the bench-accuracy run on the planet index (see
`docs/performance/load-test-planet-2026-04-27.md`) and the
forward-search i18n + abbreviation rebuild
(`docs/performance/forward-search-i18n-2026-04-27.md`). Each entry
has enough context to pick up cold; none are blocking real users
today, but each represents a known gap worth coming back to.

## Search & indexing

### Tune `BiasCoord::DISTANCE_ALPHA` against diverse same-name fixtures

α=0.10 was picked to make the AU St Kilda + cross-country Cambridge
test cases land correctly. The constant is the only tuning knob in
the bias path: `score = bm25 - α * ln(distance_km + 1)`.

- **What's missing:** a fixture of 20–50 same-name place pairs at
  varying distances (20 km, 200 km, 2000 km, 20000 km), and an
  automated sweep that proves α=0.10 ranks them correctly across
  the distribution. Right now we have a handful of ad-hoc cases.
- **Risk if mistuned:** too low → bias never wins (the original
  Melbourne/SA failure mode); too high → bias overrides genuinely
  better text matches. The α=0.10 + skip-prominence-on-bias combo
  is empirically OK but isn't proven.
- **Re-evaluate when:** post-planet rebuild bench-accuracy data
  shows real-world bias-vs-no-bias quality.

### BM25 boost on the `rank` field for unbiased queries

Without bias, ranking quality for ambiguous names depends entirely
on the multiplicative prominence boost in `boosted_score`. Tantivy
already stores `rank` as a FAST/STORED field but doesn't include
it in BM25 scoring. We could add a `BoostQuery` per term that
weights matches by inverse rank, smoothing the binary "FST returns
1, Tantivy returns N" cliff.

- **Owner cost:** ~1 day. Tantivy's `BoostQuery` is already used
  for the name-vs-suburb boost in `search_once`.
- **Re-evaluate when:** post-planet rebuild data shows specific
  rank inversions (e.g. a `Cambridge Lane` outranking `Cambridge`
  city in BM25 alone).

### Optional `bias_radius_km` hard filter

The current bias is a soft re-rank — `Tokyo` from a London bias
still finds Tokyo. Some callers want the opposite: "only return
places within 50 km of this coord" (find-my-nearest-X use cases).

- **Why it's deferred:** the `/search` shape is freeform-text-
  first; a radius filter belongs on a separate "nearby search"
  endpoint with a different semantic contract. Adding it as a
  parameter to `/search` would muddle the contract.
- **Re-evaluate when:** a customer specifically asks for
  nearest-N-X-within-Y-km. A new endpoint (`/nearby?lat=&lng=&kind=`)
  is a cleaner home for it than overloading `/search`.

### Border-precision reverse failures (~1 % at planet scale)

Reverse queries within ~5 km of national borders can pick the
neighbour country. Caused by admin polygon vertex density: OSM
borders are simplified at varying detail by region.

- **Why it's deferred:** structural. Densifying every country
  boundary is large, ongoing data work.
- **Re-evaluate when:** a customer specifically needs sub-km
  border-side accuracy in a region where OSM is sparse.

### Generalised transliteration (Cyrillic, CJK, Arabic)

`Belgrade`/`Београд`, `Tokyo`/`東京`, `Cairo`/`القاهرة`. The current
fix handles ASCII-Latin endonyms via OSM `name:xx` tags; non-Latin
scripts only cross-resolve when an explicit `name:en`/`name:de`
tag exists.

- **Workaround today:** OSM `name:en` coverage is good for major
  cities; minor places drop out.
- **Why it's deferred:** ICU transliteration is heavyweight to
  embed and the cost-benefit is a rounding error at planet scale
  (most non-Latin places that real users query DO have a `name:en`
  tag).
- **Re-evaluate when:** a deployment specifically targets a
  region (CIS, EA) where the OSM `name:en` gap is large.

## Future evaluation

### Embedding / vector-based search for geocoding

Considered as a parallel approach to the deterministic `name:xx`
fix shipped here.

- **Use cases where it'd help:** transliteration without explicit
  tags (Cyrillic, CJK, Arabic); typo tolerance beyond Levenshtein-1
  fuzzy; natural-language POI search ("coffee near Times Square").
- **Why deferred over the deterministic fix:** OSM `name:xx` is
  curator-verified, ~10× smaller than even minimal vectors at
  planet scale (4-15 GB just for index-side embeddings vs ~210 MB
  for the FST), and deterministic for regression testing. Vector
  search would be an approximation of the same mapping with worse
  precision and harder debugging.
- **Re-evaluate when:** any of the three use-cases above becomes a
  product requirement. None are today.

## Done references

Links to the snapshot docs that explain the fixes already shipped
— useful when triaging a regression that overlaps:

- `docs/performance/forward-search-i18n-2026-04-27.md` — i18n
  expansion + Saint/Mount/Fort fold (this PR).
- `docs/performance/load-test-planet-2026-04-27.md` — planet
  bench-accuracy baseline that surfaced the items above.
