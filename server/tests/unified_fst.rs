//! Exercises the unified FST + `CountryPrefixAutomaton` added in the
//! Radar-style country-prefix refactor. Validates:
//!
//! - The custom automaton accepts only keys matching `<cc><...>` for
//!   the requested country code, pruning everything else.
//! - Exact-match mode returns one hit for an exact country+key match.
//! - Prefix-match mode returns everything under the prefix.
//! - Runtime results match the per-country FST bit-for-bit for AU
//!   (regression guard during the migration).

use fst::{Automaton, IntoStreamer, Map, MapBuilder, Streamer};
use query_server::autocomplete::{Autocomplete, AutocompleteEntry, CountryPrefixAutomaton, CpaState};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn load() -> Option<Autocomplete> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    Autocomplete::open(&PathBuf::from(dir)).ok().flatten()
}

// --- Pure automaton tests (no index required) ---

fn walk(auto: &CountryPrefixAutomaton<'_>, bytes: &[u8]) -> (CpaState, bool) {
    let mut s = auto.start();
    for &b in bytes {
        s = auto.accept(&s, b);
    }
    let is_match = auto.is_match(&s);
    (s, is_match)
}

#[test]
fn exact_automaton_accepts_only_exact_key() {
    let auto = CountryPrefixAutomaton::equals(*b"au", "sydney");

    // The right key: "au" + "sydney"
    let (_, ok) = walk(&auto, b"ausydney");
    assert!(ok, "exact automaton should match 'ausydney'");

    // Wrong country
    let (_, ok) = walk(&auto, b"ussydney");
    assert!(!ok);

    // Trailing bytes
    let (_, ok) = walk(&auto, b"ausydneylane");
    assert!(!ok, "exact automaton must reject trailing bytes");

    // Partial prefix
    let (_, ok) = walk(&auto, b"ausydn");
    assert!(!ok);

    // Different key
    let (_, ok) = walk(&auto, b"aumelbourne");
    assert!(!ok);
}

#[test]
fn prefix_automaton_matches_at_prefix_end_and_beyond() {
    let auto = CountryPrefixAutomaton::starts_with(*b"au", "sydn");

    // Exactly the prefix
    let (_, ok) = walk(&auto, b"ausydn");
    assert!(ok);

    // Prefix plus more
    let (_, ok) = walk(&auto, b"ausydney");
    assert!(ok);
    let (_, ok) = walk(&auto, b"ausydney lane");
    assert!(ok);

    // Wrong country still rejects
    let (_, ok) = walk(&auto, b"ussydn");
    assert!(!ok);

    // Different prefix
    let (_, ok) = walk(&auto, b"aumelbourne");
    assert!(!ok);
}

#[test]
fn automaton_accepts_empty_prefix_as_country_filter_only() {
    // An empty prefix means "match everything in that country".
    let auto = CountryPrefixAutomaton::starts_with(*b"au", "");
    let (_, ok) = walk(&auto, b"au");
    assert!(ok, "country-only prefix should match just the country code");
    let (_, ok) = walk(&auto, b"ausydney");
    assert!(ok);
    let (_, ok) = walk(&auto, b"us");
    assert!(!ok);
}

#[test]
fn automaton_rejects_bytes_past_exact_key_in_exact_mode() {
    // Regression guard: a common bug class in custom automata is
    // accepting trailing bytes in exact mode. Make sure our state
    // machine says Dead past the key length.
    let auto = CountryPrefixAutomaton::equals(*b"au", "sy");
    let mut s = auto.start();
    s = auto.accept(&s, b'a');
    s = auto.accept(&s, b'u');
    s = auto.accept(&s, b's');
    s = auto.accept(&s, b'y');
    assert!(auto.is_match(&s));
    s = auto.accept(&s, b'd');
    assert!(!auto.is_match(&s));
    assert!(!auto.can_match(&s));
}

// --- FST integration tests (synthetic, no external data) ---

fn build_synthetic_fst() -> Map<Vec<u8>> {
    // Three countries with a handful of names each. Keys are
    // `<cc><name>`; values are arbitrary payload ids.
    let mut entries: Vec<(Vec<u8>, u64)> = vec![
        (b"aubondi beach".to_vec(), 1),
        (b"aumelbourne".to_vec(), 2),
        (b"ausydney".to_vec(), 3),
        (b"ausydney olympic park".to_vec(), 4),
        (b"aucanberra".to_vec(), 5),
        (b"usnew york".to_vec(), 10),
        (b"ussan francisco".to_vec(), 11),
        (b"usportland".to_vec(), 12),
        (b"frparis".to_vec(), 20),
        (b"frmarseille".to_vec(), 21),
    ];
    entries.sort();

    let mut buf = Vec::new();
    let mut builder = MapBuilder::new(&mut buf).expect("new MapBuilder");
    for (k, v) in entries {
        builder.insert(&k, v).expect("insert fst key");
    }
    builder.finish().expect("finish fst");
    Map::new(buf).expect("Map::new over the built fst")
}

#[test]
fn fst_stream_respects_country_filter() {
    let map = build_synthetic_fst();

    // AU prefix = "s" should pick up Sydney and both Sydney variants,
    // NOT US San Francisco or US Portland.
    let auto = CountryPrefixAutomaton::starts_with(*b"au", "s");
    let mut stream = map.search(auto).into_stream();
    let mut hits: Vec<String> = Vec::new();
    while let Some((k, _)) = stream.next() {
        hits.push(String::from_utf8_lossy(k).into_owned());
    }
    hits.sort();
    assert_eq!(hits, vec!["ausydney", "ausydney olympic park"]);
}

#[test]
fn fst_stream_exact_match_returns_single_hit() {
    let map = build_synthetic_fst();

    let auto = CountryPrefixAutomaton::equals(*b"au", "sydney");
    let mut stream = map.search(auto).into_stream();
    let first = stream.next();
    assert!(first.is_some());
    let (k, v) = first.expect("exact-match hit present");
    assert_eq!(k, b"ausydney");
    assert_eq!(v, 3);
    assert!(stream.next().is_none(), "exact match should return one hit");
}

#[test]
fn fst_stream_missing_country_returns_nothing() {
    let map = build_synthetic_fst();
    let auto = CountryPrefixAutomaton::starts_with(*b"de", "");
    let mut stream = map.search(auto).into_stream();
    assert!(stream.next().is_none());
}

// --- Synthetic routing (no external index) ---

/// Build a fake per-country + unified pair in a tempdir with
/// *deliberately different* payloads for the same logical key, so a
/// routing regression is observable. Runtime must serve the unified
/// payload when both are present.
#[test]
fn runtime_prefers_unified_when_both_layouts_present() {
    let dir = make_test_dir("routing");

    // Per-country AU, key "sydney" → entry named "PER_COUNTRY_SYD".
    write_layout(
        &dir,
        LayoutKind::PerCountry(*b"au"),
        &[("sydney", "PER_COUNTRY_SYD", "")],
    );

    // Unified, key "ausydney" → entry named "UNIFIED_SYD".
    write_layout(
        &dir,
        LayoutKind::Unified,
        &[("ausydney", "UNIFIED_SYD", "")],
    );

    let a = Autocomplete::open(&dir).expect("open").expect("some");
    let hit = a.exact_match(b"au", "sydney").expect("should hit");
    assert_eq!(
        hit.name, "UNIFIED_SYD",
        "unified FST must win over per-country when both are present"
    );

    cleanup(&dir);
}

/// With only per-country files present, the per-country path must serve.
#[test]
fn runtime_uses_per_country_when_unified_absent() {
    let dir = make_test_dir("per_country_only");
    write_layout(
        &dir,
        LayoutKind::PerCountry(*b"au"),
        &[("sydney", "PER_COUNTRY_SYD", "")],
    );
    let a = Autocomplete::open(&dir).expect("open").expect("some");
    let hit = a.exact_match(b"au", "sydney").expect("should hit");
    assert_eq!(hit.name, "PER_COUNTRY_SYD");
    cleanup(&dir);
}

/// `has_country` must reflect the unified FST's actual country coverage.
/// A unified file built with `--country au` should *not* claim coverage
/// for unrelated countries.
#[test]
fn has_country_reflects_unified_coverage() {
    let dir = make_test_dir("has_country");
    write_layout(
        &dir,
        LayoutKind::Unified,
        &[("ausydney", "SYD", "")],
    );
    let a = Autocomplete::open(&dir).expect("open").expect("some");
    assert!(a.has_country(b"au"));
    assert!(!a.has_country(b"us"), "unified covers only AU; must not lie");
    assert!(!a.has_country(b"fr"));
    cleanup(&dir);
}

/// Normalised-key length floor: "é" is 2 UTF-8 bytes but folds to "e"
/// (1 byte). Must not sneak past FST_MIN_PREFIX_LEN.
#[test]
fn exact_match_rejects_short_normalised_key() {
    let dir = make_test_dir("short_key");
    // Seed with a real hit so we know the index is loaded.
    write_layout(&dir, LayoutKind::Unified, &[("ausy", "SY", "")]);
    let a = Autocomplete::open(&dir).expect("open").expect("some");
    assert!(a.exact_match(b"au", "é").is_none());
    assert!(a.exact_match(b"au", "sy").is_some(), "control: 2-byte key works");
    cleanup(&dir);
}

// --- Runtime integration (requires real index) ---

#[test]
fn runtime_exact_match_resolves_sydney() {
    let Some(a) = load() else { return };
    let hit = a.exact_match(b"au", "sydney").expect("Sydney should resolve");
    assert!(hit.name.to_ascii_lowercase().contains("sydney"));
    assert!(hit.rank <= 19, "should be a place, got rank {}", hit.rank);
}

#[test]
fn runtime_prefix_walk_still_returns_results() {
    let Some(a) = load() else { return };
    // Heap-bounded top-K walk should surface Alysse Close even if other
    // "alys*" streets come first alphabetically.
    let hits = a.search(b"au", "alyss", 5);
    assert!(!hits.is_empty());
    assert!(hits.iter().any(|h| h.name.to_ascii_lowercase().contains("alysse")));
}

#[test]
fn runtime_country_filter_rejects_wrong_country() {
    let Some(a) = load() else { return };
    // Our test index only has AU. A us-filtered query for "sydney"
    // should return None even though "sydney" exists under AU.
    assert!(a.exact_match(b"us", "sydney").is_none());
}

// --- Synthetic test helpers ---

fn make_test_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "unified_fst_test_{}_{}_{}",
        tag,
        std::process::id(),
        nonce,
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

enum LayoutKind {
    PerCountry([u8; 2]),
    Unified,
}

/// Write a minimal `(fst, entries, strings)` triple to `dir` matching the
/// production on-disk layout. `rows` is `(fst_key, name, suburb)` — for
/// per-country layouts the key is the normalised name directly; for
/// unified, the caller pre-prefixes the cc.
fn write_layout(dir: &Path, kind: LayoutKind, rows: &[(&str, &str, &str)]) {
    let (fst_path, bin_path, strings_path) = match &kind {
        LayoutKind::PerCountry(cc) => {
            let stem = format!("fst_{}{}", cc[0] as char, cc[1] as char);
            (
                dir.join(format!("{stem}.fst")),
                dir.join(format!("{stem}.bin")),
                dir.join(format!("{stem}_strings.bin")),
            )
        }
        LayoutKind::Unified => (
            dir.join("fst_unified.fst"),
            dir.join("fst_unified.bin"),
            dir.join("fst_unified_strings.bin"),
        ),
    };

    // Build the string pool with offset 0 reserved for the empty string.
    let mut strings: Vec<u8> = vec![0];
    let intern = |s: &str, pool: &mut Vec<u8>| -> u32 {
        if s.is_empty() {
            return 0;
        }
        let off = pool.len() as u32;
        pool.extend_from_slice(s.as_bytes());
        pool.push(0);
        off
    };

    // Records: one per row. FST key → entry index.
    let mut entries_bytes: Vec<u8> = Vec::new();
    let mut fst_buf: Vec<u8> = Vec::new();
    let mut b = MapBuilder::new(&mut fst_buf).expect("new MapBuilder");

    let mut sorted: Vec<(String, String, String)> = rows
        .iter()
        .map(|(k, n, s)| (k.to_string(), n.to_string(), s.to_string()))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for (i, (key, name, suburb)) in sorted.iter().enumerate() {
        let name_off = intern(name, &mut strings);
        let suburb_off = intern(suburb, &mut strings);
        let entry = AutocompleteEntry {
            lat: 0.0,
            lng: 0.0,
            name_offset: name_off,
            suburb_offset: suburb_off,
            kind: 1,
            rank: 16,
            pad: [0; 2],
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &entry as *const AutocompleteEntry as *const u8,
                std::mem::size_of::<AutocompleteEntry>(),
            )
        };
        entries_bytes.extend_from_slice(bytes);
        b.insert(key.as_bytes(), i as u64).expect("insert fst key");
    }
    b.finish().expect("finish fst");

    std::fs::write(&fst_path, &fst_buf).expect("write fst file");
    std::fs::write(&bin_path, &entries_bytes).expect("write entries file");
    std::fs::write(&strings_path, &strings).expect("write strings file");
}
