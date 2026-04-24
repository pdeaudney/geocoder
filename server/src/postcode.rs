//! Postcode lookup backed by G-NAF LOCALITY data.
//!
//! ## Format
//!
//! `postcode_lookup.bin` is a sorted array of fixed-size records for fast
//! binary search. Each record is:
//!
//! ```text
//! struct PostcodeEntry {
//!     key_hash: u64,        // FNV-1a of "<state_abbr>|<normalised_locality>"
//!     postcode_offset: u32, // offset into postcode_lookup_strings.bin
//!     _pad: u32,
//! }
//! ```
//!
//! A 64-bit hash is used instead of the raw strings: AU has ~17k localities,
//! 64-bit collision probability is <1e-10 at this scale. This keeps each
//! record at 16 bytes and the whole file at ~270 KB for AU.
//!
//! Strings live in a separate NUL-terminated pool. Only the postcode values
//! are stored — the hash covers the (state, locality) lookup key.
//!
//! ## Hash algorithm choice
//!
//! We deliberately use **FNV-1a** — a fixed, public, deterministic
//! algorithm — rather than `std::collections::hash_map::DefaultHasher`
//! whose algorithm is explicitly not stable across Rust releases
//! (<https://doc.rust-lang.org/std/collections/hash_map/struct.DefaultHasher.html>).
//! The build-side and query-side of this lookup must produce the same
//! u64 forever, independent of toolchain upgrades. If `DefaultHasher`
//! ever changes algorithm, every postcode on disk would hash to a
//! different key and the lookup would return all-miss with no error.
//! FNV-1a is slower than FxHash but ~17k lookups per server lifetime
//! is not the bottleneck, and no-new-dep is worth the simplicity.
//!
//! ## Build
//!
//! Populated by `build-postcode-lookup` reading G-NAF PSV files:
//! - `<state>_LOCALITY_psv.psv` — LOCALITY_PID, LOCALITY_NAME,
//!   PRIMARY_POSTCODE, STATE_PID, …
//! - `<state>_STATE_psv.psv` — STATE_PID, STATE_ABBREVIATION, …
//!
//! Join on STATE_PID, then emit `(state_abbr, locality_name) → postcode` for
//! every locality with a `PRIMARY_POSTCODE`.

use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

/// Size of a single entry in `postcode_lookup.bin`, in bytes.
pub const ENTRY_SIZE: usize = 16;

/// FNV-1a 64-bit seed and multiplier — the published constants. Do not
/// change these. Changing either would silently invalidate every
/// `postcode_lookup.bin` ever built.
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x00000100000001b3;

/// FNV-1a over `bytes`, continuing from `hash`.
#[inline]
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Hash function applied symmetrically at build-time and query-time.
///
/// The lookup is keyed by `(state_abbr, normalised_locality)`. Both sides
/// lowercase, strip diacritics, and strip punctuation before hashing so
/// "St. Kilda" and "St Kilda" collide correctly.
pub fn lookup_hash(state_abbr: &str, locality: &str) -> u64 {
    let state = normalise_locality(state_abbr);
    let loc = normalise_locality(locality);
    let mut h = FNV_OFFSET;
    h = fnv1a(h, state.as_bytes());
    h = fnv1a(h, b"|");
    h = fnv1a(h, loc.as_bytes());
    h
}

/// Normalise a locality name for postcode-lookup matching: ASCII fold,
/// lowercase, collapse all non-alphanumeric runs to single spaces, trim.
pub fn normalise_locality(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;
    for ch in s.chars() {
        let folded = ascii_fold_char(ch);
        for c in folded.chars() {
            if c.is_alphanumeric() {
                out.extend(c.to_lowercase());
                last_was_space = false;
            } else if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn ascii_fold_char(ch: char) -> String {
    match ch {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => "a".into(),
        'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => "A".into(),
        'é' | 'è' | 'ê' | 'ë' => "e".into(),
        'É' | 'È' | 'Ê' | 'Ë' => "E".into(),
        'í' | 'ì' | 'î' | 'ï' => "i".into(),
        'Í' | 'Ì' | 'Î' | 'Ï' => "I".into(),
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => "o".into(),
        'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' | 'Ø' => "O".into(),
        'ú' | 'ù' | 'û' | 'ü' => "u".into(),
        'Ú' | 'Ù' | 'Û' | 'Ü' => "U".into(),
        'ñ' => "n".into(),
        'Ñ' => "N".into(),
        'ç' => "c".into(),
        'Ç' => "C".into(),
        'ß' => "ss".into(),
        c => c.to_string(),
    }
}

// --- Query side ---

/// Runtime postcode lookup. Opens `postcode_lookup.bin` and
/// `postcode_lookup_strings.bin` via mmap and binary-searches the records.
///
/// When either file is missing the lookup is a no-op (returns `None` for
/// every query) — this is the intended behaviour for deployments that
/// haven't run `build-postcode-lookup`.
pub struct PostcodeLookup {
    entries: Mmap,
    strings: Mmap,
}

impl PostcodeLookup {
    /// Open the lookup pair at `dir/postcode_lookup.bin` +
    /// `dir/postcode_lookup_strings.bin`. Returns `Ok(None)` when either
    /// file is absent so the caller can treat postcode lookup as optional.
    pub fn open(dir: &Path) -> Result<Option<Self>, String> {
        let entries_path = dir.join("postcode_lookup.bin");
        let strings_path = dir.join("postcode_lookup_strings.bin");
        if !entries_path.exists() || !strings_path.exists() {
            return Ok(None);
        }

        let entries_file = File::open(&entries_path)
            .map_err(|e| format!("open {}: {}", entries_path.display(), e))?;
        let entries = unsafe { Mmap::map(&entries_file) }
            .map_err(|e| format!("mmap {}: {}", entries_path.display(), e))?;

        let strings_file = File::open(&strings_path)
            .map_err(|e| format!("open {}: {}", strings_path.display(), e))?;
        let strings = unsafe { Mmap::map(&strings_file) }
            .map_err(|e| format!("mmap {}: {}", strings_path.display(), e))?;

        if entries.len() % ENTRY_SIZE != 0 {
            return Err(format!(
                "{} size {} is not a multiple of {} — corrupt index",
                entries_path.display(),
                entries.len(),
                ENTRY_SIZE,
            ));
        }

        Ok(Some(PostcodeLookup { entries, strings }))
    }

    /// Look up a postcode by state abbreviation (e.g. "NSW") and locality
    /// name (e.g. "Baulkham Hills"). Returns `None` when no match is found,
    /// which includes both "locality not in G-NAF" and "lookup file empty".
    pub fn postcode(&self, state_abbr: &str, locality: &str) -> Option<&str> {
        if state_abbr.is_empty() || locality.is_empty() {
            return None;
        }
        let key = lookup_hash(state_abbr, locality);
        let count = self.entries.len() / ENTRY_SIZE;

        // Binary search on the sorted key_hash.
        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let offset = mid * ENTRY_SIZE;
            let mid_key = u64::from_le_bytes(
                self.entries[offset..offset + 8]
                    .try_into()
                    .expect("16-byte slice always reads as 8 bytes starting at 0"),
            );
            if mid_key == key {
                let str_offset = u32::from_le_bytes(
                    self.entries[offset + 8..offset + 12]
                        .try_into()
                        .expect("16-byte slice always reads as 4 bytes at offset 8"),
                ) as usize;
                return read_cstr(&self.strings, str_offset);
            } else if mid_key < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        None
    }
}

fn read_cstr(pool: &[u8], offset: usize) -> Option<&str> {
    let bytes = pool.get(offset..)?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).ok()
}

// --- Build side ---

/// Record emitted by the builder, serialised into `postcode_lookup.bin`.
/// Kept in this module so build + query agree on the byte layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawEntry {
    pub key_hash: u64,
    pub postcode_offset: u32,
    pub _pad: u32,
}

impl RawEntry {
    pub fn to_le_bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&self.key_hash.to_le_bytes());
        out[8..12].copy_from_slice(&self.postcode_offset.to_le_bytes());
        out[12..16].copy_from_slice(&self._pad.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_is_diacritic_and_punctuation_agnostic() {
        assert_eq!(normalise_locality("St. Kilda"), "st kilda");
        assert_eq!(normalise_locality("St Kilda"), "st kilda");
        assert_eq!(normalise_locality("  Baulkham   Hills  "), "baulkham hills");
        assert_eq!(normalise_locality("Cañon City"), "canon city");
    }

    #[test]
    fn hash_is_deterministic_across_case_and_punctuation() {
        let a = lookup_hash("NSW", "Baulkham Hills");
        let b = lookup_hash("nsw", "baulkham hills");
        let c = lookup_hash("NSW", "Baulkham  Hills"); // double space
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn different_states_with_same_locality_hash_differently() {
        // Some locality names exist in multiple states — state abbr must
        // disambiguate.
        let nsw = lookup_hash("NSW", "Armidale");
        let sa = lookup_hash("SA", "Armidale");
        assert_ne!(nsw, sa);
    }

    #[test]
    fn hash_is_stable_across_toolchain_versions() {
        // CRITICAL — pins the FNV-1a output for a known (state, locality)
        // pair. If this test ever fails, the hash algorithm has been
        // changed or a rustc upgrade has broken something unexpected, and
        // every previously-built `postcode_lookup.bin` is now unreadable.
        // Rebuild all indexes; do not paper over by updating the constant.
        //
        // The expected value was captured after the FNV-1a switch; any
        // drift is a real incident.
        let h = lookup_hash("NSW", "Baulkham Hills");
        // Captured after the FNV-1a swap; any drift here indicates the
        // hash output has changed and all postcode_lookup.bin files are
        // unreadable until rebuilt.
        assert_eq!(
            h, 14417430809016431040u64,
            "postcode hash drift: existing indexes are unreadable"
        );
    }

    #[test]
    fn fnv1a_matches_published_vectors() {
        // Guard against a silent corruption of FNV_OFFSET / FNV_PRIME.
        // These are the published FNV-1a 64-bit test vectors for the
        // empty input and the literal "a". Changing the constants makes
        // this test fail without touching any of the product code.
        assert_eq!(fnv1a(FNV_OFFSET, b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a(FNV_OFFSET, b"a"), 0xaf63dc4c8601ec8c);
    }
}
