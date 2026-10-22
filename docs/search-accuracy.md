# Serving search corrections and accuracy

The native serving path supports country-aware spelling correction, street
directional aliases and bounded whitespace repair. REST and gRPC share the
same forward query implementation. Country-list searches use the same ranking
and proximity rules as searches across loaded country shards.

## Whitespace repair

Structured street-and-house requests require one explicit country and a city
or postcode. Freeform repair supports a leading house number with comma-separated
street/locality sections, or house/street text with explicit locality fields.
For example, `12 Ke ats Street, Testville, State, 2000` can resolve to
`12 Keats Street` when the address exists in the selected index.

The server borrows the original street span, enumerates one alphabetic split or
join, prunes against indexed name terms and retrieves strict address candidates.
House number, unit and supplied locality constraints remain mandatory. It
preserves existing matching addresses and declines competing address identities,
saturated result pages, conflicting fields and unexplained sections. Numeric
tokens and punctuation are not edited. Work is bounded to 128 street bytes,
12 words and 16 candidate strings. Units use the same normalization for search
and acceptance.

Freeform street/locality boundaries without commas or explicit context remain
outside this repair path. Inline units or postal/state hints that would be
reassigned to the street are declined. Existing search behavior is retained
when the repair cannot establish a supported interpretation.

G-NAF indexing also retains `STREET_SUFFIX_CODE`, preserving directional
distinctions such as `Parkes Place E` and `Parkes Place W`. Existing indexes
must be rebuilt to gain suffixes discarded by an earlier builder; the query
whitespace repair itself needs no rebuild or model dependency.

## Measured accuracy

Evaluated on 2026-10-22 against the existing AU, CA, GB, NZ and US indexes.
The fresh comparison used 5,000 new canonical street groups, excluding 62,464
previously used groups, and 45,312 requests per binary. Ground-truth labels and
coordinates were used only for scoring; no coordinate bias was supplied.

Correct first address within 50 metres for comma-separated freeform input:

| Input | Cases | Before freeform repair | After |
| --- | ---: | ---: | ---: |
| Clean | 10,000 | 90.36% | 90.36% |
| Extra space | 2,526 | 0.59% | 43.15% |
| Missing space | 2,578 | 3.45% | 44.14% |
| All noisy | 5,104 | 2.04% | 43.65% |

Repair recovered 2,124 matches with zero regressions on all eight measured
field/distance metrics. With locality context supplied, extra-space accuracy
was 83.31% and missing-space accuracy was 83.89%. Country-only requests remain
outside repair eligibility. All structured and plain freeform controls were
unchanged, and every fresh request returned HTTP 200. All 2,168 changed top
results, including changes between already-correct results, were correct within
50 metres. The noisy gain's 95% street-group bootstrap interval was +40.18 to
+43.09 percentage points.

The separate 11,000-case house-number corpus was rerun against both binaries.
All score vectors and first results were unchanged: 10,104 first-result matches
within 50 metres, zero HTTP errors and zero regressions. It includes units,
ranges, house suffixes, street abbreviations and character-deletion typos.

These are synthetic retrieval tests using imported index labels, not independent
address coverage or production traffic accuracy. Timings from overlapping local
benchmarks are not a production load test.

## Run serving checks

```sh
GEOCODER_INDEX_DIR="$PWD/data/index" cargo test --release -p query-server \
  --lib --test search_whitespace --test search_country_merge \
  --test search_structured --test search_bias --test fuzzy_fallback \
  --test oa_street_search --test forward_australia
cargo test --release -p query-server --bin build-gnaf-index --bin sample-addresses
cargo test --release -p regression-runner
python3 -m unittest discover -s scripts -p test_bench_house_numbers.py
make bench-house-numbers INDEX=./data/index SAMPLE=1000
```

The index-backed integration tests need the relevant country shards. Fixture
tests exercise ambiguity rejection, Unicode, units, ranges, numeric identifiers,
missing numbers, conflicting context and inline hints without model tooling.
On macOS, the Rust transliteration feature also needs the project's ICU build
environment. See the [regression guide](../tests/regression/README.md) for the
standalone house-number benchmark and combined field/coordinate checks.
