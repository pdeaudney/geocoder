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
    let Some(q) = parse_hn(query) else {
        return false;
    };
    let Some(s) = parse_hn(stored) else {
        return false;
    };

    // Suffix discipline. When the query carries an explicit suffix
    // letter (`1327A`), the user is naming a specific cadastral parcel
    // (often a duplex unit). Returning a sibling-suffix record
    // (`1327B`) is wrong — they're distinct titles that share the
    // building number only.
    //
    //   q has suffix, s has suffix          → must be the same suffix
    //   q has suffix, s has no suffix       → only match if `s` is a
    //                                          true range (1320-1340)
    //                                          containing q's digit
    //   q has no suffix                     → no constraint (preserves
    //                                          the documented permissive
    //                                          behaviour: "42" → "42A")
    if !q.suffix.is_empty() {
        if !s.suffix.is_empty() {
            if !q.suffix.eq_ignore_ascii_case(&s.suffix) {
                return false;
            }
        } else if s.first == s.last {
            // Stored is a single number with no suffix; query has
            // suffix → distinct parcel.
            return false;
        }
    }

    ranges_overlap((q.first, q.last), (s.first, s.last))
}

/// Detect the AU shorthand `<unit>/<housenumber>` (e.g. `3/827a`,
/// `12/45`). Returns `Some((unit, hn))` when the input matches.
/// Lets callers pull the unit out into its own slot rather than
/// stuffing it into the housenumber field.
///
/// Rules:
///   - exactly one `/` separator
///   - LHS is non-empty and all ASCII digits (the unit number)
///   - RHS is non-empty and starts with an ASCII digit (the building
///     number, possibly with a trailing suffix letter)
///   - whitespace around either side is trimmed
///
/// Examples:
///   `"3/827a"`     → `Some(("3", "827a"))`
///   `"12 / 45"`    → `Some(("12", "45"))`
///   `"12-14"`      → `None`  (range, not unit/hn)
///   `"827a"`       → `None`  (no slash)
///   `"Apt 3/45"`   → `None`  (LHS not all digits — let the OSM
///                              normaliser's word-prefix path handle it)
pub fn parse_au_unit_address(s: &str) -> Option<(&str, &str)> {
    let trimmed = s.trim();
    let (lhs, rhs) = trimmed.split_once('/')?;
    let lhs = lhs.trim();
    let rhs = rhs.trim();
    if lhs.is_empty() || rhs.is_empty() {
        return None;
    }
    if !lhs.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !rhs.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((lhs, rhs))
}

#[derive(Debug, Clone)]
struct ParsedHn {
    /// Lower bound of the housenumber range (the only digit value
    /// for non-range inputs).
    first: u32,
    /// Upper bound of the range, equal to `first` for single numbers.
    last: u32,
    /// Suffix letter(s) on the housenumber (`"A"` for `1327A`,
    /// `""` for plain `1327`). For ranges with suffixes on each end
    /// (rare — `1320A-1340A`), we keep the suffix from the FIRST
    /// number; both ends having identical suffixes is the conventional
    /// shape and the matcher's suffix discipline only needs to
    /// distinguish present vs absent + which letter.
    suffix: String,
}

/// Parse a housenumber string into its numeric range and suffix.
/// Returns `None` when the string has no leading-digit component
/// (`"Lot 5"`, `"PO Box 12"`).
fn parse_hn(hn: &str) -> Option<ParsedHn> {
    let trimmed = hn.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((first, last)) = trimmed.split_once('-') {
        let (f, fs) = parse_digits_and_suffix(first.trim())?;
        let (l, _ls) = parse_digits_and_suffix(last.trim())?;
        Some(ParsedHn {
            first: f.min(l),
            last: f.max(l),
            suffix: fs.to_string(),
        })
    } else {
        let (n, suffix) = parse_digits_and_suffix(trimmed)?;
        Some(ParsedHn {
            first: n,
            last: n,
            suffix: suffix.to_string(),
        })
    }
}

/// Read a base-10 unsigned integer from the leading characters of
/// `s` and return it together with the alpha suffix that follows.
/// Returns `None` when `s` doesn't start with a digit.
///
///   `"1327A"` → `Some((1327, "A"))`
///   `"42"`    → `Some((42, ""))`
///   `"Lot 5"` → `None`
fn parse_digits_and_suffix(s: &str) -> Option<(u32, &str)> {
    let s = s.trim_start();
    let mut digits_end = 0usize;
    let mut acc: u32 = 0;
    for (i, ch) in s.char_indices() {
        if let Some(d) = ch.to_digit(10) {
            acc = acc.checked_mul(10)?.checked_add(d)?;
            digits_end = i + ch.len_utf8();
        } else {
            break;
        }
    }
    if digits_end == 0 {
        return None;
    }
    let suffix = s[digits_end..].trim();
    Some((acc, suffix))
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
    fn suffix_discipline() {
        // Bare number query → still matches suffixed stored. User
        // typed only the building number; the suffix is sub-address
        // detail they didn't specify, so any sibling suffix is a
        // reasonable hit (geographic-nearest tiebreak picks one).
        assert!(housenumber_matches("42", "42A"));
        assert!(housenumber_matches("42", "42B"));

        // Suffixed query against suffixed stored: only the same
        // suffix matches. Distinct cadastral parcels in G-NAF (often
        // separate duplex units) — returning the wrong one is wrong.
        assert!(housenumber_matches("1327A", "1327A"));     // exact
        assert!(housenumber_matches("1327a", "1327A"));     // case-insensitive exact
        assert!(!housenumber_matches("1327A", "1327B"));    // suffix mismatch
        assert!(!housenumber_matches("1327B", "1327A"));    // symmetric

        // Suffixed query against bare-number stored: no match.
        // 42 and 42A are typically distinct G-NAF parcels (the original
        // house got subdivided into 42 + 42A); returning 42 when the
        // user asked for 42A is wrong.
        assert!(!housenumber_matches("42A", "42"));

        // Suffixed query against a TRUE range: match if the digit
        // falls inside the range. The range form denotes a span of
        // building numbers regardless of any unit-letter quirks
        // ("827a inside 820-830" is a reasonable building-level hit).
        assert!(housenumber_matches("827a", "820-830"));
        assert!(housenumber_matches("827a", "827-830"));
        assert!(!housenumber_matches("827a", "820-826")); // out of range

        // Both sides suffixed AND the range is annotated with the
        // same suffix on both ends (rare but seen): still matches
        // when the digit is in range.
        assert!(housenumber_matches("1327A", "1320A-1340A"));
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
    fn parse_overflow_returns_none() {
        // Beyond u32::MAX — checked arithmetic returns None rather
        // than wrapping. A 12-digit "housenumber" from corrupt input
        // shouldn't crash the matcher.
        assert!(parse_hn("999999999999").is_none());
    }

    #[test]
    fn parse_au_unit_address_basic() {
        assert_eq!(parse_au_unit_address("3/827a"), Some(("3", "827a")));
        assert_eq!(parse_au_unit_address("12/45"), Some(("12", "45")));
        assert_eq!(parse_au_unit_address("12 / 45"), Some(("12", "45")));
        assert_eq!(parse_au_unit_address(" 3 / 827a "), Some(("3", "827a")));
    }

    #[test]
    fn parse_au_unit_address_rejects_non_pattern() {
        // Word prefix → not the AU shorthand (let OSM word-prefix
        // normaliser handle "Apt 3/45").
        assert_eq!(parse_au_unit_address("Apt 3/45"), None);
        // Range, not unit/hn.
        assert_eq!(parse_au_unit_address("12-14"), None);
        // No slash.
        assert_eq!(parse_au_unit_address("827a"), None);
        // RHS doesn't start with a digit.
        assert_eq!(parse_au_unit_address("3/A45"), None);
        // Missing side.
        assert_eq!(parse_au_unit_address("/45"), None);
        assert_eq!(parse_au_unit_address("3/"), None);
    }
}
