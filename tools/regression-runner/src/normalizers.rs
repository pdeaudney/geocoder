//! Pelias acceptance-test field normalisers.
//!
//! Ported from `lib/normalizers.js` in pelias/fuzzy-tester (the runner
//! that pelias/acceptance-tests uses), at commit
//! e97e7fee67afa4ea6f54f6de4da8560864e73355. `lib/eval_test.js` there
//! applies a field's list left to right to the expected and the actual
//! value before an exact comparison.
//!
//! The JS regexes have no `u` flag, so `\w`, `\d`, `\b` and the `i`
//! flag are ASCII-only. `\s` and `String.prototype.trim` use the
//! ECMAScript whitespace set. The ports below keep those rules.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Normalizer {
    Identity,
    StripPunctuation,
    ToLowerCase,
    ToUpperCase,
    Trim,
    AbbreviateDirectionals,
    AbbreviateStreetSuffixes,
    RemoveOrdinals,
    RemoveNumbers,
    AbbreviateMountSaintFort,
}

const DIRECTIONALS: &[(&str, &str)] = &[
    ("NORTH", "N"),
    ("SOUTH", "S"),
    ("EAST", "E"),
    ("WEST", "W"),
    ("NORTHEAST", "NE"),
    ("NORTHWEST", "NW"),
    ("SOUTHEAST", "SE"),
    ("SOUTHWEST", "SW"),
];

const STREET_SUFFIXES: &[(&str, &str)] = &[
    ("AVE", "AV"),
    ("AVENUE", "AV"),
    ("BOULEVARD", "BLVD"),
    ("CIRCLE", "CIR"),
    ("COURT", "CT"),
    ("COVE", "CV"),
    ("DRIVE", "DR"),
    ("HIGHWAY", "HWY"),
    ("JUNIOR", "JR"),
    ("LANE", "LN"),
    ("LOOP", "LP"),
    ("PARKWAY", "PKWY"),
    ("PIKE", "PKE"),
    ("PLACE", "PL"),
    ("ROAD", "RD"),
    ("STREET", "ST"),
    ("TERR", "TER"),
    ("TERRACE", "TER"),
    ("TRACE", "TRCE"),
    ("WAY", "WY"),
];

const MOUNT_SAINT_FORT: &[(&str, &str)] = &[("MOUNT", "Mt"), ("SAINT", "St"), ("FORT", "Ft")];

impl Normalizer {
    pub fn apply(self, value: &str) -> String {
        match self {
            Self::Identity => value.to_string(),
            Self::StripPunctuation => value
                .chars()
                .filter(|&c| is_js_word(c) || is_js_whitespace(c))
                .collect(),
            Self::ToLowerCase => value.to_lowercase(),
            Self::ToUpperCase => value.to_uppercase(),
            Self::Trim => value.trim_matches(is_js_whitespace).to_string(),
            Self::AbbreviateDirectionals => replace_terms(value, DIRECTIONALS),
            Self::AbbreviateStreetSuffixes => replace_terms(value, STREET_SUFFIXES),
            Self::RemoveOrdinals => remove_ordinals(value),
            Self::RemoveNumbers => value
                .chars()
                .filter(|c| !c.is_ascii_digit())
                .collect::<String>()
                .trim_matches(is_js_whitespace)
                .to_string(),
            Self::AbbreviateMountSaintFort => replace_terms(value, MOUNT_SAINT_FORT),
        }
    }
}

pub fn apply_all(normalizers: &[Normalizer], value: &str) -> String {
    normalizers
        .iter()
        .fold(value.to_string(), |acc, n| n.apply(&acc))
}

fn is_js_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// ECMAScript WhiteSpace + LineTerminator. Differs from Rust's
/// `char::is_whitespace` only on U+0085 (not JS) and U+FEFF (JS).
fn is_js_whitespace(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

/// JS `replaceTerms`: for each term in order, replace every
/// `\bTERM\b` match, ASCII case-insensitive.
fn replace_terms(value: &str, terms: &[(&str, &str)]) -> String {
    terms
        .iter()
        .fold(value.to_string(), |acc, (term, replacement)| {
            let bytes = acc.as_bytes();
            let mut out = String::with_capacity(acc.len());
            let mut copied = 0;
            let mut i = 0;
            while i + term.len() <= bytes.len() {
                let end = i + term.len();
                let at_start = i == 0 || !is_js_word(bytes[i - 1] as char);
                let at_end = end == bytes.len() || !is_js_word(bytes[end] as char);
                if at_start && at_end && bytes[i..end].eq_ignore_ascii_case(term.as_bytes()) {
                    out.push_str(&acc[copied..i]);
                    out.push_str(replacement);
                    copied = end;
                    i = end;
                } else {
                    i += 1;
                }
            }
            out.push_str(&acc[copied..]);
            out
        })
}

/// JS `/((\d+)(st|nd|th|rd))/ig` → `$2`: drop an ordinal suffix that
/// directly follows a run of ASCII digits. No word boundary applies.
fn remove_ordinals(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let suffix = bytes.get(i..i + 2);
        if suffix.is_some_and(|s| {
            ["st", "nd", "th", "rd"]
                .iter()
                .any(|o| s.eq_ignore_ascii_case(o.as_bytes()))
        }) {
            out.push_str(&value[copied..i]);
            copied = i + 2;
            i += 2;
        }
    }
    out.push_str(&value[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Normalizer::*;

    #[test]
    fn names_match_pelias() {
        let names = [
            "identity",
            "stripPunctuation",
            "toLowerCase",
            "toUpperCase",
            "trim",
            "abbreviateDirectionals",
            "abbreviateStreetSuffixes",
            "removeOrdinals",
            "removeNumbers",
            "abbreviateMountSaintFort",
        ];
        for name in names {
            serde_json::from_value::<Normalizer>(serde_json::json!(name))
                .expect("Pelias normaliser name deserialises");
        }
        assert!(serde_json::from_value::<Normalizer>(serde_json::json!("lowercase")).is_err());
    }

    #[test]
    fn identity_keeps_value() {
        assert_eq!(Identity.apply(" A-b "), " A-b ");
    }

    #[test]
    fn strip_punctuation_keeps_ascii_word_and_js_space() {
        assert_eq!(StripPunctuation.apply("St. John's, NW_1"), "St Johns NW_1");
        // JS `\w` is ASCII-only without the `u` flag, so accents go too.
        assert_eq!(StripPunctuation.apply("Café\u{a0}Rue"), "Caf\u{a0}Rue");
        assert_eq!(StripPunctuation.apply("a\u{85}b\u{feff}"), "ab\u{feff}");
    }

    #[test]
    fn case_mapping() {
        assert_eq!(ToLowerCase.apply("Straße ÉTÉ"), "straße été");
        assert_eq!(ToUpperCase.apply("Straße été"), "STRASSE ÉTÉ");
    }

    #[test]
    fn trim_uses_js_whitespace() {
        assert_eq!(Trim.apply("\u{feff}\t Main St \u{3000}"), "Main St");
        assert_eq!(Trim.apply("\u{85}x"), "\u{85}x");
    }

    #[test]
    fn abbreviate_directionals_whole_words_case_insensitive() {
        assert_eq!(
            AbbreviateDirectionals.apply("north Main St Southwest"),
            "N Main St SW"
        );
        assert_eq!(
            AbbreviateDirectionals.apply("Northern Westway"),
            "Northern Westway"
        );
        assert_eq!(AbbreviateDirectionals.apply("EAST-WEST"), "E-W");
    }

    #[test]
    fn abbreviate_street_suffixes_in_pelias_order() {
        assert_eq!(AbbreviateStreetSuffixes.apply("16TH STREET"), "16TH ST");
        assert_eq!(AbbreviateStreetSuffixes.apply("Fifth Avenue"), "Fifth AV");
        assert_eq!(AbbreviateStreetSuffixes.apply("Park Ave"), "Park AV");
        assert_eq!(AbbreviateStreetSuffixes.apply("Terrace Way"), "TER WY");
        assert_eq!(
            AbbreviateStreetSuffixes.apply("Streetly Road"),
            "Streetly RD"
        );
    }

    #[test]
    fn remove_ordinals_without_word_boundary() {
        assert_eq!(RemoveOrdinals.apply("West 26th Street"), "West 26 Street");
        assert_eq!(RemoveOrdinals.apply("1ST 2nd 3Rd 4th"), "1 2 3 4");
        assert_eq!(RemoveOrdinals.apply("21stCentury 1st2nd"), "21Century 12");
        assert_eq!(RemoveOrdinals.apply("Strand 5 th"), "Strand 5 th");
    }

    #[test]
    fn remove_numbers_then_trim() {
        assert_eq!(RemoveNumbers.apply(" 7-Eleven 42 "), "-Eleven");
    }

    #[test]
    fn abbreviate_mount_saint_fort() {
        assert_eq!(
            AbbreviateMountSaintFort.apply("MOUNT Pleasant, saint Paul, Fort Worth, Fortune"),
            "Mt Pleasant, St Paul, Ft Worth, Fortune"
        );
    }

    #[test]
    fn apply_all_runs_left_to_right() {
        let chain = [ToUpperCase, AbbreviateDirectionals, ToLowerCase];
        assert_eq!(apply_all(&chain, "North 1st Street"), "n 1st street");
        assert_eq!(
            apply_all(&[ToUpperCase, AbbreviateStreetSuffixes], "3122 16th Street"),
            "3122 16TH ST"
        );
        assert_eq!(apply_all(&[], "x"), "x");
    }
}
