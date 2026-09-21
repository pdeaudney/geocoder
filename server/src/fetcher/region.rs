//! OSM region preset → (URL, dest filename) set.
//!
//! 16 presets:
//!
//!   - 9 Geofabrik continent extracts (the `--region all-continents`
//!     fan-out): africa, antarctica, asia, australia-oceania,
//!     central-america, europe, north-america, russia, south-america
//!   - 6 Geofabrik sub-region shortcuts: australia, new-zealand, niue,
//!     united-kingdom, canada, usa
//!   - 1 planet stream from planet.openstreetmap.org
//!
//! `oceania` is an alias for `australia-oceania` — the full continent
//! extract, not AU+NZ-only (the latter would silently drop Fiji, PNG,
//! Vanuatu, Solomon Is, etc.). The `australia` and `new-zealand`
//! sub-region presets exist for callers that explicitly want only
//! those countries.

use std::str::FromStr;

use anyhow::{anyhow, Result};
use url::Url;

const GEOFABRIK: &str = "https://download.geofabrik.de";
const PLANET: &str = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf";

/// Top-level OSM region preset. Drives URL derivation, the
/// continent-fan-out logic, and the planet/continent mismatch check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Africa,
    Antarctica,
    Asia,
    AustraliaOceania,
    CentralAmerica,
    Europe,
    NorthAmerica,
    Russia,
    SouthAmerica,
    Australia,
    NewZealand,
    Niue,
    UnitedKingdom,
    Canada,
    Usa,
    Planet,
    AllContinents,
}

impl Region {
    /// Continents iterated by `--region all-continents`. Alphabetical
    /// with `australia-oceania` between `asia` and `central-america`.
    pub const ALL_CONTINENTS: [Region; 9] = [
        Region::Africa,
        Region::Antarctica,
        Region::Asia,
        Region::AustraliaOceania,
        Region::CentralAmerica,
        Region::Europe,
        Region::NorthAmerica,
        Region::Russia,
        Region::SouthAmerica,
    ];

    /// Resolve the region to its set of URLs. `AllContinents` returns
    /// 9 URLs; everything else returns one.
    pub fn urls(self) -> Vec<Url> {
        match self {
            Region::AllContinents => Region::ALL_CONTINENTS
                .iter()
                .map(|r| r.urls().pop().expect("single-URL region"))
                .collect(),
            Region::Planet => vec![Url::parse(PLANET).expect("static planet URL is valid")],
            single => {
                vec![Url::parse(&single.geofabrik_url()).expect("static Geofabrik URL is valid")]
            }
        }
    }

    /// Single-URL Geofabrik path. Panics for `AllContinents` /
    /// `Planet` because they have non-Geofabrik or fan-out semantics.
    fn geofabrik_url(self) -> String {
        let path = match self {
            Region::Africa => "africa-latest.osm.pbf",
            Region::Antarctica => "antarctica-latest.osm.pbf",
            Region::Asia => "asia-latest.osm.pbf",
            Region::AustraliaOceania => "australia-oceania-latest.osm.pbf",
            Region::CentralAmerica => "central-america-latest.osm.pbf",
            Region::Europe => "europe-latest.osm.pbf",
            Region::NorthAmerica => "north-america-latest.osm.pbf",
            Region::Russia => "russia-latest.osm.pbf",
            Region::SouthAmerica => "south-america-latest.osm.pbf",
            Region::Australia => "australia-oceania/australia-latest.osm.pbf",
            Region::NewZealand => "australia-oceania/new-zealand-latest.osm.pbf",
            Region::Niue => "australia-oceania/niue-latest.osm.pbf",
            Region::UnitedKingdom => "europe/united-kingdom-latest.osm.pbf",
            Region::Canada => "north-america/canada-latest.osm.pbf",
            Region::Usa => "north-america/us-latest.osm.pbf",
            Region::Planet | Region::AllContinents => {
                panic!("geofabrik_url() called on Planet/AllContinents")
            }
        };
        format!("{GEOFABRIK}/{path}")
    }

    /// Geofabrik publishes `<file>.md5` sidecars per extract. Planet
    /// (planet.osm.org) doesn't, so we return None for Planet.
    pub fn md5_url(url: &Url) -> Option<Url> {
        if url.host_str() == Some("download.geofabrik.de") {
            Url::parse(&format!("{}.md5", url.as_str())).ok()
        } else {
            None
        }
    }

    /// Replication state.txt URL. Geofabrik publishes
    /// `<region>-updates/state.txt`; planet.osm.org publishes
    /// `replication/day/state.txt`.
    pub fn state_url(url: &Url) -> Option<Url> {
        let s = url.as_str();
        // Planet has its own replication URL pattern; check before
        // the generic Geofabrik suffix-strip (which would otherwise
        // produce planet-updates/state.txt — wrong host, wrong path).
        if s == PLANET {
            return Url::parse("https://planet.openstreetmap.org/replication/day/state.txt").ok();
        }
        if let Some(stem) = s.strip_suffix("-latest.osm.pbf") {
            Url::parse(&format!("{stem}-updates/state.txt")).ok()
        } else {
            None
        }
    }

    /// Whether this preset produces planet-pattern files vs continent
    /// pattern. Drives mismatch detection.
    pub fn is_planet(self) -> bool {
        matches!(self, Region::Planet)
    }

    /// Whether this preset produces multiple continent files.
    pub fn is_all_continents(self) -> bool {
        matches!(self, Region::AllContinents)
    }
}

impl FromStr for Region {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "africa" => Ok(Region::Africa),
            "antarctica" => Ok(Region::Antarctica),
            "asia" => Ok(Region::Asia),
            // `oceania` is an alias for the full continent extract.
            "oceania" | "australia-oceania" => Ok(Region::AustraliaOceania),
            "central-america" => Ok(Region::CentralAmerica),
            "europe" => Ok(Region::Europe),
            "north-america" => Ok(Region::NorthAmerica),
            "russia" => Ok(Region::Russia),
            "south-america" => Ok(Region::SouthAmerica),
            "au" | "australia" => Ok(Region::Australia),
            "nz" | "new-zealand" => Ok(Region::NewZealand),
            "niue" => Ok(Region::Niue),
            "uk" | "gb" | "united-kingdom" => Ok(Region::UnitedKingdom),
            "ca" | "canada" => Ok(Region::Canada),
            "usa" | "us" => Ok(Region::Usa),
            "planet" => Ok(Region::Planet),
            "all-continents" => Ok(Region::AllContinents),
            other => Err(anyhow!(
                "unknown region '{other}' (expected one of: africa, antarctica, asia, oceania, australia-oceania, central-america, europe, north-america, russia, south-america, au, nz, uk, ca, usa, planet, all-continents)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_resolve_to_expected_urls() {
        // Lock in the URL set so that a typo or refactor changes the
        // table here, not the runtime.
        let cases: &[(Region, &[&str])] = &[
            (
                Region::Africa,
                &["https://download.geofabrik.de/africa-latest.osm.pbf"],
            ),
            (
                Region::Antarctica,
                &["https://download.geofabrik.de/antarctica-latest.osm.pbf"],
            ),
            (
                Region::Asia,
                &["https://download.geofabrik.de/asia-latest.osm.pbf"],
            ),
            (
                Region::AustraliaOceania,
                &["https://download.geofabrik.de/australia-oceania-latest.osm.pbf"],
            ),
            (
                Region::CentralAmerica,
                &["https://download.geofabrik.de/central-america-latest.osm.pbf"],
            ),
            (
                Region::Europe,
                &["https://download.geofabrik.de/europe-latest.osm.pbf"],
            ),
            (
                Region::NorthAmerica,
                &["https://download.geofabrik.de/north-america-latest.osm.pbf"],
            ),
            (
                Region::Russia,
                &["https://download.geofabrik.de/russia-latest.osm.pbf"],
            ),
            (
                Region::SouthAmerica,
                &["https://download.geofabrik.de/south-america-latest.osm.pbf"],
            ),
            (
                Region::Australia,
                &["https://download.geofabrik.de/australia-oceania/australia-latest.osm.pbf"],
            ),
            (
                Region::NewZealand,
                &["https://download.geofabrik.de/australia-oceania/new-zealand-latest.osm.pbf"],
            ),
            (
                Region::Niue,
                &["https://download.geofabrik.de/australia-oceania/niue-latest.osm.pbf"],
            ),
            (
                Region::UnitedKingdom,
                &["https://download.geofabrik.de/europe/united-kingdom-latest.osm.pbf"],
            ),
            (
                Region::Canada,
                &["https://download.geofabrik.de/north-america/canada-latest.osm.pbf"],
            ),
            (
                Region::Usa,
                &["https://download.geofabrik.de/north-america/us-latest.osm.pbf"],
            ),
            (
                Region::Planet,
                &["https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf"],
            ),
        ];
        for (region, expected) in cases {
            let actual: Vec<String> = region.urls().into_iter().map(|u| u.to_string()).collect();
            let expected_owned: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
            assert_eq!(actual, expected_owned, "region={region:?}");
        }
    }

    #[test]
    fn all_continents_returns_nine_geofabrik_urls() {
        let urls = Region::AllContinents.urls();
        assert_eq!(urls.len(), 9);
        for url in &urls {
            assert_eq!(
                url.host_str(),
                Some("download.geofabrik.de"),
                "all-continents must route through Geofabrik (got {url})",
            );
            assert!(
                url.path().ends_with("-latest.osm.pbf"),
                "expected continent extract suffix, got {url}",
            );
        }
    }

    #[test]
    fn oceania_alias_resolves_to_full_continent() {
        // Locks in the bug fix from commit 332b7f7: `oceania` must NOT
        // mean AU + NZ only. It maps to the full australia-oceania
        // continent extract (Fiji, PNG, etc.).
        assert_eq!(
            Region::from_str("oceania").unwrap().urls(),
            Region::AustraliaOceania.urls()
        );
    }

    #[test]
    fn english_country_aliases_resolve_to_country_extracts() {
        assert_eq!(Region::from_str("uk").unwrap(), Region::UnitedKingdom);
        assert_eq!(Region::from_str("gb").unwrap(), Region::UnitedKingdom);
        assert_eq!(Region::from_str("ca").unwrap(), Region::Canada);
    }

    #[test]
    fn unknown_region_errors() {
        let err = Region::from_str("middle-earth").unwrap_err();
        assert!(
            err.to_string().contains("unknown region 'middle-earth'"),
            "expected helpful error, got {err}"
        );
    }

    #[test]
    fn md5_sidecar_only_for_geofabrik() {
        let geofabrik = Url::parse("https://download.geofabrik.de/africa-latest.osm.pbf").unwrap();
        let planet =
            Url::parse("https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf").unwrap();
        assert_eq!(
            Region::md5_url(&geofabrik).map(|u| u.to_string()),
            Some("https://download.geofabrik.de/africa-latest.osm.pbf.md5".to_string())
        );
        assert!(Region::md5_url(&planet).is_none());
    }

    #[test]
    fn state_url_derivation() {
        let geofabrik = Url::parse("https://download.geofabrik.de/africa-latest.osm.pbf").unwrap();
        let planet =
            Url::parse("https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf").unwrap();
        let nested =
            Url::parse("https://download.geofabrik.de/north-america/us-latest.osm.pbf").unwrap();
        assert_eq!(
            Region::state_url(&geofabrik).map(|u| u.to_string()),
            Some("https://download.geofabrik.de/africa-updates/state.txt".to_string())
        );
        assert_eq!(
            Region::state_url(&planet).map(|u| u.to_string()),
            Some("https://planet.openstreetmap.org/replication/day/state.txt".to_string())
        );
        assert_eq!(
            Region::state_url(&nested).map(|u| u.to_string()),
            Some("https://download.geofabrik.de/north-america/us-updates/state.txt".to_string())
        );
    }
}
