//! Country-aware mapping of `admin_level` values to the output address
//! fields used by `AdminResult`. Loaded from JSON at startup — the default
//! config is embedded at compile time, and an optional file can override it
//! via `Index::load_with_admin_config` or the `GEOCODER_ADMIN_CONFIG` env
//! var in the binary.
//!
//! The format is deliberately narrower than Nominatim's `address-levels.json`
//! (we don't use per-tag search ranks), but the same principle: per-country
//! overrides layered on top of a global default.

use serde::Deserialize;
use std::collections::HashMap;

/// Which output field an admin polygon populates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminField {
    Country,
    State,
    County,
    /// Sets `county` only if not yet populated (useful for secondary levels
    /// like AU level-7 districts that should only surface when there's no
    /// LGA match).
    CountyIfEmpty,
    City,
    /// Sets `city` only if not yet populated.
    CityIfEmpty,
    Postcode,
    /// Explicit opt-out — present in config so a country can override a
    /// default that would otherwise apply.
    Ignore,
}

/// Either a bare field name (`"city"`) or an object with a field + optional
/// area cap in square degrees. The cap exists specifically for AU where
/// OSM occasionally tags pastoral stations as `admin_level=9` with huge
/// polygons (>1000 km²) — real urban suburbs are always well under that,
/// so rejecting oversized polygons avoids them winning the smallest-area
/// contest over real localities.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAdminEntry {
    Field(AdminField),
    Detailed {
        field: AdminField,
        #[serde(default)]
        max_area: Option<f32>,
    },
}

impl RawAdminEntry {
    fn parts(&self) -> (AdminField, Option<f32>) {
        match self {
            RawAdminEntry::Field(f) => (*f, None),
            RawAdminEntry::Detailed { field, max_area } => (*field, *max_area),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawAdminSection {
    #[serde(default)]
    admin: HashMap<String, RawAdminEntry>,
}

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    defaults: RawAdminSection,
    #[serde(default)]
    countries: HashMap<String, RawAdminSection>,
}

/// Resolved admin-mapping entry: output field plus an optional area cap.
#[derive(Debug, Clone, Copy)]
pub struct AdminEntry {
    pub field: AdminField,
    pub max_area: Option<f32>,
}

/// Resolved admin-mapping lookup table. `by_country[cc][admin_level] = Some(entry)`
/// when the country has an override; otherwise `defaults[admin_level]` applies.
#[derive(Debug, Default, Clone)]
pub struct AdminConfig {
    defaults: HashMap<u8, AdminEntry>,
    by_country: HashMap<String, HashMap<u8, AdminEntry>>,
}

impl AdminConfig {
    /// Load the compiled-in default configuration.
    pub fn embedded_default() -> Self {
        let raw: RawConfig = serde_json::from_str(include_str!("../config/admin-mapping.json"))
            .expect("embedded admin-mapping.json must be valid");
        Self::from_raw(raw)
    }

    /// Parse a JSON string (for tests and file-based overrides).
    pub fn from_json(src: &str) -> Result<Self, String> {
        let raw: RawConfig =
            serde_json::from_str(src).map_err(|e| format!("parse admin config: {e}"))?;
        Ok(Self::from_raw(raw))
    }

    fn from_raw(raw: RawConfig) -> Self {
        let defaults = to_level_map(&raw.defaults);
        let by_country = raw
            .countries
            .into_iter()
            .map(|(cc, section)| (cc.to_ascii_uppercase(), to_level_map(&section)))
            .collect();
        AdminConfig { defaults, by_country }
    }

    /// Look up the output entry for `(country_code, admin_level)`. Returns
    /// `None` to mean "no output" (no mapping or explicit `Ignore`).
    pub fn lookup(&self, country_code: Option<&str>, admin_level: u8) -> Option<AdminEntry> {
        let override_map = country_code
            .and_then(|cc| self.by_country.get(&cc.to_ascii_uppercase()));

        if let Some(map) = override_map {
            if let Some(entry) = map.get(&admin_level) {
                return if entry.field == AdminField::Ignore {
                    None
                } else {
                    Some(*entry)
                };
            }
        }

        self.defaults
            .get(&admin_level)
            .copied()
            .filter(|e| e.field != AdminField::Ignore)
    }
}

fn to_level_map(section: &RawAdminSection) -> HashMap<u8, AdminEntry> {
    section
        .admin
        .iter()
        .filter_map(|(k, v)| {
            k.parse::<u8>().ok().map(|lvl| {
                let (field, max_area) = v.parts();
                (lvl, AdminEntry { field, max_area })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(opt: Option<AdminEntry>) -> Option<AdminField> {
        opt.map(|e| e.field)
    }

    #[test]
    fn defaults_map_admin_levels() {
        let cfg = AdminConfig::embedded_default();
        assert_eq!(field(cfg.lookup(None, 2)), Some(AdminField::Country));
        assert_eq!(field(cfg.lookup(None, 4)), Some(AdminField::State));
        assert_eq!(field(cfg.lookup(None, 6)), Some(AdminField::County));
        assert_eq!(field(cfg.lookup(None, 8)), Some(AdminField::City));
        assert_eq!(field(cfg.lookup(None, 11)), Some(AdminField::Postcode));
    }

    #[test]
    fn au_overrides_level_6_to_county_and_9_to_city() {
        let cfg = AdminConfig::embedded_default();
        assert_eq!(field(cfg.lookup(Some("AU"), 6)), Some(AdminField::County));
        assert_eq!(field(cfg.lookup(Some("AU"), 9)), Some(AdminField::City));
        assert_eq!(field(cfg.lookup(Some("AU"), 7)), Some(AdminField::CountyIfEmpty));
    }

    #[test]
    fn au_level_9_has_max_area_cap() {
        let cfg = AdminConfig::embedded_default();
        let entry = cfg
            .lookup(Some("AU"), 9)
            .expect("AU level 9 should resolve in the embedded default");
        assert_eq!(entry.field, AdminField::City);
        // Cap rejects pastoral-station-sized polygons.
        let cap = entry.max_area.expect("AU level 9 entry should carry max_area cap");
        assert!(cap > 0.01 && cap < 0.1, "cap {cap} should be in urban-suburb range");
    }

    #[test]
    fn nz_overrides_level_6_to_city() {
        let cfg = AdminConfig::embedded_default();
        assert_eq!(field(cfg.lookup(Some("NZ"), 6)), Some(AdminField::City));
        assert_eq!(field(cfg.lookup(Some("NZ"), 10)), Some(AdminField::CountyIfEmpty));
    }

    #[test]
    fn unknown_country_uses_defaults() {
        let cfg = AdminConfig::embedded_default();
        assert_eq!(field(cfg.lookup(Some("XX"), 6)), Some(AdminField::County));
        assert_eq!(field(cfg.lookup(Some("XX"), 8)), Some(AdminField::City));
    }

    #[test]
    fn explicit_ignore_returns_none_and_blocks_default() {
        let cfg = AdminConfig::from_json(
            r#"{"defaults":{"admin":{"6":"county"}},"countries":{"AU":{"admin":{"6":"ignore"}}}}"#,
        )
        .expect("test fixture parses");
        assert_eq!(field(cfg.lookup(Some("AU"), 6)), None);
        assert_eq!(field(cfg.lookup(None, 6)), Some(AdminField::County));
    }

    #[test]
    fn detailed_entry_parses_max_area() {
        let cfg = AdminConfig::from_json(
            r#"{"countries":{"AU":{"admin":{"9":{"field":"city","max_area":0.05}}}}}"#,
        )
        .expect("test fixture parses");
        let entry = cfg.lookup(Some("AU"), 9).expect("AU level 9 present");
        assert_eq!(entry.field, AdminField::City);
        assert_eq!(entry.max_area, Some(0.05));
    }

    #[test]
    fn country_code_is_case_insensitive() {
        let cfg = AdminConfig::embedded_default();
        assert_eq!(field(cfg.lookup(Some("au"), 9)), Some(AdminField::City));
        assert_eq!(field(cfg.lookup(Some("Au"), 9)), Some(AdminField::City));
    }
}
