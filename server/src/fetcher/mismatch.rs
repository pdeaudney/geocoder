//! Planet/continent PBF directory consistency check.
//!
//! `build-index` globs `*.osm.pbf`, so mixing `planet-latest.osm.pbf`
//! with `<continent>-latest.osm.pbf` files in the same directory
//! would silently double-process every node and way, producing a
//! corrupt index and burning many extra hours of build time.
//!
//! Two flavours of mismatch worth catching:
//!
//!   1. The operator just ran `--region planet` once and is now
//!      switching to `--region all-continents` (or vice versa).
//!   2. A backup-restore put the wrong-pattern PBFs in the directory.
//!
//! Either way the right answer is to bail with a concrete remediation,
//! not to merge them and corrupt the index.

use std::fmt;
use std::path::{Path, PathBuf};

use super::region::Region;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    PlanetExistsButContinentsRequested {
        planet_path: PathBuf,
    },
    ContinentsExistButPlanetRequested {
        continent_paths: Vec<PathBuf>,
    },
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mismatch::PlanetExistsButContinentsRequested { planet_path } => {
                write!(
                    f,
                    "Error: {} exists from an earlier --region planet fetch.\n\n\
                     Mixing planet + continent PBFs in the same directory would cause\n\
                     build-index to double-process every node and way (corrupt index +\n\
                     much longer build). Resolve before re-running:\n\n\
                       - To keep the existing planet PBF, re-run with --region planet:\n\
                             fetch-data --region planet\n\n\
                       - To switch to the parallel continent fetch, remove the planet PBF first:\n\
                             rm {}\n\
                             # then re-run with --region all-continents\n",
                    planet_path.display(),
                    planet_path.display(),
                )
            }
            Mismatch::ContinentsExistButPlanetRequested { continent_paths } => {
                let listed: String = continent_paths
                    .iter()
                    .map(|p| format!("    {}\n", p.display()))
                    .collect();
                let parent = continent_paths
                    .first()
                    .and_then(|p| p.parent())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<pbf>".to_string());
                write!(
                    f,
                    "Error: continent PBFs from an earlier --region all-continents fetch:\n\
                     {listed}\n\
                     Adding planet-latest.osm.pbf alongside these would cause build-index\n\
                     to double-process. Resolve before re-running:\n\n\
                       - To keep the continent PBFs, re-run with --region all-continents:\n\
                             fetch-data --region all-continents\n\n\
                       - To switch to the single-stream planet PBF:\n\
                             rm {parent}/*-latest.osm.pbf\n\
                             # then re-run with --region planet\n",
                )
            }
        }
    }
}

impl std::error::Error for Mismatch {}

/// Inspect `<dir>` for `*-latest.osm.pbf` files; flag mismatches
/// against the requested target region. Empty directory or matching
/// pattern returns `Ok(())`.
pub fn check_pbf_directory_consistency(dir: &Path, target: Region) -> Result<(), Mismatch> {
    if !dir.exists() {
        return Ok(());
    }

    let mut planet_path: Option<PathBuf> = None;
    let mut continent_paths: Vec<PathBuf> = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // If we can't read the dir, treat it as empty — the fetch
        // attempt that follows will surface the I/O error with more
        // context than we can here.
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with("-latest.osm.pbf") {
            continue;
        }
        if name == "planet-latest.osm.pbf" {
            planet_path = Some(path);
        } else {
            continent_paths.push(path);
        }
    }

    // Sort so the diagnostic output is deterministic across filesystems.
    continent_paths.sort();

    let want_continent_pattern = target.is_all_continents()
        || matches!(
            target,
            Region::Africa
                | Region::Antarctica
                | Region::Asia
                | Region::AustraliaOceania
                | Region::CentralAmerica
                | Region::Europe
                | Region::NorthAmerica
                | Region::Russia
                | Region::SouthAmerica
                | Region::Australia
                | Region::NewZealand
                | Region::Niue
                | Region::Usa
        );
    let want_planet_pattern = target.is_planet();

    if want_continent_pattern && planet_path.is_some() {
        return Err(Mismatch::PlanetExistsButContinentsRequested {
            planet_path: planet_path.unwrap(),
        });
    }
    if want_planet_pattern && !continent_paths.is_empty() {
        return Err(Mismatch::ContinentsExistButPlanetRequested { continent_paths });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        File::create(&p).expect("touch");
        p
    }

    #[test]
    fn empty_directory_passes_for_any_target() {
        let tmp = tempfile::tempdir().unwrap();
        for target in [Region::Planet, Region::AllContinents, Region::AustraliaOceania] {
            assert_eq!(check_pbf_directory_consistency(tmp.path(), target), Ok(()));
        }
    }

    #[test]
    fn planet_only_matches_planet_target() {
        let tmp = tempfile::tempdir().unwrap();
        let planet = touch(tmp.path(), "planet-latest.osm.pbf");
        assert_eq!(
            check_pbf_directory_consistency(tmp.path(), Region::Planet),
            Ok(())
        );
        // ...but rejects when we're asked for continents.
        let err = check_pbf_directory_consistency(tmp.path(), Region::AllContinents).unwrap_err();
        assert_eq!(
            err,
            Mismatch::PlanetExistsButContinentsRequested {
                planet_path: planet
            }
        );
    }

    #[test]
    fn continents_only_matches_continent_target() {
        let tmp = tempfile::tempdir().unwrap();
        let _eu = touch(tmp.path(), "europe-latest.osm.pbf");
        let _na = touch(tmp.path(), "north-america-latest.osm.pbf");
        assert_eq!(
            check_pbf_directory_consistency(tmp.path(), Region::AllContinents),
            Ok(())
        );
        assert_eq!(
            check_pbf_directory_consistency(tmp.path(), Region::Europe),
            Ok(())
        );
        // ...but rejects when we're asked for planet.
        let err = check_pbf_directory_consistency(tmp.path(), Region::Planet).unwrap_err();
        match err {
            Mismatch::ContinentsExistButPlanetRequested { continent_paths } => {
                assert_eq!(continent_paths.len(), 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn nonexistent_directory_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("nonexistent");
        assert_eq!(
            check_pbf_directory_consistency(&nonexistent, Region::Planet),
            Ok(())
        );
    }

    #[test]
    fn unrelated_files_ignored() {
        // Sidecars, partial files, the .etag/.md5 pattern — none of
        // these should trip the mismatch check.
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "europe-latest.osm.pbf.etag");
        touch(tmp.path(), "europe-latest.osm.pbf.md5");
        touch(tmp.path(), "europe-latest.osm.pbf.partial");
        touch(tmp.path(), "europe-latest.osm.pbf.state.txt");
        touch(tmp.path(), "README.md");
        assert_eq!(
            check_pbf_directory_consistency(tmp.path(), Region::Planet),
            Ok(())
        );
    }

    #[test]
    fn display_remediation_includes_path() {
        let m = Mismatch::PlanetExistsButContinentsRequested {
            planet_path: PathBuf::from("/data/pbf/planet-latest.osm.pbf"),
        };
        let s = format!("{m}");
        assert!(s.contains("/data/pbf/planet-latest.osm.pbf"));
        assert!(s.contains("--region planet"));
        assert!(s.contains("--region all-continents"));
    }
}
