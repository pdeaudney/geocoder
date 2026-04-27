//! Exercises the FST exact-match fast-path used by `/search`. Validates
//! that a simple freeform query matching an FST key resolves in ~µs-land
//! via the FST, while structured and non-matching queries still go
//! through tantivy.

#![cfg(feature = "forward")]

use query_server::autocomplete::Autocomplete;
use std::path::PathBuf;
use std::time::Instant;

fn load() -> Option<Autocomplete> {
    let dir = std::env::var("GEOCODER_INDEX_DIR").ok()?;
    Autocomplete::open(&PathBuf::from(dir)).ok().flatten()
}

#[test]
fn exact_sydney_hits_the_fst() {
    let Some(a) = load() else { return };
    let hit = a.exact_match(b"au", "sydney").expect("FST should have Sydney");
    assert!(hit.name.to_ascii_lowercase().contains("sydney"));
    // Prominence boost picked the highest-rank Sydney record; should be
    // place (rank 16-19), not a street (rank 26).
    assert!(hit.rank <= 19, "expected place-level rank, got {}", hit.rank);
}

#[test]
fn exact_miss_returns_none() {
    let Some(a) = load() else { return };
    assert!(a.exact_match(b"au", "not_a_real_place_name_xyz").is_none());
}

#[test]
fn too_short_prefix_skips_fst() {
    let Some(a) = load() else { return };
    // Under the min-prefix length. We'd rather fall through to tantivy
    // than pick the FST's arbitrary "a" match.
    assert!(a.exact_match(b"au", "a").is_none());
}

/// Catches the regression class we actually care about: the FST
/// fast-path silently falling through to a Tantivy lookup or an
/// unbounded FST walk. Either of those takes ~milliseconds, so a
/// threshold in tens of microseconds (let alone the 20µs the test
/// originally pinned) is over-tight for shared/busy CI hardware
/// where micro-latency is dominated by scheduler tail.
///
/// Strategy:
/// - Warm caches via 50 calls.
/// - Measure 5 trials of 1000 calls each. Trial duration is a
///   smoothed signal even on noisy hardware; taking the MIN trial
///   strips the slow-trial tail (any run that hit a context
///   switch, page fault, or thermal throttle).
/// - Assert the best trial is <500µs avg per call. In release on
///   bare metal this is ~1.5µs; in debug on quiet hardware ~19µs;
///   in debug on busy CI we've measured up to 80µs. 500µs gives
///   roughly 25× headroom over our worst real measurement and
///   still catches the only regression that matters: Tantivy
///   fallthrough (1–10ms+) or unbounded FST walk (10ms+).
#[test]
fn fast_path_latency_under_500us_best_of_5() {
    let Some(a) = load() else { return };
    // Warm caches and JIT branch prediction.
    for _ in 0..50 {
        let _ = a.exact_match(b"au", "sydney");
    }

    let runs_per_trial = 1000;
    let trials = 5;
    let mut best = std::time::Duration::MAX;
    for _ in 0..trials {
        let t0 = Instant::now();
        for _ in 0..runs_per_trial {
            std::hint::black_box(a.exact_match(b"au", "sydney"));
        }
        let elapsed = t0.elapsed();
        if elapsed < best {
            best = elapsed;
        }
    }
    let per = best / runs_per_trial;
    eprintln!("FST exact_match('sydney') best-of-{trials} avg: {per:?}");
    assert!(
        per.as_micros() < 500,
        "FST fast-path should be <500µs (catches Tantivy fallthrough \
         or unbounded FST walk regressions); got {per:?}. \
         Real release latency is ~1.5µs; if this test starts failing \
         it's almost certainly a real bug, not CI noise."
    );
}
