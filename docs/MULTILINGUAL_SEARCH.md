# Multilingual search via ICU transliteration

A reference doc for engineers working on the geocoder's cross-script search behaviour. Explains what transliteration is, why it matters for our product, what ICU provides, and how the implementation hangs together.

If you're looking for the per-PR change record, see [`docs/performance/forward-search-translit-2026-04-29.md`](performance/forward-search-translit-2026-04-29.md). This doc covers the concept; that one covers the patch.

---

## Why cross-script search matters here

35-40 % of geocoder users are not English-native. They search in:

- Cyrillic (Russian, Ukrainian, Bulgarian, Serbian, Macedonian)
- Greek
- Han ideographs (Mandarin, Cantonese, Japanese kanji)
- Hiragana / Katakana (Japanese)
- Hangul (Korean)
- Arabic / Persian / Urdu
- Hebrew
- Thai
- Devanagari (Hindi, Marathi, Nepali)

The OSM data we index has names in many of these scripts. Without script-aware search, two failure modes hit real users:

**Failure 1 — User types native script for a place that has no English name.**

> Russian user types `Челябинск` looking for the city of the same name. OSM has only `name=Челябинск` (no `name:en`). Pre-ICU the geocoder returns nothing — the Cyrillic query survives the existing tokenizer as one multi-byte token that doesn't match anything in the index.

**Failure 2 — English user types Latin transliteration for a non-Latin place.**

> English user types `Chelyabinsk`. OSM has only `name=Челябинск`. Pre-ICU the geocoder returns nothing — there's no Latin form of the name anywhere in the index.

Both failure modes evaporate if we **bake Latin transliterations into the index at build time**. The Russian user's native query matches the original Cyrillic stored form (which is preserved). The English user's Latin query matches the transliterated form (which is added). Same place, same hit, regardless of what the user types.

That's what this system does.

---

## Vocabulary primer

Engineers new to i18n often conflate these. They're distinct:

### Transliteration vs translation

- **Transliteration** is letter-by-letter (or syllable-by-syllable) script conversion. Same word, different script. `Москва` → `Moskva`. The pronunciation and meaning are preserved; only the writing system changes.
- **Translation** is meaning conversion across languages. `Москва` → `Moscow` is a translation (English exonym), not a transliteration. Different words for the same place.

Why this matters for us: ICU does transliteration. It produces `al-Kharṭūm` from `الخرطوم`, never `Khartoum`. To get `Khartoum`, you need the OSM `name:en` tag (curator-supplied translation) or a hand-curated exonym table.

### Endonym vs exonym

- **Endonym** is the name a place has in its native language. `Köln` (Cologne in German), `München` (Munich), `日本` (Japan in Japanese), `Москва` (Moscow in Russian).
- **Exonym** is the name a place has in another language. `Cologne`, `Munich`, `Japan`, `Moscow` are English exonyms.

OSM tags both: `name=Köln, name:en=Cologne`. PR #11 already wires up the exonym path (every `name:xx` becomes a searchable form). Transliteration is a SEPARATE bridge — it covers the case where neither endonym nor curated exonym is enough.

### Script vs language

- **Script** is a writing system: Latin, Cyrillic, Han, Arabic. A property of CHARACTERS.
- **Language** is a system of communication: Russian, Bulgarian, Mandarin, Cantonese. A property of WORDS.

Multiple languages can share a script (Russian + Bulgarian + Serbian all use Cyrillic). One language can use multiple scripts (Japanese uses Han + Hiragana + Katakana). Some scripts are language-specific (Thai is essentially only used for Thai).

ICU has rules at BOTH granularities — generic `Cyrillic-Latin` works for any Cyrillic input, while `Russian-Latin/BGN` is tuned for Russian conventions specifically.

### CLDR

The **Common Locale Data Repository** ([cldr.unicode.org](https://cldr.unicode.org/)) is the Unicode consortium's curated database of locale-specific information: date formats, number formats, plural rules, collation orders, and — relevantly — transliteration rules.

CLDR rules are written in a domain-specific language called the Unicode Transliteration Format. They're authored by linguists, version-controlled, and ship with every ICU release. Examples:

```
# Russian-Latin/BGN.txt (excerpt)
:: NFD ;
а ↔ a ;
б ↔ b ;
в ↔ v ;
ё ↔ ë ;       # not "yo" — BGN uses ë
ж ↔ zh ;
...
```

These rules are deterministic: same input + same CLDR version = same output, byte-for-byte. That property is critical — without it, build/runtime divergence would give us recall regressions that depend on which version of ICU was installed.

### ICU

The **International Components for Unicode** ([icu.unicode.org](https://icu.unicode.org/)) is a C/C++/Java library that consumes CLDR data and provides locale-aware string operations: collation, normalization, case-folding, transliteration, segmentation, formatting, parsing.

For us, only the transliteration component matters. We use it via `rust_icu_utrans` (FFI bindings).

---

## What ICU brings that naive approaches don't

Hand-rolled transliteration tables look easy and aren't. Cases ICU handles that surprise newcomers:

### Russian `ё` and stress

In Russian, `ё` is technically always pronounced `yo`. In practice writers omit the diaeresis and write `е` instead, and the reader infers from context. CLDR's BGN scheme renders both `ё` and stressed-`е` as `ë` (preserving the distinction); the scientific scheme uses `e` and `yo` differently. ICU has the rule; you don't write it.

### Greek final-sigma

Greek lowercase sigma has two forms: `σ` (mid-word) and `ς` (word-final). Both are the SAME letter — only the position differs. `Πειραιάς` ends in `ς`, but `σς` would also appear. ICU's `Greek-Latin` knows that `Π-ε-ι-ρ-α-ι-ά-ς` transliterates to `Peiraiás` (single `s` at word end), not `Peiraiaß` or similar. A character-by-character table would get this wrong.

### Arabic shadda + sun letters

Arabic has gemination marks (`shadda` ـّ) that double the consonant they sit on, and "sun letters" where the article `al-` assimilates to the following consonant: `ال + ش = اش` pronounced `ash-`. Transliterating `الشمس` (the sun) yields `ash-shams`, NOT `al-shams`. ICU has the rule.

### CJK Pinyin tone marks

`北京` in standard Pinyin is `Běijīng` with tone marks. In informal writing or English exonyms it's `Beijing`. ICU's `Han-Latin` produces the marked form (`běi jīng` with diacritics + space); we then post-process to strip the inter-syllable space (`běijīng`) and Tantivy's `AsciiFoldingFilter` strips the diacritics at index time (`beijing`).

### Greek-letter-name vs Greek-letter

`α` is the GREEK LETTER ALPHA, not the Latin letter `a`. They look similar; they're different codepoints. Transliteration converts `α` → `a`, but a character-equivalence table would either miss `α` (if keyed on Greek codepoints only) or false-match it to other Greek-looking Latin letters.

### Devanagari schwa deletion

In written Devanagari, every consonant carries an inherent `a` vowel. In spoken Hindi, that vowel is often dropped at word ends and certain syllable positions. Transliterating `मुंबई` straight character-by-character gives `mu-ṁ-ba-ī` (with the `ī`); contextual rules give `Mumbai`. ICU's `Devanagari-Latin` handles the schwa-deletion patterns.

These edge cases compound across scripts. Implementing them correctly per-script is years of linguistic work; using ICU is a `cargo add` plus a deployment dep.

---

## Architecture: build-time-only

```
                   ┌──────────────────────────────────────────────┐
                   │           BUILD HOST (one-time)              │
                   │                                              │
   OSM PBF  ───────▶ build-index ─▶ binary index files            │
                   │                                              │
                   │           build-forward-index ◀─ libicu      │
                   │              │                               │
                   │              ▼                               │
                   │      tantivy/ (with Latin forms baked in)    │
                   │                                              │
                   │       build-autocomplete-fst ◀─ libicu       │
                   │              │                               │
                   │              ▼                               │
                   │      fst_*.bin (with Latin alias keys)       │
                   └──────────────┬───────────────────────────────┘
                                  │ ship to S3
                                  ▼
                   ┌──────────────────────────────────────────────┐
                   │       SERVING HOST (libicu-free)             │
                   │                                              │
   user query ─────▶ query-server ─▶ search FST + Tantivy ─▶ hits │
                   │                                              │
                   └──────────────────────────────────────────────┘
```

**Why build-time-only:**

1. **Build-time runs once per index** (~30 min on planet); query-time runs per request. Computational asymmetry favours doing the work upfront.
2. **`UTransliterator` instances are expensive to construct** (~MB-scale CLDR rule tables loaded into the C++ instance). Loading them in the runtime query-server adds startup time and memory footprint.
3. **The runtime binary stays libicu-free.** Operators serving the index in production install the static `query-server` binary; they don't need libicu on serving hosts. Single-binary deployment promise preserved.
4. **The transliterations become plain UTF-8 in the index.** Tantivy reads them as strings; the FST stores them as byte sequences. No ICU at read time.

**The trade-off:** if a user's typed Latin transliteration doesn't match any of the schemes ICU emitted at build time, they miss. Mitigation: emit MULTIPLE schemes per script (BGN + scientific for Cyrillic, Pinyin + Names for Han, default + BGN for Greek and Arabic). Real-world Latin spellings of non-Latin places mostly cluster around one of those schemes.

---

## Where the code lives

```
server/src/translit.rs                    Public API: transliterate_for_index()
                                          Per-script Unicode block detection
                                          Thread-local UTransliterator cache
                                          Multi-scheme + post-pass logic

server/src/forward.rs::tantivy_doc        append_translit() helper
                                          Called for canonical name + each alternate

server/src/bin/build_autocomplete_fst.rs  ingest_translit_keys() helper
                                          Emits FST alias keys per Latin form

server/Cargo.toml                         translit feature gates the rust_icu_* deps
                                          Required by build binaries; optional for
                                          the runtime query-server
```

Walk through what happens when a place has Cyrillic OSM data:

1. **Build-time, in `tantivy_doc`:** input is OSM `name=Челябинск`, no `name:en` alternate.
2. **`append_translit("Челябинск")`** runs:
   - `script_of` detects Cyrillic via Unicode block `0x0400..=0x04FF`.
   - Loops over `TRANSLITERATOR_DEFS` for `(Script::Cyrillic, _)` entries.
   - For `Cyrillic-Latin` (default scientific), ICU produces `Čeljabinsk`.
   - For `Russian-Latin/BGN`, ICU produces `Chelyabinsk`.
   - Both are appended to the indexed name field.
3. **Tantivy's analyzer chain** (`SimpleTokenizer + AsciiFoldingFilter + LowerCaser`) tokenizes the augmented field:
   - `Челябинск` → one Cyrillic token (preserved).
   - `Čeljabinsk` → `čeljabinsk` → diacritic-fold → `celjabinsk`.
   - `Chelyabinsk` → `chelyabinsk`.
4. **Query time:** an English user types `Chelyabinsk`. Tokenization gives `chelyabinsk`. Tantivy posting list has it. Hit. Done.
5. **Symmetric query time:** a Russian user types `Челябинск`. Tokenization gives the Cyrillic token. Tantivy posting list has it (preserved from step 3). Hit.

---

## Schemes per script (reference)

Source: `TRANSLITERATOR_DEFS` in `server/src/translit.rs`.

| Script | Schemes emitted | Why multiple |
|---|---|---|
| Cyrillic | `Cyrillic-Latin` (scientific) + `Russian-Latin/BGN` | English users type BGN forms (`Chelyabinsk`); academic / library users type scientific forms (`Čeljabinsk`). Emit both. |
| Greek | `Greek-Latin` + `Greek-Latin/BGN` | Same — academic vs general-purpose. |
| Han (CJK) | `Han-Latin` + `Han-Latin/Names` | Modern Pinyin + a name-specific variant. |
| Hiragana | `Hiragana-Latin` | Single Hepburn scheme. |
| Katakana | `Katakana-Latin` | Same. |
| Hangul | `Hangul-Latin` | Revised Romanization. |
| Arabic | `Arabic-Latin` + `Arabic-Latin/BGN` | Plus a custom post-pass: emit a definite-article-stripped variant (`al-Khartum` → `Khartum`) so users typing without the article still match. |
| Hebrew | `Hebrew-Latin` | Single ISO scheme. |
| Thai | `Thai-Latin` | Single Royal scheme. |
| Devanagari | `Devanagari-Latin` | Single ISO scheme. |

**CJK + Hangul post-pass** (in `transliterate_for_index`): ICU's Han-Latin / Kana-Latin / Hangul-Latin produce **space-separated syllables** (`běi jīng`, `to kyo`, `seo ul`). Real users type CJK place names as one word (`Beijing`, `Tokyo`, `Seoul`). We emit a spaceless variant alongside the spaced one, so both match.

**Arabic article post-pass** (in `transliterate_for_index`): ICU produces `al-Khartum` from `الخرطوم`. Users typing `Khartum` (no article) wouldn't match. We emit a stripped variant (`Khartum`) alongside the canonical `al-Khartum`.

---

## What this DOESN'T solve (honest gaps)

After this system, certain queries STILL fail. Documented here so you don't waste time chasing them as bugs:

### Exonyms ≠ transliterations

- `Moscow` ≠ `Moskva`. The English name `Moscow` is a TRANSLATION, not a transliteration. ICU's CLDR rules will never produce `Moscow` from `Москва`.
- Same for `Cologne`/`Köln`, `The Hague`/`Den Haag`, `Vienna`/`Wien`, `Khartoum`/`al-Kharṭūm`, `Cairo`/`al-Qāhirah`.
- The OSM `name:en` path covers these for major cities. For minor places without `name:en`, English-typing users miss.
- **Mitigation:** hand-curated exonym table is in `TODO.md` as a follow-up. ~200 entries planet-wide, high leverage.

### Multi-word CJK queries

- `渋谷駅` (Shibuya Station) survives our pipeline as `shibuyaeki` — one token after the post-pass. A Tantivy doc indexed with `shibuya` and `eki` separately wouldn't match.
- Single-place CJK queries (`東京`, `北京`, `銀座`) work fine because the canonical OSM name is also one CJK token.
- **Mitigation:** `icu_segmenter` (ICU4X, pure Rust) handles word-breaking for CJK + Thai + Khmer. Adds ~3-5 days of work. Symmetric build/query change required (unlike translit which is build-only). Recorded in `TODO.md`.

### Pinyin tone-mark ambiguity

- ICU's `Han-Latin` produces `Xī'ān` for 西安 (city) and `Xiàn` for 县 (county). Both fold to `xian` after diacritic-stripping at index time.
- A query for `xian` matches BOTH. False positive on the wrong meaning.
- **Mitigation:** none for now. It's a recall improvement (places that previously returned nothing now return something), not a regression. If precision becomes a real problem, we'd need to keep tone marks in the FST keys — meaningful schema change, deferred.

### Mixed-script queries

- `Москва Тверская ulitsa` (half-Cyrillic, half-Latin) tokenizes at the script boundary. Each token resolves through its own path; ranking is noisier than a pure-script query.
- Acceptable v1 behaviour — these are extremely rare in real traffic.

---

## Performance characteristics

**At build time:**
- Translit runs in `tantivy_doc` and `Candidate.aliases` ingest, both inside Rayon `par_iter`. Each thread has its own `UTransliterator` cache via `thread_local!` (rust_icu's `UTransliterator` is `!Sync`).
- Construction cost: ~ms per `UTransliterator`. Amortized to ~µs/name across the build because each thread constructs each transliterator once.
- Per-name cost: ~1-3 µs for ASCII / pre-detected Latin (immediate exit via `script_of`); ~5-15 µs for non-Latin names depending on length and number of schemes that match.
- Planet impact: ~+200-600 s on the `forward_classify` and `autocomplete_classify` phases (currently ~30 min combined). Acceptable.
- Memory peak: +~600 MB during the per-country intern phase (each thread holds N transliterator instances + working buffers).

**At query time:**
- Zero. Translit doesn't run at query time; the runtime query-server doesn't link libicu.

**Index size:**
- Tantivy: +8-12 % on top of current 2-3 GB. Latin transliterations are short tokens with high cross-document redundancy; postings dedup absorbs most of it.
- FST: +5-8 % on the 210 MB. FSTs share prefixes aggressively.

---

## How to verify changes

### Unit tests
```bash
PKG_CONFIG_PATH=$(brew --prefix icu4c)/lib/pkgconfig \
    cargo test -p query-server --lib translit::
```
Pins specific Latin outputs for representative inputs. If you change a scheme or post-pass, expect to update test expectations.

### Integration tests (planet-scale)
```bash
GEOCODER_PLANET_INDEX_DIR=/path/to/built/index \
    cargo test -p query-server --test translit_forward_search
```
Behavioural — `Chelyabinsk`, `北京`, `الخرطوم` style queries against a real index. Skips if the env var isn't set.

### Manual smoke
```bash
PKG_CONFIG_PATH=$(brew --prefix icu4c)/lib/pkgconfig \
    cargo run --release --bin query-server -- /path/to/index 127.0.0.1:13580 &

curl -s 'http://localhost:13580/search?q=Chelyabinsk' | jq '.results[0].name'
curl -s 'http://localhost:13580/search?q=Челябинск' | jq '.results[0].name'
# Both should return the same place.
```

### Inspecting what ICU produced
```rust
// One-off: drop into a test or scratch binary.
use query_server::translit::transliterate_for_index;
fn main() {
    println!("{:?}", transliterate_for_index("Челябинск"));
    // ["Čeljabinsk", "Chelyabinsk"]
}
```

---

## How to extend

### Add a new transliteration scheme for an existing script

1. Find the ICU ID. On Linux: `uconv -L | grep -i <script>`. Examples: `Russian-Latin/BGN`, `Han-Latin/Names`, `Hangul-Latin`.
2. Append `(Script::X, "ICU-ID")` to `TRANSLITERATOR_DEFS` in `server/src/translit.rs`.
3. Add a unit test pinning the expected Latin output for a representative input.
4. Rebuild the index — translit changes don't take effect until the next build.

### Add support for a new script

E.g. Tamil (which we currently don't index):

1. Add a `Script::Tamil` variant to the enum in `translit.rs`.
2. Add the Tamil Unicode block (`0x0B80..=0x0BFF`) to the `script_of` match.
3. Look up the ICU ID (`Tamil-Latin` or similar; verify with `uconv -L`).
4. Append `(Script::Tamil, "Tamil-Latin")` to `TRANSLITERATOR_DEFS`.
5. Add a unit test.
6. Add a behavioural test in `translit_forward_search.rs` using a known Tamil place.

### Disable translit for a slim deployment

Operators serving an exclusively-Latin corpus (no Russian/CJK/Arabic data) can build without translit:

```bash
cargo build --release \
    --no-default-features --features forward,grpc \
    -p query-server --bin query-server
```

The build binaries (`build-forward-index`, `build-autocomplete-fst`) require translit (`required-features = ["translit"]` in `Cargo.toml`) — operators NOT serving non-Latin scripts can simply not run them, but if they DO run them, translit is part of the build.

For Docker: edit the `Dockerfile` to drop the build binaries from the runtime stage and remove `libicu72` from the runtime apt install. The result is a smaller image with the runtime query-server only.

### Debug a query that doesn't transliterate as expected

Common causes:

1. **The OSM name is in a script we don't handle.** Check `script_of(input)`. If it returns `LatinOrOther` for a non-Latin input, add the script's Unicode block.
2. **The ICU ID is unsupported by the local libicu version.** Check `uconv -L | grep <id>` on the build host. If missing, that scheme is silently skipped (logged at debug level).
3. **The post-pass stripped something it shouldn't.** Common: Arabic article-strip on a name that legitimately starts with `al-` (e.g. a person's name, not a place). Add the test case, then patch the post-pass condition.
4. **Build vs runtime asymmetry.** ICU is build-time only; if the BUILD host has a different libicu version than expected, the index might have unexpected Latin forms. Operators rebuild on libicu upgrade.

---

## References

- ICU project: https://icu.unicode.org/
- CLDR transliteration overview: https://cldr.unicode.org/index/cldr-spec/transliteration-guidelines
- BCP-47 transform extension (the `t-` syntax in transliterator IDs): https://www.unicode.org/reports/tr35/#Transforms
- The `rust_icu` umbrella crate: https://crates.io/crates/rust_icu
- Our patch snapshot: [`forward-search-translit-2026-04-29.md`](performance/forward-search-translit-2026-04-29.md)
- Open follow-ups (CJK segmentation, exonym table, libicu version manifest): [`TODO.md`](../TODO.md)
- Related PRs: #11 (i18n endonym + abbreviation fold, ASCII-Latin only), #14 (this work — generalised transliteration via ICU)
