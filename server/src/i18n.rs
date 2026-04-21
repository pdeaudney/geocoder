//! Localised name lookup backed by `i18n_names.bin`.
//!
//! The builder writes one 16-byte record per `name:<lang>` tag it sees on
//! admin polygons and place points, sorted by `(entity_type, entity_id,
//! lang_code)`. At runtime we binary-search that array when a caller
//! provides a `lang=` hint.
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
    pub _pad0: u8,
    pub lang_code: u16, // packed 2-char ASCII, `'a' | ('b' << 8)`
    pub entity_id: u32,
    pub name_id: u32,   // offset into the same strings.bin everything else uses
    pub _pad1: u32,
}

pub const ENTITY_ADMIN: u8 = 0;
pub const ENTITY_PLACE: u8 = 1;

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

    /// Binary-search for `(entity_type, entity_id, lang_code)`. Returns
    /// the `name_id` offset into `strings.bin` when found — the caller
    /// resolves that to an actual `&str` through the existing string
    /// pool. Ordering is (entity_type, entity_id, lang_code) ascending.
    pub fn lookup(&self, entity_type: u8, entity_id: u32, lang_code: u16) -> Option<u32> {
        let records = self.records();
        records
            .binary_search_by(|rec| {
                (rec.entity_type, rec.entity_id, rec.lang_code)
                    .cmp(&(entity_type, entity_id, lang_code))
            })
            .ok()
            .map(|idx| records[idx].name_id)
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
}
