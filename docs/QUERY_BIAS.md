# Query bias: proximity hints on `/search`

A reference for engineers working on or against the geocoder's `/search` endpoint, explaining what `bias_lat` / `bias_lng` does, when it fires, and what it doesn't do.

For per-PR change history see `docs/performance/forward-search-translit-2026-04-29.md` and the bias-related parts of `docs/performance/forward-search-i18n-2026-04-27.md`. This doc covers the model, not the patches.

---

## What it is

`bias_lat` / `bias_lng` are optional WGS84 coordinates passed alongside a `/search` query to nudge ranking toward results geographically close to that point. The same parameters exist on the gRPC `Search` RPC (proto3 `optional double` to distinguish "not set" from `(0, 0)` — Null Island is a valid coord).

```
GET /search?q=Cambridge&bias_lat=42.36&bias_lng=-71.06    → Cambridge MA
GET /search?q=Cambridge&bias_lat=52.20&bias_lng=0.12      → Cambridge UK
GET /search?q=Cambridge                                    → one of them, by BM25 alone
```

Both fields are required together. Range-validated: `lat ∈ [-90, 90]`, `lng ∈ [-180, 180]`. Out-of-range or partial-set returns 400 / `InvalidArgument`.

## What it solves

**Same-name disambiguation.** Without bias, queries like `Cambridge`, `Aurora`, `Springfield`, `Münster`, `St Kilda` resolve to one of several places sharing the name based on BM25 alone — which doesn't know the user's geographic intent. Real bench-accuracy failure modes pre-bias:

| Query | `country_code` | Hint coord | What we returned | Distance from hint |
|---|---|---|---|---|
| `Carrollton` | us | (33.58, -85.07) — GA | Carrollton TX | 915 km |
| `Aurora` | us | (39.73, -104.83) — CO | Aurora IL | 2020 km |
| `Cornwall` | ca | (45.02, -74.73) — ON | Cornwall UK | 904 km |
| `Münster` | de | (51.96, 7.63) — NW | Münster (other) | 202 km |

With bias on, the hint coord pushes the locally-correct match to the top.

## What it does NOT solve

1. **Hard filtering.** Bias is a soft tiebreak. `Tokyo` from a London bias still returns Tokyo AU because BM25 dominates when one match is clearly better. If you need "only show me places within X km of this coord", that's a `/nearby` use case (currently a TODO follow-up — adding it as `bias_radius_km` on `/search` would muddle the contract).
2. **Cross-language search.** Bias doesn't help a Russian user finding a Russian place — that's the OSM `name:xx` expansion (already in `forward::tantivy_doc` and `Candidate.aliases` per PR #11), plus generalised ICU script-to-Latin transliteration when that PR lands. Bias and translit compose: a Boston user typing `Cambridge` finds Cambridge MA via bias; an English user typing `Chelyabinsk` finds Челябинск via translit; both can apply to the same query.
3. **Typo tolerance.** A typo like `Caimbridge` doesn't get fixed by bias. Tantivy's fuzzy fallback (Levenshtein distance 1) handles single-char typos; bias plays no role.
4. **Recall improvement when there are zero matches.** If a query returns zero hits without bias, it returns zero with bias too. Bias only re-orders existing matches.

## The scoring model

```
score = bm25 − α · ln(distance_km + 1)
```

with `α = 0.10` (a constant in [`server/src/forward.rs::BiasCoord::DISTANCE_ALPHA`](../server/src/forward.rs)).

Why `ln` and not linear distance:

- Distance from the user is rarely linear in importance. A 1 km miss vs a 10 km miss matters more than a 100 km miss vs a 1000 km miss; both far cases are "definitely not local".
- `ln(distance_km + 1)` gives ~0.46 penalty at 100 km, ~0.92 at 10 000 km — bounded growth, so a clearly-better text match always wins.
- The `+1` in `ln(distance_km + 1)` keeps the penalty at zero distance well-defined (`ln(1) = 0`).

Why `α = 0.10`:

- Empirical. Tuned on the original same-name test set (`St Kilda` Melbourne vs SA, `Cambridge` UK vs MA). Higher values made the bias too aggressive (overrode genuinely better text matches); lower values made it too weak to flip same-name candidates.
- The TODO captures "tune α against a diverse same-name fixture" as a follow-up — bench-accuracy with bias enabled (post PR #15 + the bench-fixture upgrade) gives the first principled data.
- Operators wanting to evaluate sensitivity can edit the constant locally and rebuild.

## When the bias path activates

Bias only kicks in when the search goes through Tantivy. The handler skips the FST fast-path entirely when `bias_lat` / `bias_lng` are present, because the FST returns one globally-prominent pick and that contradicts the bias intent. Concretely (see `server/src/main.rs::search`):

```rust
let is_simple_freeform = ...
    && limit == 1
    && bias.is_none()  // ← bias suppresses the FST fast-path
    && ...;
```

Latency cost: ~50 µs at p50 vs the FST fast-path's ~400 ns for `limit == 1` queries. Real cost; only pay it when bias is supplied.

## The prominence-boost interaction

Without bias, `Forward::search_once` applies a Nominatim-style prominence boost to BM25 (`boosted_score`) — places of higher admin tier (city > town > suburb) get multiplicatively higher scores. This is the right default: a query for `Sydney` should return the city, not "Sydney Lane".

With bias, the prominence boost is **skipped**. Two competing intents (global admin tier vs. user's local geography) — the explicit signal wins. Without this, AU's South Australia St Kilda (rank 16, admin centre) outranked Melbourne's St Kilda (rank 19, suburb) even with a Melbourne bias because the prominence boost was a strong multiplicative factor on BM25 (~7 unit gap) while the distance penalty was a small additive factor (~0.5 unit gap on α=0.10).

Trade-off: a query for `Sydney` from a London bias loses the prominence boost. In practice this is fine — Sydney is unique enough that BM25 alone places the city above any "Sydney" street. If a deployment shows otherwise (a query like `Springfield` returning a Springfield Lane over Springfield IL because the prominence boost was disabled), the right fix is probably a query-shape decision: boost prominence AND apply distance penalty when both make sense. Untested today.

## What to pass and where to source it

**Caller decides what `lat` / `lng` mean:**

- Browser app: `navigator.geolocation.getCurrentPosition()` — most accurate when the user grants permission.
- Mobile app: GPS / FusedLocationProvider — sub-10 m accuracy when active.
- Backend with logged-in user: cached home/work coord from the user profile.
- Backend with only the request IP: chain through `/geocode/ip?ip=<x-forwarded-for>` to get a city-level coord. Cache the result; IP→coord rarely changes.

The geocoder doesn't know or care which source produced the coord. See [`docs/SDK_PATTERNS.md`](SDK_PATTERNS.md) for snippets per language.

**Privacy:** the coord is a coord, not an IP. No PII crosses the API boundary unless the caller explicitly logs the request line. The geocoder only logs request URLs at debug level by default.

## Bench-accuracy uses bias

The bench-accuracy regression suite (`scripts/run-bench-accuracy.sh` + `tools/regression-runner/src/bench_accuracy.rs`) passes the fixture's `lat_hint` / `lng_hint` per-row as `bias_lat` / `bias_lng` to `/search`. Without this it was measuring unbiased BM25 ranking — correct *recall* but not the product-realistic answer for ambiguous-name queries.

Pre-bias-fixture pass rate (US slice, sample of failures): 31/59 (52.5 %) — every wrong-Carrollton, wrong-Aurora, wrong-Montgomery counted as a failure even though production callers would pass bias and get the right one.

Post-bias-fixture (same hardware, same index): the bench measures product-realistic behaviour. Use the resulting pass rate to evaluate `α` and the prominence-skip rule.

## Things to think about if extending bias

- **Per-mode α** — `/autocomplete` typeahead probably wants a different bias strength than `/search`. Currently we only apply bias on `/search`; `/autocomplete` has none.
- **Caller-supplied α** — exposing `bias_strength` as a query param lets advanced callers tune. Probably not for v1; revisit if real traffic shows mode-dependent quality.
- **Region bias instead of point bias** — some queries want "anywhere in the UK" rather than "near London". Currently implementable as a country filter (`country_code=gb`) which is cheaper than passing a point. The point-bias path can stay focused.
- **Bias from request peer IP at the server side** — explicitly NOT in scope. Privacy-by-default; clients resolve their own coord.
- **Hard radius filter** — captured as a TODO follow-up to add a `/nearby` endpoint with a different semantic contract. Don't overload `/search` with a radius parameter — soft and hard semantics on the same endpoint break the mental model.

## References

- Industry implementations of proximity bias: Pelias `focus.point.{lat,lon}`, Mapbox `proximity`, HERE `at`, Radar `near`, Google Places `location` + `radius`.
- The bias scoring constant: [`server/src/forward.rs::BiasCoord::DISTANCE_ALPHA`](../server/src/forward.rs).
- The bias call site: `Forward::search_once` (re-rank step) and `main.rs::search` (FST fast-path gate).
- The validation: `BiasCoord::try_new(lat, lng)`.
- The tests: [`server/tests/search_bias.rs`](../server/tests/search_bias.rs).
- Open follow-ups (α tuning, BM25 boost on rank field, radius hard filter): [`TODO.md`](../TODO.md).
