//! Per-field length caps for user-supplied text inputs.
//!
//! Both the REST and gRPC surfaces enforce these — drift between the
//! two would let one front-end accept inputs the other rejects.
//! Constants live here, both layers wrap [`check`] in their layer-
//! specific error type (`Response` for REST, `tonic::Status` for
//! gRPC).
//!
//! # Why these specific values
//!
//! Anchored against real-user input observation, not pulled from
//! thin air:
//!
//! - **Search `q` — 512 chars.** Pelias caps at ~256; we double it
//!   to leave headroom for freeform queries that bundle POI hints
//!   or multi-locality context. Above this, the BooleanQuery
//!   construction in tantivy starts producing too many SHOULD
//!   clauses for BM25 to score meaningfully — the request still
//!   succeeds but the result quality degrades faster than the
//!   compute cost grows.
//! - **Autocomplete `q` — 128 chars.** Typeahead is short by
//!   definition; real users very rarely type more than ~50 chars
//!   before they hit Enter. The FST walk is already capped at
//!   10 000 keys, but the prefix bytes still get copied into the
//!   automaton state.
//! - **Structured fields (`street`, `city`, `state`) — 256 chars.**
//!   Single-field max for any structured component. OSM canonical
//!   names are <100 chars even for the longest cities/streets.
//! - **`postcode` — 16 chars.** Longest real postcode systems
//!   (GB `SW1A 1AA`, BR with hyphen) sit ≤10. 16 is generous
//!   buffer.
//! - **`housenumber` — 32 chars.** Most are 1–10 chars; some carry
//!   suffixes/fractions (`123A`, `12-1/2`); cap at 32.
//! - **`country_code` — 2 chars.** Hard ISO-3166-1 alpha-2
//!   constraint; longer values are malformed by definition.
//! - **`lang` — 16 chars.** BCP-47 with subtags can reach
//!   `zh-Hant-TW` (10) and similar; 16 covers everything we'd see.

/// Max length for `q` on `/search` (freeform full-address queries).
pub const SEARCH_Q: usize = 512;

/// Max length for `q` on `/autocomplete` (typeahead prefixes).
pub const AUTOCOMPLETE_Q: usize = 128;

/// Max length for any single structured-query text field
/// (`street`, `city`, `state`).
pub const STRUCTURED_FIELD: usize = 256;

/// Max length for `postcode` parameters.
pub const POSTCODE: usize = 16;

/// Max length for `housenumber` parameters.
pub const HOUSENUMBER: usize = 32;

/// Max length for ISO-3166-1 alpha-2 country codes (always exactly 2,
/// so this is also the upper bound — anything longer is malformed).
pub const COUNTRY_CODE: usize = 2;

/// Max length for `/search?country_code=` when used with the Radar-
/// style comma-separated multi-country filter (`US,CA,MX,…`). Each
/// alpha-2 + comma is 3 bytes; 64 covers ~21 codes which is more
/// than any real-world deployment serves at once.
pub const COUNTRY_CODE_LIST: usize = 64;

/// Max length for BCP-47 language tags (`en`, `en-US`, `zh-Hant-TW`,
/// …).
pub const LANG: usize = 16;

/// Max length for an IPv4 / IPv6 textual address. IPv6 with zone id
/// (`fe80::1%eth0`) is the longest realistic form; cap at 64 covers
/// every edge case including embedded-IPv4 forms.
pub const IP: usize = 64;

/// Validate that `value` is at most `max` chars long. Length is
/// measured in bytes (UTF-8) — a multi-byte char counts as more
/// than one. That's intentional: the caps protect against
/// memory/CPU abuse, and bytes are the unit downstream tokenisers
/// see.
///
/// Returns the formatted error message on overflow so the calling
/// layer can wrap it in its own response type.
pub fn check(name: &str, value: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        return Err(format!(
            "{name}: input is {} bytes; max allowed is {}",
            value.len(),
            max,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_passes_within_cap() {
        assert!(check("q", "sydney", 64).is_ok());
        assert!(check("q", "", 64).is_ok());
        assert!(check("q", &"a".repeat(64), 64).is_ok());
    }

    #[test]
    fn check_rejects_over_cap() {
        let err = check("q", &"a".repeat(65), 64).unwrap_err();
        assert!(err.contains("q"));
        assert!(err.contains("65"));
        assert!(err.contains("64"));
    }

    #[test]
    fn check_uses_byte_length_not_char_length() {
        // "é" is 2 UTF-8 bytes. 5 of them = 10 bytes.
        let s = "é".repeat(5);
        assert_eq!(s.len(), 10);
        assert!(check("q", &s, 9).is_err());
        assert!(check("q", &s, 10).is_ok());
    }
}
