//! Localised / alternate name lookup backed by `i18n_names.bin`.
//!
//! The builder writes one 16-byte record per `name:<lang>` /
//! `official_name` / `alt_name` (and, from commit 3 onwards,
//! `short_name`/`old_name`/...) tag it sees on admin polygons and place
//! points, sorted by `(entity_type, entity_id, alias_type, lang_code)`.
//! At runtime we binary-search that array when a caller provides a
//! `lang=` hint and walk the per-entity run when collecting alternates
//! for the forward index.
//!
//! Missing file = empty response; the server starts without the file and
//! the query path transparently falls back to the default (OSM `name` tag)
//! name. Deployments that never want localisation don't need to do
//! anything.

use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

/// Record layout matching C++ `I18nName`. The `#[repr(C)]` layout must
/// stay at 16 bytes — the struct_layout tests pin this.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct I18nRecord {
    pub entity_type: u8, // 0 = admin polygon, 1 = place point
    pub alias_type: u8,  // ALIAS_PRIMARY / ALIAS_OFFICIAL / ALIAS_ALT / ...
    pub lang_code: u16,  // packed 2-char ASCII, `'a' | ('b' << 8)`, or 0 if language-agnostic
    pub entity_id: u32,
    pub name_id: u32,    // offset into the same strings.bin everything else uses
    pub _pad1: u32,
}

pub const ENTITY_ADMIN: u8 = 0;
pub const ENTITY_PLACE: u8 = 1;
pub const ENTITY_POI: u8 = 2;

/// Alias-type discriminator stored in `I18nRecord.alias_type`. Mirrors
/// the `ALIAS_*` constants in `builder/src/build_index.cpp`. The byte
/// slot was previously a padding byte and the alias-type was overloaded
/// into `lang_code` via sentinel values; splitting it off lets us
/// represent alias-type × language as a 2-D key (e.g. `short_name:fr`,
/// `old_name:en`) and removes the sentinel-collision footgun.
pub const ALIAS_PRIMARY: u8 = 0; // variant of the primary `name` tag (`name:<lang>`)
pub const ALIAS_OFFICIAL: u8 = 1; // `official_name` / `official_name:<lang>`
pub const ALIAS_ALT: u8 = 2; // `alt_name` / `alt_name:<lang>` (also `name:left`/`right`)
pub const ALIAS_SHORT: u8 = 3; // `short_name` / `short_name:<lang>` — e.g. "JFK"
pub const ALIAS_OLD: u8 = 4; // `old_name` / `old_name:<lang>` — e.g. "Bombay"
pub const ALIAS_LOC: u8 = 5; // `loc_name` / `loc_name:<lang>` — informal local name
pub const ALIAS_INT: u8 = 6; // `int_name` / `int_name:<lang>` — international form
pub const ALIAS_REG: u8 = 7; // `reg_name` / `reg_name:<lang>` — regional form
pub const ALIAS_REF: u8 = 8; // `ref` — typically road / route reference (e.g. "A14")
pub const ALIAS_INT_REF: u8 = 9; // `int_ref` — international ref (e.g. "E40")
pub const ALIAS_NAT_REF: u8 = 10; // `nat_ref` — national ref

pub struct I18nNames {
    mmap: Mmap,
}

impl I18nNames {
    /// Open `<dir>/i18n_names.bin`. Returns `Ok(None)` when the file
    /// is missing (or zero bytes), so callers treat localisation as a
    /// best-effort enhancement rather than a hard dependency.
    pub fn open(dir: &Path) -> Result<Option<Self>, String> {
        let path = dir.join("i18n_names.bin");
        if !path.exists() {
            return Ok(None);
        }
        let f = File::open(&path).map_err(|e| format!("open {}: {}", path.display(), e))?;
        let mmap =
            unsafe { Mmap::map(&f) }.map_err(|e| format!("mmap {}: {}", path.display(), e))?;
        if mmap.is_empty() {
            return Ok(None);
        }
        if mmap.len() % std::mem::size_of::<I18nRecord>() != 0 {
            return Err(format!(
                "{} size {} is not a multiple of {}",
                path.display(),
                mmap.len(),
                std::mem::size_of::<I18nRecord>()
            ));
        }
        Ok(Some(I18nNames { mmap }))
    }

    /// Typed view over the mmap. Public so tooling (the DuckDB index
    /// dumper in particular) can iterate records without copying or
    /// parsing the file a second time. Hot-path callers go through
    /// `lookup` instead.
    pub fn records(&self) -> &[I18nRecord] {
        crate::as_typed_slice(&self.mmap)
    }

    /// Binary-search for `(entity_type, entity_id, alias_type, lang_code)`.
    /// Returns the `name_id` offset into `strings.bin` when found — the
    /// caller resolves that to an actual `&str` through the existing
    /// string pool. Ordering is
    /// `(entity_type, entity_id, alias_type, lang_code)` ascending.
    pub fn lookup(
        &self,
        entity_type: u8,
        entity_id: u32,
        alias_type: u8,
        lang_code: u16,
    ) -> Option<u32> {
        let records = self.records();
        records
            .binary_search_by(|rec| {
                (rec.entity_type, rec.entity_id, rec.alias_type, rec.lang_code)
                    .cmp(&(entity_type, entity_id, alias_type, lang_code))
            })
            .ok()
            .map(|idx| records[idx].name_id)
    }

    /// All alternate names for one entity. Returns
    /// `(alias_type, lang_code, name_id)` triples. The on-disk array is
    /// sorted by `(entity_type, entity_id, alias_type, lang_code)`, so we
    /// partition_point to the run's start and walk while
    /// `(entity_type, entity_id)` stays constant. O(log N + k) where k
    /// is the per-entity count.
    pub fn alternates_for(
        &self,
        entity_type: u8,
        entity_id: u32,
    ) -> impl Iterator<Item = (u8, u16, u32)> + '_ {
        let records = self.records();
        let start = records.partition_point(|rec| {
            (rec.entity_type, rec.entity_id) < (entity_type, entity_id)
        });
        records[start..]
            .iter()
            .take_while(move |rec| rec.entity_type == entity_type && rec.entity_id == entity_id)
            .map(|rec| (rec.alias_type, rec.lang_code, rec.name_id))
    }
}

/// Parse a user-supplied language tag like `"en"`, `"FR"`, `"en-US"` into
/// the packed 2-char lowercase form stored in the index. For richer BCP-47
/// tags we just take the first two letters — `en-US` and `en-AU` both
/// match records tagged `name:en`, which is the pragmatic thing to do.
pub fn pack_lang_code(lang: &str) -> Option<u16> {
    let bytes = lang.trim().as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let a = bytes[0].to_ascii_lowercase();
    let b = bytes[1].to_ascii_lowercase();
    if !a.is_ascii_alphabetic() || !b.is_ascii_alphabetic() {
        return None;
    }
    Some(a as u16 | ((b as u16) << 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_16_bytes() {
        assert_eq!(std::mem::size_of::<I18nRecord>(), 16);
    }

    #[test]
    fn lang_packing_roundtrip() {
        assert_eq!(pack_lang_code("en"), Some(('e' as u16) | (('n' as u16) << 8)));
        assert_eq!(pack_lang_code("FR"), Some(('f' as u16) | (('r' as u16) << 8)));
        // Extended tags — we take the first two letters.
        assert_eq!(pack_lang_code("en-US"), pack_lang_code("en"));
        assert_eq!(pack_lang_code("zh-Hant"), pack_lang_code("zh"));
        // Rejected inputs.
        assert_eq!(pack_lang_code(""), None);
        assert_eq!(pack_lang_code("x"), None);
        assert_eq!(pack_lang_code("12"), None);
    }

    /// All ALIAS_* and ENTITY_* constants must be distinct values.
    /// A duplicate would silently merge two alias families on disk
    /// (e.g. ALIAS_OLD=ALIAS_LOC) and a `short_name` query would
    /// resolve to the wrong tag family. Cheap insurance against
    /// future const additions colliding.
    #[test]
    fn alias_and_entity_constants_are_distinct() {
        let aliases: [u8; 11] = [
            ALIAS_PRIMARY, ALIAS_OFFICIAL, ALIAS_ALT, ALIAS_SHORT,
            ALIAS_OLD, ALIAS_LOC, ALIAS_INT, ALIAS_REG, ALIAS_REF,
            ALIAS_INT_REF, ALIAS_NAT_REF,
        ];
        let mut sorted: Vec<u8> = aliases.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            aliases.len(),
            "duplicate ALIAS_* constants: {aliases:?}"
        );
        let entities: [u8; 3] = [ENTITY_ADMIN, ENTITY_PLACE, ENTITY_POI];
        let mut esorted: Vec<u8> = entities.to_vec();
        esorted.sort();
        esorted.dedup();
        assert_eq!(esorted.len(), entities.len(), "duplicate ENTITY_* constants");
    }

    /// alternates_for must yield the alias_type AND lang_code
    /// alongside the name_id, even for entries the consumer doesn't
    /// currently care about. A regression to the old `(lang_code,
    /// name_id)` tuple shape would break the C++→Rust binary contract
    /// without a struct_layout failure (the on-disk layout is the
    /// same; only the iterator shape changed). Binary search +
    /// take_while behaviour is the load-bearing thing tested here.
    #[test]
    fn alternates_for_yields_alias_type_and_lang() {
        // Two POIs (entity_id 100 and 200) each with three aliases.
        // We'll partition_point + take_while on entity 100 and pin
        // both the order and the field surfacing.
        let mut recs = vec![
            // entity 100
            I18nRecord {
                entity_type: ENTITY_POI, alias_type: ALIAS_PRIMARY,
                lang_code: pack_lang_code("en").unwrap(), entity_id: 100,
                name_id: 1, _pad1: 0,
            },
            I18nRecord {
                entity_type: ENTITY_POI, alias_type: ALIAS_SHORT,
                lang_code: 0, entity_id: 100, name_id: 2, _pad1: 0,
            },
            I18nRecord {
                entity_type: ENTITY_POI, alias_type: ALIAS_OLD,
                lang_code: 0, entity_id: 100, name_id: 3, _pad1: 0,
            },
            // entity 200 — must NOT be returned for the entity-100 walk
            I18nRecord {
                entity_type: ENTITY_POI, alias_type: ALIAS_PRIMARY,
                lang_code: pack_lang_code("fr").unwrap(), entity_id: 200,
                name_id: 999, _pad1: 0,
            },
        ];
        recs.sort_by_key(|r| (r.entity_type, r.entity_id, r.alias_type, r.lang_code));

        // Walk like alternates_for does (partition_point +
        // take_while). We can't exercise I18nNames directly without
        // building an i18n_names.bin mmap, so this mirrors the
        // iterator logic against the same in-memory slice.
        let start = recs.partition_point(|rec| {
            (rec.entity_type, rec.entity_id) < (ENTITY_POI, 100)
        });
        let walked: Vec<(u8, u16, u32)> = recs[start..]
            .iter()
            .take_while(|rec| rec.entity_type == ENTITY_POI && rec.entity_id == 100)
            .map(|rec| (rec.alias_type, rec.lang_code, rec.name_id))
            .collect();

        // Three rows for entity 100, ordered by (alias_type,
        // lang_code) — PRIMARY (0) < SHORT (3) < OLD (4).
        assert_eq!(walked.len(), 3, "entity 100 has three aliases");
        assert_eq!(walked[0].0, ALIAS_PRIMARY);
        assert_eq!(walked[0].2, 1, "PRIMARY:en → name_id 1");
        assert_eq!(walked[1].0, ALIAS_SHORT);
        assert_eq!(walked[1].2, 2, "SHORT:0 → name_id 2");
        assert_eq!(walked[2].0, ALIAS_OLD);
        assert_eq!(walked[2].2, 3, "OLD:0 → name_id 3");
        // entity 200's row must NOT have leaked in.
        assert!(walked.iter().all(|(_, _, nid)| *nid != 999));
    }

    /// Pin the contract that `alias_type` distinguishes records with the
    /// same `(entity, lang)`. A regression here means a `lang=de` query
    /// could silently return an `official_name` (`alias_type=OFFICIAL,
    /// lang_code=0`) instead of `name:de` (`alias_type=PRIMARY,
    /// lang_code='de'`), which is exactly what splitting the alias_type
    /// byte off the lang_code is meant to prevent.
    #[test]
    fn alias_type_distinguishes_resolution() {
        // Build a tiny in-memory index with three records for the same
        // entity: name:en, official_name (no lang), alt_name (no lang).
        let recs = [
            I18nRecord {
                entity_type: ENTITY_PLACE,
                alias_type: ALIAS_PRIMARY,
                lang_code: pack_lang_code("en").unwrap(),
                entity_id: 42,
                name_id: 100,
                _pad1: 0,
            },
            I18nRecord {
                entity_type: ENTITY_PLACE,
                alias_type: ALIAS_OFFICIAL,
                lang_code: 0,
                entity_id: 42,
                name_id: 200,
                _pad1: 0,
            },
            I18nRecord {
                entity_type: ENTITY_PLACE,
                alias_type: ALIAS_ALT,
                lang_code: 0,
                entity_id: 42,
                name_id: 300,
                _pad1: 0,
            },
        ];
        // Sort to mirror the on-disk invariant.
        let mut recs = recs;
        recs.sort_by_key(|r| (r.entity_type, r.entity_id, r.alias_type, r.lang_code));

        // Pure-Rust binary search to mirror what `lookup` does over an
        // mmap-backed slice — this lets the test assert ordering &
        // resolution semantics without building an actual i18n_names.bin.
        let lookup = |alias: u8, lang: u16| -> Option<u32> {
            recs.binary_search_by(|rec| {
                (rec.entity_type, rec.entity_id, rec.alias_type, rec.lang_code)
                    .cmp(&(ENTITY_PLACE, 42, alias, lang))
            })
            .ok()
            .map(|i| recs[i].name_id)
        };

        // PRIMARY + 'en' must resolve to the name:en row, not collapse
        // into OFFICIAL/ALT just because they share the entity.
        assert_eq!(lookup(ALIAS_PRIMARY, pack_lang_code("en").unwrap()), Some(100));
        assert_eq!(lookup(ALIAS_OFFICIAL, 0), Some(200));
        assert_eq!(lookup(ALIAS_ALT, 0), Some(300));
        // Cross-axis miss: no PRIMARY-with-lang=0, no OFFICIAL-with-lang=en.
        assert_eq!(lookup(ALIAS_PRIMARY, 0), None);
        assert_eq!(lookup(ALIAS_OFFICIAL, pack_lang_code("en").unwrap()), None);
    }
}
