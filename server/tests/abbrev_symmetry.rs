//! Symmetry test for the place-name abbreviation fold.
//!
//! `Saint Kilda` and `St Kilda` must produce the same FST key, the
//! same Tantivy `name` field token set, and the same query-time
//! tokens. Drift between any of those four sites silently breaks
//! Saint ↔ St lookups at scale.
//!
//! The build-time FST normaliser (`normalise_fst_key` in
//! `bin/build_autocomplete_fst.rs`) calls
//! `autocomplete::fold_place_abbreviations` directly. The Tantivy
//! build path goes through `forward::canonicalise_phrase`, and the
//! query side goes through `autocomplete::normalise_prefix` and
//! `forward::tokenize_user_input`. Exercising the public helpers in
//! this test plus the runtime functions covers all four sites.

use query_server::autocomplete::{fold_place_abbreviations, normalise_prefix};
use query_server::forward::{canonicalise_phrase, tokenize_user_input};

#[test]
fn saint_collapse_at_leading_position() {
    for variant in ["Saint Kilda", "St Kilda", "saint kilda", "ST kilda"] {
        assert_eq!(normalise_prefix(variant), "st kilda", "input: {variant:?}");
    }
}

#[test]
fn mount_collapse_at_leading_position() {
    for variant in ["Mount Pleasant", "Mt Pleasant", "MT Pleasant"] {
        assert_eq!(normalise_prefix(variant), "mt pleasant", "input: {variant:?}");
    }
}

#[test]
fn fort_collapse_at_leading_position() {
    for variant in ["Fort Worth", "Ft Worth"] {
        assert_eq!(normalise_prefix(variant), "ft worth", "input: {variant:?}");
    }
}

/// Trailing `St` is the OSM convention for "Street" (e.g. `Main St`).
/// It must NOT collapse to `st` (= Saint) — that's the only ambiguity
/// the position rule handles. Pinned explicitly so a future edit can't
/// quietly drop the rule.
#[test]
fn trailing_st_stays_literal() {
    assert_eq!(normalise_prefix("Main St"), "main st");
    assert_eq!(normalise_prefix("Hampton St"), "hampton st");
    assert_eq!(tokenize_user_input("Main St"), vec!["main", "st"]);
    assert_eq!(tokenize_user_input("Hampton St"), vec!["hampton", "st"]);
}

/// Saint as a leading prefix of a 3+ token phrase still folds.
/// `St James Court` is a real-world saintly street name where `St`
/// means Saint, not Street.
#[test]
fn saint_in_longer_phrase_still_collapses() {
    assert_eq!(normalise_prefix("Saint James Court"), "st james court");
    assert_eq!(normalise_prefix("St James Court"), "st james court");
    assert_eq!(
        tokenize_user_input("Saint James Court"),
        vec!["st", "james", "court"]
    );
}

#[test]
fn sainte_foy_variants_converge() {
    for variant in ["Sainte-Foy", "Ste-Foy", "Ste Foy", "Sainte Foy"] {
        assert_eq!(normalise_prefix(variant), "st foy", "input: {variant:?}");
    }
}

/// The Tantivy build path (`canonicalise_phrase`) and the FST runtime
/// path (`normalise_prefix`) must agree on the abbreviation fold for
/// ASCII inputs. canonicalise_phrase preserves the original casing
/// for non-abbreviated words — tantivy's LowerCaser handles that at
/// index time — so we compare lowercase token sequences.
#[test]
fn build_path_matches_query_path() {
    for input in [
        "Saint Kilda",
        "St Kilda",
        "Sainte Foy",
        "Mount Pleasant",
        "Mt Pleasant",
        "Fort Worth",
        "Main St",
        "St James Court",
        "Saint James Court",
        "Hampton St",
    ] {
        let query_side = normalise_prefix(input);
        let build_side_lc = canonicalise_phrase(input).to_ascii_lowercase();
        let q_tokens: Vec<&str> = query_side.split(' ').filter(|t| !t.is_empty()).collect();
        let b_tokens: Vec<&str> = build_side_lc.split_whitespace().collect();
        assert_eq!(
            q_tokens, b_tokens,
            "build vs query mismatch for {input:?}: \
             canonicalise_phrase → {build_side_lc:?} vs normalise_prefix → {query_side:?}"
        );
    }
}

/// Single-token inputs are returned unchanged. The position rule
/// requires at least two tokens to distinguish prefix from suffix.
#[test]
fn helper_passes_through_short_inputs() {
    assert_eq!(fold_place_abbreviations("st"), "st");
    assert_eq!(fold_place_abbreviations("saint"), "saint");
    assert_eq!(fold_place_abbreviations(""), "");
    assert_eq!(fold_place_abbreviations("mt"), "mt");
}

/// `tokenize_user_input` and `normalise_prefix` produce comparable
/// outputs once `tokenize_user_input` is joined with single spaces.
/// Both must drop the saint/mount/fort variants identically.
#[test]
fn tokenize_user_input_matches_normalise_prefix() {
    for input in [
        "Saint Kilda",
        "Mount Pleasant",
        "Fort Worth",
        "Main St",
        "St James Court",
    ] {
        let tokens = tokenize_user_input(input);
        let joined = tokens.join(" ");
        assert_eq!(
            joined,
            normalise_prefix(input),
            "tokenize_user_input vs normalise_prefix mismatch for {input:?}"
        );
    }
}

/// Place names whose canonical OSM form starts with an uppercase
/// diacritic (`Île-de-France`, `Östersund`, `Élysée`, `Ürümqi`,
/// `Ångström`) MUST normalise to pure ASCII. Pre-PR the build-side
/// fold table missed uppercase variants — `'Î'` fell through, then
/// `to_lowercase()` produced `'î'` (still non-ASCII), and the FST
/// key got UTF-8 bytes the runtime query path could never produce.
/// Result: title-cased diacritic places weren't reachable via FST
/// lookup. Pin the invariant here.
#[test]
fn fst_key_is_pure_ascii_for_uppercase_diacritic_names() {
    for input in [
        "Île-de-France",
        "Östersund",
        "Élysée",
        "Köln",
        "Ürümqi",
        "Ångström",
        "Ñuble",
        "Çatalhöyük",
    ] {
        let key = normalise_prefix(input);
        assert!(
            key.is_ascii(),
            "normalise_prefix({input:?}) returned non-ASCII bytes \
             {:?}; FST keys must be ASCII to match runtime lookups",
            key.as_bytes(),
        );
        // Spot-check the expected ASCII form for two well-known cases.
        if input == "Île-de-France" {
            assert_eq!(key, "ile de france");
        }
        if input == "Östersund" {
            assert_eq!(key, "ostersund");
        }
    }
}
