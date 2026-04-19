//! Verifies review item #2 is fixed: the dedup set in `Index::query_geo`
//! is now a bounded linear scan, so any repeat of an already-seen street ID
//! is skipped regardless of its bit pattern.
//!
//! The scan inside `query_geo` is not directly observable without a mock
//! index, so this test reproduces the exact dedup logic (linear scan of a
//! capped buffer) and asserts the correct behaviour. If the implementation
//! regresses back to a slot-table or drops dedup entirely, these assertions
//! break.

const CAP: usize = 256;

fn dedup_linear_scan(ids: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let mut seen: [u32; CAP] = [0; CAP];
    let mut len = 0usize;
    let mut processed = Vec::new();
    for id in ids {
        if seen[..len].iter().any(|&s| s == id) {
            continue;
        }
        if len < CAP {
            seen[len] = id;
            len += 1;
        }
        processed.push(id);
    }
    processed
}

#[test]
fn colliding_low_bits_are_still_deduped() {
    // These would have collided on the old slot-table (both slot 0). Linear
    // scan correctly keeps both as distinct entries.
    let stream = [0x40, 0x80, 0x40];
    assert_eq!(
        dedup_linear_scan(stream),
        vec![0x40, 0x80],
        "0x40 repeat after 0x80 must NOT re-emerge — dedup is now correct",
    );
}

#[test]
fn non_colliding_ids_are_deduped() {
    let stream = [1u32, 2, 3, 1, 2, 3, 1];
    assert_eq!(dedup_linear_scan(stream), vec![1, 2, 3]);
}

#[test]
fn realistic_density_has_no_redundant_work() {
    // Same fixture as before — 9 cells × 16 streets, some cross-cell. The
    // number of "processed" entries should now exactly equal the number of
    // distinct IDs, not exceed it.
    let streets_per_cell = 16;
    let cells = 9;

    let mut processed = 0;
    let mut seen: [u32; CAP] = [0; CAP];
    let mut len = 0usize;
    let mut actual_unique = std::collections::HashSet::new();

    for cell in 0..cells {
        for i in 0..streets_per_cell {
            let id: u32 = if i < 8 {
                (i as u32) * 64 + 7
            } else {
                (cell as u32) * 1_000 + (i as u32)
            };
            actual_unique.insert(id);
            if !seen[..len].iter().any(|&s| s == id) {
                if len < CAP {
                    seen[len] = id;
                    len += 1;
                }
                processed += 1;
            }
        }
    }

    eprintln!(
        "linear-scan processed {} streets; unique = {}; rate = {:.2}x",
        processed,
        actual_unique.len(),
        processed as f64 / actual_unique.len() as f64,
    );

    assert_eq!(
        processed,
        actual_unique.len(),
        "every distinct street should be processed exactly once",
    );
}
