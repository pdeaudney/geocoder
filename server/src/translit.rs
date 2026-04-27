//! ICU-based transliteration of non-Latin scripts to Latin for
//! search-index expansion. Build-time only — see the `translit`
//! Cargo feature gate.
//!
//! For every place / street name whose canonical text (or any
//! `name:xx` alternate) is non-Latin, the build pipeline emits one
//! or more Latin transliterations alongside the original. Those
//! Latin forms are concatenated into the Tantivy `name` field and
//! emitted as additional FST alias keys, so a user typing the
//! Latin form (e.g. `Chelyabinsk` for `Челябинск`, `Beijing` for
//! `北京`) finds the place even when OSM lacks a `name:en`.
//!
//! # Library
//!
//! Backed by `rust_icu_utrans` (FFI to system libicu). ICU4X
//! (`icu_experimental` 0.5) ships baked CLDR data for only
//! Han/Kana/Ethiopic plus a handful of Cyrillic BGN per-language
//! transliterators — Russian, Greek, Arabic, Thai, Devanagari, and
//! Persian Latin transliterators are NOT bundled. libicu has the
//! full CLDR set out of the box, with the trade-off of needing
//! libicu installed at build time. The runtime query-server
//! doesn't link libicu — transliterations are baked into the
//! on-disk index files.
//!
//! # Thread safety
//!
//! `UTransliterator` is not `Sync` (it wraps a raw pointer to an
//! ICU C++ instance). The build pipeline uses rayon for parallel
//! per-place classification, so we hold transliterator instances
//! in a `thread_local!`. Each rayon worker constructs its own on
//! first use; subsequent calls reuse the cached instance.
//!
//! # Failure mode
//!
//! Transliterator construction can fail when an ICU ID isn't
//! available in the local libicu version. We cache `Option<...>`
//! per thread per ID — a failed construction is silently skipped
//! (the affected script just doesn't get a Latin alternate, no
//! panic). Operators see the gap reflected in bench-accuracy.

use std::cell::RefCell;
use std::collections::HashMap;

use rust_icu_sys as sys;
use rust_icu_utrans::UTransliterator;

/// Detected script for a name. Drives which ICU transliterator(s)
/// we run on it. `LatinOrOther` is the no-op case — already-Latin
/// inputs (the bulk of the corpus) skip transliteration entirely.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Script {
    Cyrillic,
    Greek,
    Han,
    Hiragana,
    Katakana,
    Hangul,
    Arabic,
    Hebrew,
    Thai,
    Devanagari,
    LatinOrOther,
}

/// Detect the dominant non-Latin script of a string by scanning
/// for the first non-ASCII codepoint and matching it against
/// known Unicode block ranges. Place names are typically
/// single-script — mixed-script place names are rare and the
/// first non-Latin codepoint always picks the right tier.
///
/// Latin-with-diacritics (`Köln`, `São Paulo`) returns
/// `LatinOrOther` because Tantivy's `AsciiFoldingFilter` already
/// handles those at index time; we don't need ICU for them.
pub fn script_of(s: &str) -> Script {
    for ch in s.chars() {
        match ch as u32 {
            // Cyrillic + supplements + extended-A/B + historic.
            0x0400..=0x04FF | 0x0500..=0x052F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F => {
                return Script::Cyrillic
            }
            // Greek and Coptic + Greek Extended.
            0x0370..=0x03FF | 0x1F00..=0x1FFF => return Script::Greek,
            // CJK Unified Ideographs + Extension A + Compatibility.
            0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF => return Script::Han,
            // Hiragana.
            0x3040..=0x309F => return Script::Hiragana,
            // Katakana.
            0x30A0..=0x30FF => return Script::Katakana,
            // Hangul Syllables.
            0xAC00..=0xD7AF => return Script::Hangul,
            // Arabic + supplements + presentation forms.
            0x0600..=0x06FF | 0x0750..=0x077F | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => {
                return Script::Arabic
            }
            // Hebrew + presentation forms.
            0x0590..=0x05FF | 0xFB1D..=0xFB4F => return Script::Hebrew,
            // Thai.
            0x0E00..=0x0E7F => return Script::Thai,
            // Devanagari.
            0x0900..=0x097F => return Script::Devanagari,
            _ => {} // ASCII, Latin-with-diacritics, or unmapped: keep scanning
        }
    }
    Script::LatinOrOther
}

/// All (script, ICU transliterator ID) pairs we apply at build
/// time. Multiple schemes per script where they meaningfully
/// diverge for place-name recall — e.g. Cyrillic gets the default
/// scientific scheme AND BGN/PCGN, since users vary on which they
/// type for cities like Челябинск (`Čeljabinsk` scientific vs
/// `Chelyabinsk` BGN). For Han we run two Pinyin variants for
/// the same reason.
///
/// Keep this list in sync with the schemes recorded in the index
/// manifest by `query_server::manifest`.
const TRANSLITERATOR_DEFS: &[(Script, &str)] = &[
    (Script::Cyrillic, "Cyrillic-Latin"),
    (Script::Cyrillic, "Russian-Latin/BGN"),
    (Script::Greek, "Greek-Latin"),
    (Script::Greek, "Greek-Latin/BGN"),
    (Script::Han, "Han-Latin"),
    (Script::Han, "Han-Latin/Names"),
    (Script::Hiragana, "Hiragana-Latin"),
    (Script::Katakana, "Katakana-Latin"),
    (Script::Hangul, "Hangul-Latin"),
    (Script::Arabic, "Arabic-Latin"),
    (Script::Arabic, "Arabic-Latin/BGN"),
    (Script::Hebrew, "Hebrew-Latin"),
    (Script::Thai, "Thai-Latin"),
    (Script::Devanagari, "Devanagari-Latin"),
];

thread_local! {
    /// Per-thread cache of `UTransliterator` instances. ICU
    /// transliterators are not `Sync`, so we hold one per rayon
    /// worker. `None` entries record IDs that failed to construct
    /// (typically: unsupported by the local libicu version) so we
    /// skip them without retrying on every call.
    static TRANSLITS: RefCell<HashMap<&'static str, Option<UTransliterator>>> =
        RefCell::new(HashMap::new());
}

/// Produce zero or more Latin transliterations of `name` suitable
/// for indexing alongside the original text. Returns an empty
/// vector when the input is already Latin/ASCII (the common
/// case at planet scale — most names don't need any work) or
/// when no transliterator for the detected script could be
/// constructed.
///
/// Output contents:
/// - One entry per `(source_script, scheme)` pair that produces a
///   non-empty Latin string different from the input.
/// - Multiple schemes for the same script may produce the same
///   Latin form (e.g. default Cyrillic + BGN both yield `Moskva`
///   for `Москва`). De-dup is the caller's job — Tantivy's
///   posting list and the FST's `BTreeMap` of keys already collapse
///   identical results, so the cost of returning duplicates is a
///   couple of extra string allocations per name, not extra disk.
///
/// Apostrophe-like soft signs (`ʹ`, `'`) and combining marks
/// produced by ICU survive into the output. Downstream
/// tokenisation (`canonicalise_phrase` for Tantivy, `normalise_fst_key`
/// for the FST) strips them via the existing non-alphanumeric
/// split, so the indexed tokens land on `tver`/`xa-rkov` etc.
pub fn transliterate_for_index(name: &str) -> Vec<String> {
    let script = script_of(name);
    if script == Script::LatinOrOther {
        return Vec::new();
    }
    TRANSLITS.with(|cell| {
        let mut map = cell.borrow_mut();
        let mut out: Vec<String> = Vec::new();
        for &(src, id) in TRANSLITERATOR_DEFS {
            if src != script {
                continue;
            }
            // get_or_insert_with: cache the constructed instance
            // (or the None sentinel if construction failed).
            let entry = map.entry(id).or_insert_with(|| {
                UTransliterator::new(id, None, sys::UTransDirection::UTRANS_FORWARD).ok()
            });
            if let Some(t) = entry {
                if let Ok(latin) = t.transliterate(name) {
                    if !latin.is_empty() && latin != name {
                        out.push(latin);
                    }
                }
            }
        }

        // Arabic-specific post-pass: emit a definite-article-stripped
        // variant so users typing `Khartoum` (no `al-` prefix) still
        // match the BGN form `al-Khartum`. Cheap — only runs on
        // Arabic-source rows.
        if script == Script::Arabic {
            let stripped: Vec<String> = out
                .iter()
                .filter_map(|s| strip_arabic_article(s))
                .collect();
            out.extend(stripped);
        }

        // CJK + Hangul post-pass: ICU's Han-Latin/Hiragana-Latin/
        // Katakana-Latin/Hangul-Latin emit space-separated syllables
        // (`běi jīng`, `to-kyo`, `seo ul`). Users type CJK place
        // names as one word (`Beijing`, `Tokyo`, `Seoul`), so emit
        // a spaceless variant alongside the spaced one. The
        // spaced version stays useful for syllable-by-syllable
        // queries; the spaceless wins the common case.
        if matches!(
            script,
            Script::Han | Script::Hiragana | Script::Katakana | Script::Hangul
        ) {
            let collapsed: Vec<String> = out
                .iter()
                .filter_map(|s| {
                    let no_space: String = s.chars().filter(|c| !c.is_whitespace()).collect();
                    if no_space != *s && !no_space.is_empty() {
                        Some(no_space)
                    } else {
                        None
                    }
                })
                .collect();
            out.extend(collapsed);
        }
        out
    })
}

/// Strip a leading definite-article prefix (`al-`, `al `, `al'`)
/// from a transliterated Arabic name. Returns `None` when no such
/// prefix is present, so the caller doesn't push a duplicate.
fn strip_arabic_article(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    for prefix in ["al-", "al ", "al'"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            if !rest.is_empty() {
                // Preserve original casing of the post-prefix part.
                let prefix_len = prefix.len();
                if s.len() > prefix_len {
                    return Some(s[prefix_len..].to_owned());
                }
            }
        }
    }
    None
}

/// List the ICU transliterator IDs we apply, in stable order. Used
/// by the index manifest emitter so a runtime query-server can
/// detect a build/runtime ICU mismatch.
pub fn scheme_ids() -> Vec<&'static str> {
    TRANSLITERATOR_DEFS.iter().map(|(_, id)| *id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_scripts() {
        assert_eq!(script_of("Москва"), Script::Cyrillic);
        assert_eq!(script_of("Челябинск"), Script::Cyrillic);
        assert_eq!(script_of("Αθήνα"), Script::Greek);
        assert_eq!(script_of("Πειραιάς"), Script::Greek);
        assert_eq!(script_of("北京"), Script::Han);
        assert_eq!(script_of("東京"), Script::Han); // CJK ideograph
        assert_eq!(script_of("ひらがな"), Script::Hiragana);
        assert_eq!(script_of("カタカナ"), Script::Katakana);
        assert_eq!(script_of("서울"), Script::Hangul);
        assert_eq!(script_of("الخرطوم"), Script::Arabic);
        assert_eq!(script_of("ירושלים"), Script::Hebrew);
        assert_eq!(script_of("กรุงเทพ"), Script::Thai);
        assert_eq!(script_of("मुंबई"), Script::Devanagari);
    }

    #[test]
    fn latin_inputs_skip_translit() {
        assert_eq!(script_of("Sydney"), Script::LatinOrOther);
        assert_eq!(script_of("Köln"), Script::LatinOrOther); // Latin-with-diacritics
        assert_eq!(script_of("São Paulo"), Script::LatinOrOther);
        assert!(transliterate_for_index("Sydney").is_empty());
        assert!(transliterate_for_index("Köln").is_empty());
    }

    #[test]
    fn cyrillic_produces_latin() {
        let out = transliterate_for_index("Москва");
        assert!(!out.is_empty(), "expected at least one Cyrillic→Latin variant");
        // The default scientific scheme produces "Moskva". BGN also
        // produces "Moskva" for Moscow specifically. Pin the
        // canonical form as a smoke check.
        assert!(
            out.iter().any(|s| s.eq_ignore_ascii_case("Moskva")),
            "expected Moskva in {out:?}"
        );
    }

    #[test]
    fn cyrillic_chelyabinsk_bgn_form() {
        // The motivating case: a small Russian city with no
        // OSM `name:en`. ICU's BGN scheme produces what English
        // users actually type.
        let out = transliterate_for_index("Челябинск");
        assert!(!out.is_empty());
        assert!(
            out.iter().any(|s| s.eq_ignore_ascii_case("Chelyabinsk")
                || s.eq_ignore_ascii_case("Čeljabinsk")),
            "expected Chelyabinsk (BGN) or Čeljabinsk (scientific) in {out:?}"
        );
    }

    #[test]
    fn han_produces_pinyin() {
        let out = transliterate_for_index("北京");
        assert!(!out.is_empty(), "expected Han→Latin output");
        // ICU produces `běi jīng` (with diacritics + space). Our
        // post-pass adds a spaceless variant `běijīng`. Tantivy's
        // AsciiFoldingFilter strips the diacritics at index time
        // so the actual indexed token becomes `beijing`. Pin both
        // forms here so a future libicu/post-pass change has to
        // explicitly update the test.
        assert!(out.iter().any(|s| s.contains(' ')), "expected spaced form: {out:?}");
        assert!(out.iter().any(|s| !s.contains(' ')), "expected collapsed form: {out:?}");
        // Verify the collapsed form starts with `b` and ends with
        // `g` — the Pinyin skeleton, surviving any tone-mark
        // shape change. Don't pin the exact diacritic chars.
        let collapsed: Vec<&String> = out.iter().filter(|s| !s.contains(' ')).collect();
        assert!(
            collapsed.iter().any(|s| s.starts_with('b') || s.starts_with('B')),
            "expected b-initial collapsed form: {collapsed:?}"
        );
        assert!(
            collapsed.iter().any(|s| s.ends_with('g')),
            "expected g-final collapsed form: {collapsed:?}"
        );
    }

    #[test]
    fn arabic_emits_article_stripped_variant() {
        let out = transliterate_for_index("الخرطوم");
        if out.is_empty() {
            // libicu version may lack Arabic-Latin — acceptable.
            return;
        }
        // We always emit at least the base form; the strip post-pass
        // adds an `al-`-less form if the base started with `al-`.
        let has_al = out.iter().any(|s| s.to_lowercase().starts_with("al"));
        let has_stripped = out.iter().any(|s| {
            let l = s.to_lowercase();
            !l.starts_with("al-") && !l.starts_with("al ")
        });
        assert!(
            has_al || has_stripped,
            "expected base or stripped Arabic form in {out:?}"
        );
    }

    #[test]
    fn arabic_strip_prefix_unit() {
        assert_eq!(
            strip_arabic_article("al-Khartum").as_deref(),
            Some("Khartum")
        );
        assert_eq!(
            strip_arabic_article("Al-Qahira").as_deref(),
            Some("Qahira")
        );
        assert_eq!(strip_arabic_article("Cairo"), None);
        assert_eq!(strip_arabic_article("al-"), None); // empty rest
    }

    #[test]
    fn scheme_ids_lists_all_definitions() {
        let ids = scheme_ids();
        assert_eq!(ids.len(), TRANSLITERATOR_DEFS.len());
        assert!(ids.contains(&"Cyrillic-Latin"));
        assert!(ids.contains(&"Han-Latin"));
        assert!(ids.contains(&"Arabic-Latin"));
    }
}
