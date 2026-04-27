# Forward search: ICU transliteration for multilingual users

Date: 2026-04-29
Status: implemented; planet rebuild required to take effect at worldwide scale.

## Why

35-40 % of geocoder users are not English-native. PR #11 added expansion of OSM `name:xx` tags so cross-language search resolves whenever the right tag exists — but two real failure classes remained:

1. **Russian/Greek/Arabic/Thai users typing native-script** queries for smaller cities/streets where OSM lacks `name:xx`. The query `Челябинск` survived the existing tokenizer as a single non-Latin token that didn't match anything in the index.
2. **English users typing Latin transliterations** like `Khartoum`, `Tashkent`, `Tver`, `Chelyabinsk` for places where OSM has only the canonical native-script name. Pre-PR these returned zero results.

ICU's CLDR transliteration rules deterministically convert non-Latin scripts to Latin without depending on OSM curators tagging it. PR #11's `name:xx` expansion handled 60-80 % of cross-language traffic; ICU closes the rest.

## What changed

**Build-time ICU transliteration.** Every non-Latin name (canonical OR `name:xx` alternate) gets one or more Latin transliterations emitted alongside the original text into:
- The Tantivy `name` field (concatenated with the existing canonical + alternates).
- The autocomplete FST as additional alias keys pointing at the same entry.

Query side is unchanged — Latin queries match the build-time-transliterated forms; native-script queries match the original stored forms. The runtime query-server doesn't link libicu.

### Library

`rust_icu_utrans` (FFI to system libicu). Pivoted from ICU4X (`icu_experimental`) after discovering its `compiled_data` feature ships baked CLDR transliterators for only Han / Kana / Ethiopic plus per-language Cyrillic BGN — Russian, Greek, Arabic, Thai, Devanagari, Persian Latin transliterators are NOT bundled. libicu has the full CLDR set out of the box.

The build binaries link libicu; the runtime query-server doesn't. Operators install `libicu-dev` (Debian/Ubuntu) or `icu4c` (macOS) on build hosts only. The Packer worldwide-build AMI provisions it; the serving AMI doesn't.

### Schemes per script

Multiple schemes per script where they meaningfully diverge for place-name recall:

| Script | Schemes |
|---|---|
| Cyrillic | `Cyrillic-Latin` (default scientific) + `Russian-Latin/BGN` |
| Greek | `Greek-Latin` + `Greek-Latin/BGN` |
| Han | `Han-Latin` (Pinyin) + `Han-Latin/Names` |
| Hiragana | `Hiragana-Latin` |
| Katakana | `Katakana-Latin` |
| Hangul | `Hangul-Latin` |
| Arabic | `Arabic-Latin` + `Arabic-Latin/BGN` (with article-strip post-pass) |
| Hebrew | `Hebrew-Latin` |
| Thai | `Thai-Latin` |
| Devanagari | `Devanagari-Latin` |

CJK + Hangul post-pass strips inter-syllable spaces (`běi jīng` → also `běijīng`) so users typing `Beijing` as one word match. Arabic post-pass strips the `al-` definite article so users typing `Khartoum` match a `al-Khartum` indexed form.

### Files modified

- `server/Cargo.toml` — `rust_icu_utrans` + `rust_icu_ustring` + `rust_icu_common` + `rust_icu_sys` deps; new `translit` feature (default-on); `required-features = ["translit"]` on `build-forward-index` and `build-autocomplete-fst`.
- `server/src/translit.rs` — **new.** Per-script detection (Unicode block ranges) + thread-local `UTransliterator` cache (rust_icu transliterators are not Sync) + multi-scheme transliteration + post-passes for Arabic/CJK. ~340 LOC including 8 unit tests.
- `server/src/lib.rs` — register the `translit` module behind the feature gate.
- `server/src/forward.rs::tantivy_doc` — `append_translit` helper called for the canonical name and each alternate.
- `server/src/bin/build_autocomplete_fst.rs::ingest` — `ingest_translit_keys` helper emits FST alias keys for each Latin transliteration.
- `server/tests/translit_forward_search.rs` — **new.** Behavioural tests gated on `GEOCODER_PLANET_INDEX_DIR`.
- `Dockerfile` — `libicu-dev` in the rust builder stage; `libicu72` runtime in the final stage (so build binaries shipped into the runtime image can run during `auto` mode).
- `packer/build-worldwide.pkr.hcl` — `libicu-dev` + `pkg-config` in the build-AMI provisioner.
- `README.md` — `Build from source` prereqs include `icu4c` (macOS) / `libicu-dev` (Debian); note the keg-only `PKG_CONFIG_PATH` requirement on macOS.

## Honest list of what still doesn't work

After this PR, these queries STILL fail. They're documented for follow-up TODOs:

1. **Exonym ≠ transliteration.** `Moscow`, `Cologne`, `The Hague`, `Vienna`, `Khartoum` are translated names, not transliterations. ICU on `الخرطوم` produces `al-Kharṭūm`, never `Khartoum`. The OSM `name:en` path keeps doing the heavy lifting for these. Mitigation: a small hand-curated exonym table is a follow-up.
2. **Multi-word CJK queries.** `渋谷駅` → `shibuyaeki` survives as one token after the post-pass. Tantivy won't match a doc with `shibuya` + `eki` as separate tokens. Defer to ICU `BreakIterator` follow-up.
3. **Pinyin tone ambiguity.** `xian` matches both `Xī'ān` (Xi'an) and `Xiàn` (Xian county) after diacritic stripping. Acceptable — recall improvement, not precision regression.
4. **Mixed-script queries.** `Москва Тверская ulitsa`: tokenizer splits at script boundary; ranking will be noisier but still correct.
5. **Tamil/Bengali/Telugu place names.** Devanagari is in scope; other Indic scripts less so.

## Index size + build-time impact

Empirically estimated against the i18n endonym data point:

- **Tantivy**: +8-12 % on top of current 2-3 GB. Latin transliterations are short tokens with cross-document redundancy; postings dedup absorbs most of it.
- **FST**: +5-8 % on the 210 MB. FSTs share prefixes aggressively.
- **Builder peak RAM**: +~600 MB during the per-country intern phase.
- **Build wall-time**: +18-25 % on `forward_classify` and `autocomplete_classify` phases. ICU translit benchmarks at ~1-3 µs/name; 200M planet names → +200-600 s on a 30-min phase. Acceptable.

Net within the 20-25 % growth envelope agreed previously, with headroom.

## Symmetry / determinism

- ICU CLDR transliteration is deterministic per ICU version. Build-host libicu version determines Latin output for every name.
- Cross-version determinism: ICU upgrades may change Latin output for some inputs. The current design DOES NOT record the libicu version in the index manifest — operators are responsible for rebuilding when they upgrade libicu. Captured as a TODO follow-up; low priority because runtime is libicu-free (no version-mismatch correctness issue, only stale-Latin-form recall regression).
- Build / query symmetry: query side is unchanged. Latin queries (whether typed directly or transliterated by ICU at build time) flow through the same `tokenize_user_input` / `normalise_prefix` pipeline that already handled them. No new symmetry invariant to maintain.

## Risks

1. **`rust_icu_*` API surface.** Mature crate (~5 years on crates.io), wraps stable ICU C API. Low risk.
2. **Planet rebuild required.** ~13.5 h on GB10. Land this with no other rebuild-required changes pending.
3. **macOS dev environment friction.** `icu4c` is keg-only; `PKG_CONFIG_PATH` must be set explicitly per cargo invocation. Documented in README. Linux build hosts (Debian Bookworm, the Packer AMI base) install via apt and pkg-config Just Works.
4. **Builder memory ceiling.** +600 MB peak adds to an already-tight planet build. If GB10 hits OOM, fall back to per-country sequential intern.

## For future maintainers

- Adding a new transliteration scheme: append a `(Script, "ICU-ID")` to `TRANSLITERATOR_DEFS` in `server/src/translit.rs`. Verify the ID is in the locally-installed libicu (`uconv -L | grep <id>` on Linux). Re-run `cargo test -p query-server --lib translit::` to lock the new output.
- Adding a new script (e.g. Tamil): add a Unicode block range to `script_of`, a new `Script` enum variant, and the appropriate ICU IDs to `TRANSLITERATOR_DEFS`.
- Skipping translit for a slim runtime build: `cargo build --no-default-features --features forward,grpc -p query-server --bin query-server`. The serving binary doesn't need libicu; the index files carry baked Latin forms.
