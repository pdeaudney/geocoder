# Serving search corrections and accuracy

The native serving path supports country-aware spelling correction, street directional aliases and bounded whitespace repair.
REST and gRPC share the same forward query implementation.
Country-list searches use the same ranking and proximity rules as searches across loaded country shards.

## Whitespace repair

Structured street-and-house requests require one explicit country and a city or postcode.
Freeform repair supports a leading house number with comma-separated street/locality sections, undelimited street/locality text, or house/street text with explicit locality fields.
For example, `12 Ke ats Street, Testville, State, 2000` can resolve to `12 Keats Street` when the address exists in the selected index.

The server borrows the original street span, enumerates one alphabetic split or join, prunes against indexed name terms and retrieves strict address candidates.
House number, unit and supplied locality constraints remain mandatory.
It preserves existing matching addresses and declines competing address identities, saturated result pages, conflicting fields and unexplained sections.
Numeric tokens and punctuation are not edited.
Work is bounded to 128 street bytes, 12 words and 16 candidate strings.
Units use the same normalization for search and acceptance.

Without commas, the server preserves raw text spans, extracts a leading unit and house plus a trailing postcode, and proposes street boundaries at existing spaces.
Indexed locality/state terms prune proposals.
A retrieved address must explain the entire remaining tail as its city, state (when a postcode is supplied), or city plus state.
Full state names and existing AU/US/CA abbreviations work.
All interpretations share a 32-query budget; an overflow or full result page causes abstention.
The plain adapter also declines inputs over 512 bytes or 24 whitespace-separated words.
Unchanged interpretations are checked before any edits.
The adapter infers boundaries from raw text and stored address fields; evaluation labels supply no parser hints.

Only one alphabetic split or join inside the proposed street is repaired.
Locality typos, arbitrary word order and trailing unit placement remain outside this adapter.
Existing search behavior is retained when a repair is declined.

G-NAF indexing also retains `STREET_SUFFIX_CODE`, preserving directional distinctions such as `Parkes Place E` and `Parkes Place W`.
Existing indexes must be rebuilt to gain suffixes discarded by an earlier builder; the query whitespace repair itself needs no rebuild or model dependency.

## Measured accuracy

### Current figures by query type

Accuracy here means that the first result matches the requested house, street, country, recorded unit and postcode when supplied, and lies within 50 metres of the reference coordinate.
The current holdout uses AU, CA, GB, NZ and US indexes.

| Clean-query format | Cases | Accuracy |
| --- | ---: | ---: |
| Structured address fields | 10,000 | 92.28% |
| Freeform with commas | 10,000 | 90.18% |
| Freeform without commas | 10,000 | 90.77% |

The separate house-number benchmark includes units, numeric ranges, house suffixes, abbreviation variants and character-deletion typos.

| House-number benchmark variant | Cases | Accuracy |
| --- | ---: | ---: |
| Structured | 5,000 | 95.38% |
| Freeform | 5,000 | 91.38% |
| Street abbreviation | 500 | 92.80% |
| Character-deletion typo | 500 | 60.40% |

The cohorts have different query distributions, so their figures are reported separately.

### Undelimited freeform repair - 2026-10-23

Compared with serving commit `22395eb` on the same five-country indexes.
The fresh holdout contains 5,000 new street groups, excluding 67,464 earlier groups, and 45,660 requests per binary.
The policy was selected using validation and adversarial fixtures before inspecting fresh accuracy outcomes.

Correct first address within 50 metres for plain input without commas:

| Input | Cases | Before | After |
| --- | ---: | ---: | ---: |
| Clean | 10,000 | 90.77% | 90.77% |
| Extra space | 2,674 | 0.34% | 18.96% |
| Missing space | 2,546 | 3.89% | 38.49% |
| All noisy | 5,220 | 2.07% | 28.49% |

There were **1,379 recovered queries and zero regressions** on all eight field/distance metrics.
The noisy gain's 95% street-group bootstrap interval was +25.12 to +27.62 percentage points.
With locality/postcode context supplied, noisy accuracy rose from 50/2,580 (1.94%) to 1,429/2,580 (55.39%).
Country-only controls were unchanged.
Gains by country: AU 292, CA 207, GB 302, NZ 417, US 161.

All 1,412 changed top results were correct within 50 metres.
Clean first results and scores in all three formats, and structured/comma controls, were unchanged.
Three structured requests returned HTTP 400 in both binaries because their source house numbers exceeded the 32-byte API limit; these remain counted as failures.
All other fresh requests returned HTTP 200.

The separate 11,000-case house-number/character-typo benchmark retained identical score vectors and first results: 10,104 correct within 50 metres.
All 279 Rust tests passed, including the fixture that preserves `Rose Bank Road` rather than reinterpreting it as `Rosebank` in locality `Road`.

A separate local load check used concurrency four, a fixed 90% clean / 10% whitespace-noisy mix and 2,000 measured requests per binary, with warmups and baseline/candidate/candidate/baseline ordering:

| Overall latency | Before | After |
| --- | ---: | ---: |
| p95 | 12.84 ms | 12.64 ms |
| p99 | 35.51 ms | 37.09 ms |

This measures a local synthetic workload, not production load or independent address coverage.
The measured p99 increased by 1.58 ms.
Binary hashes, corpora, reports, policy history and commands are saved in `data/byt5/plain-freeform-20261023/README.md` and `supplemental-audit.json`.
The custom model/research files were verified unchanged; no model or index rebuild is required.
The large holdout's datasets and reports are local evaluation artifacts; the commands below run the committed serving tests and standalone house-number benchmark.

### Earlier comma adapter evaluation

Evaluated on 2026-10-22 against the existing AU, CA, GB, NZ and US indexes.
The fresh comparison used 5,000 new canonical street groups, excluding 62,464 previously used groups, and 45,312 requests per binary.
Ground-truth labels and coordinates were used only for scoring; no coordinate bias was supplied.

Correct first address within 50 metres for comma-separated freeform input:

| Input | Cases | Before freeform repair | After |
| --- | ---: | ---: | ---: |
| Clean | 10,000 | 90.36% | 90.36% |
| Extra space | 2,526 | 0.59% | 43.15% |
| Missing space | 2,578 | 3.45% | 44.14% |
| All noisy | 5,104 | 2.04% | 43.65% |

Repair recovered 2,124 matches with zero regressions on all eight measured field/distance metrics.
With locality context supplied, extra-space accuracy was 83.31% and missing-space accuracy was 83.89%.
Country-only requests remain outside repair eligibility.
All structured and plain freeform controls were unchanged, and every fresh request returned HTTP 200.
All 2,168 changed top results, including changes between already-correct results, were correct within 50 metres.
The noisy gain's 95% street-group bootstrap interval was +40.18 to +43.09 percentage points.

The separate 11,000-case house-number corpus was rerun against both binaries.
All score vectors and first results were unchanged: 10,104 first-result matches within 50 metres, zero HTTP errors and zero regressions.
It includes units, ranges, house suffixes, street abbreviations and character-deletion typos.

These are synthetic retrieval tests using imported index labels, not independent address coverage or production traffic accuracy.
Timings from overlapping local benchmarks are not a production load test.

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

The index-backed integration tests need the relevant country shards.
Fixture tests exercise ambiguity rejection, Unicode, units, ranges, numeric identifiers, missing numbers, conflicting context and inline hints without model tooling.
On macOS, the Rust transliteration feature also needs the project's ICU build environment.
See the [regression guide](../tests/regression/README.md) for the standalone house-number benchmark and combined field/coordinate checks.
