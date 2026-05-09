//! Range-aware housenumber matching.
//!
//! Address-point datasets store house numbers in a mix of forms:
//! - **Single**: `"42"`, `"42A"`, `"Lot 5"`
//! - **Range**:  `"255-257"` (G-NAF NUMBER_FIRST + NUMBER_LAST joined),
//!               `"10-12"` (OSM `addr:housenumber=10-12`)
//!
//! The G-NAF builder at `server/src/bin/build_gnaf_index.rs:347-363`
//! emits hyphenated ranges as a single record. The OSM area handler
//! preserves whatever the OSM tag says, hyphens included. Both of
//! these flow into the same `eq_ignore_ascii_case` matcher today,
//! which means a query for `"256"` against a stored `"255-257"`
//! returns nothing.
//!
//! Radar's HorizonDB blog noted: *"None of our data storage
//! technologies supported ranges of addresses, so we knew we were
//! going to need to build new infrastructure."* They built ranges as
//! a first-class concept. We don't have a separate range index, but
//! we can fix the matcher: a query for any number within a stored
//! range should hit, and a stored single inside a queried range
//! should hit too.
//!
//! ## Match rules
//!
//! Given a query `Q` and a stored `S`:
//!
//! 1. Case-insensitive exact equality always matches (preserves the
//!    historical behaviour for queries like `"42A"` ↔ `"42a"`).
//! 2. Otherwise, parse both into `(first, last)` numeric ranges:
//!    - `"42"` → `(42, 42)`
//!    - `"255-257"` → `(255, 257)`
//!    - `"42A"` → `(42, 42)` (suffix letter ignored — the building
//!      number is 42)
//!    - `"Lot 5"` / non-numeric prefix → unparseable (no range match
//!      possible; only exact equality would have hit)
//! 3. Match if the two ranges overlap, i.e.
//!    `max(q.first, s.first) <= min(q.last, s.last)`.
//!
//! ## Side-effects to be aware of
//!
//! - `"42"` query → matches stored `"42A"` and `"42B"` records.
//!   Previously these would not match. The new behaviour is more
//!   permissive — a user who types just the number gets the closest
//!   suffix-letter record back, which is what the geographically-
//!   nearest tiebreak in the calling matcher would have returned
//!   anyway if interpolation had filled the gap. Acceptable.
//! - Reversed input ranges (`"257-255"`) are normalised by sorting
//!   before the overlap check. Defensive — G-NAF and OSM never emit
//!   them but freeform queries might.
//! - Ranges with non-numeric components (`"A-1 to A-5"`) are not
//!   parsed — they fall through to exact match only. Edge case;
//!   would need a separate alpha-aware sequencer.

/// Match a query housenumber string against a stored housenumber
/// string with range-aware semantics. See module docs for rules.
pub fn housenumber_matches(query: &str, stored: &str) -> bool {
    if query.eq_ignore_ascii_case(stored) {
        return true;
    }
    let Some(q) = parse_range(query) else {
        return false;
    };
    let Some(s) = parse_range(stored) else {
        return false;
    };
    ranges_overlap(q, s)
}

/// Parse a housenumber string into a `(first, last)` numeric range.
/// Returns `None` when the string has no leading-digit component
/// (e.g. `"Lot 5"`, `"PO Box 12"`).
fn parse_range(hn: &str) -> Option<(u32, u32)> {
    let trimmed = hn.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((first, last)) = trimmed.split_once('-') {
        let f = parse_leading_digits(first)?;
        let l = parse_leading_digits(last)?;
        // Normalise reversed inputs so the overlap check is order-
        // independent.
        Some((f.min(l), f.max(l)))
    } else {
        let v = parse_leading_digits(trimmed)?;
        Some((v, v))
    }
}

/// Read a base-10 unsigned integer from the leading characters of
/// `s`, ignoring any trailing alpha suffix or whitespace. Returns
/// `None` when `s` doesn't start with a digit.
fn parse_leading_digits(s: &str) -> Option<u32> {
    let s = s.trim_start();
    let mut acc: u32 = 0;
    let mut any = false;
    for ch in s.chars() {
        if let Some(d) = ch.to_digit(10) {
            acc = acc.checked_mul(10)?.checked_add(d)?;
            any = true;
        } else {
            break;
        }
    }
    if any {
        Some(acc)
    } else {
        None
    }
}

#[inline]
fn ranges_overlap(q: (u32, u32), s: (u32, u32)) -> bool {
    q.0.max(s.0) <= q.1.min(s.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_preserved() {
        assert!(housenumber_matches("42", "42"));
        assert!(housenumber_matches("42A", "42a")); // case-insensitive
        assert!(housenumber_matches("Lot 5", "lot 5"));
    }

    #[test]
    fn query_in_stored_range() {
        // Real Castle Hill case from this conversation.
        assert!(housenumber_matches("256", "255-257"));
        assert!(housenumber_matches("255", "255-257"));
        assert!(housenumber_matches("257", "255-257"));
    }

    #[test]
    fn query_outside_stored_range_does_not_match() {
        assert!(!housenumber_matches("258", "255-257"));
        assert!(!housenumber_matches("254", "255-257"));
    }

    #[test]
    fn stored_in_query_range() {
        // User asked for 255-257 even though G-NAF only has 255.
        assert!(housenumber_matches("255-257", "255"));
        assert!(housenumber_matches("255-257", "256"));
        assert!(housenumber_matches("255-257", "257"));
    }

    #[test]
    fn overlapping_ranges_match() {
        assert!(housenumber_matches("254-256", "255-257"));
        assert!(housenumber_matches("253-300", "255-257"));
        assert!(housenumber_matches("255-257", "256-260"));
    }

    #[test]
    fn disjoint_ranges_do_not_match() {
        assert!(!housenumber_matches("250-254", "255-257"));
        assert!(!housenumber_matches("258-260", "255-257"));
    }

    #[test]
    fn suffix_letter_is_ignored_for_range_match() {
        // "42" query matches "42A" stored — building number is the
        // same. Documented as an intentional permissiveness in the
        // module docs.
        assert!(housenumber_matches("42", "42A"));
        assert!(housenumber_matches("42A", "42"));
        // But "42B" stored vs "42A" query: parse_range gives (42,42)
        // for both, so they overlap. Edge case — exact match was
        // false (different suffixes), but range match is true.
        assert!(housenumber_matches("42A", "42B"));
    }

    #[test]
    fn non_numeric_falls_through_to_exact_only() {
        // "Lot 5" has no leading digits — parse_range returns None,
        // so only exact match would apply.
        assert!(!housenumber_matches("5", "Lot 5"));
        assert!(!housenumber_matches("Lot 5", "5"));
        // But exact match still works.
        assert!(housenumber_matches("Lot 5", "lot 5"));
    }

    #[test]
    fn reversed_range_input_normalises() {
        // No real dataset emits "257-255" but freeform queries might.
        assert!(housenumber_matches("256", "257-255"));
        assert!(housenumber_matches("257-255", "256"));
    }

    #[test]
    fn empty_inputs() {
        assert!(housenumber_matches("", "")); // trivially eq
        assert!(!housenumber_matches("42", ""));
        assert!(!housenumber_matches("", "42"));
    }

    #[test]
    fn parse_range_overflow_returns_none() {
        // Beyond u32::MAX — checked arithmetic returns None rather
        // than wrapping. A 12-digit "housenumber" from corrupt input
        // shouldn't crash the matcher.
        assert_eq!(parse_range("999999999999"), None);
    }
}
