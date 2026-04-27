# Deferred follow-ups

Surfaced by the bench-accuracy run on the planet index (see
`docs/performance/load-test-planet-2026-04-27.md`) and the
forward-search i18n + abbreviation rebuild
(`docs/performance/forward-search-i18n-2026-04-27.md`). Each entry
has enough context to pick up cold; none are blocking real users
today, but each represents a known gap worth coming back to.

## Search & indexing

### Same-name disambiguation in `/search`

Queries like `Münster`, `Cambridge`, `Mount Pleasant` resolve to
exactly one of the many places sharing that name (the FST fast-path
returns a single representative entry). Real users get a valid
result but no signal that another match exists.

- **Workaround today:** structured queries with `country_code` or
  `state` filters scope the lookup to the right region.
- **Why it's deferred:** fixing it requires either a multi-value FST
  payload (schema change, larger keys) or a Tantivy fallback merge
  that runs alongside the FST hit and returns top-N (more wiring,
  more latency variance). Marginal real-user benefit relative to
  the implementation complexity.
- **Re-evaluate when:** a deployment shows users overwhelmingly
  picking ambiguous-name places via the wrong region — the
  workaround is on the caller, not the index.

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
