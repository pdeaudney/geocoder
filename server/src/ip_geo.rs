//! Optional IP → coordinate lookup backed by MaxMind's GeoLite2-City
//! database. Loaded at server startup if `GeoLite2-City.mmdb` is found in
//! the data directory (or at a path given by `GEOLITE2_DB`). Missing DB
//! = `/geocode/ip` returns 503 gracefully.
//!
//! GeoLite2 is free but requires a signup at maxmind.com. The running
//! binary is licence-neutral — operators download and license their own
//! copy.

use maxminddb::{geoip2, Reader};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub struct IpGeo {
    reader: Reader<Vec<u8>>,
}

impl IpGeo {
    /// Open `<data_dir>/GeoLite2-City.mmdb` or the path in `GEOLITE2_DB`.
    /// Returns `Ok(None)` when neither is present so the server still
    /// starts for deployments that don't need IP geocoding.
    pub fn open(data_dir: &Path) -> Result<Option<Self>, String> {
        let path = match std::env::var("GEOLITE2_DB") {
            Ok(p) => PathBuf::from(p),
            Err(_) => data_dir.join("GeoLite2-City.mmdb"),
        };
        if !path.exists() {
            return Ok(None);
        }
        let reader = Reader::open_readfile(&path)
            .map_err(|e| format!("open {}: {}", path.display(), e))?;
        eprintln!("Loaded GeoLite2-City from {}", path.display());
        Ok(Some(IpGeo { reader }))
    }

    /// Look up an IP. Returns `(lat, lng)` when the DB has a row and a
    /// location is present; `None` when the IP is private, unresolvable,
    /// or the DB simply lacks coordinates for that subnet.
    pub fn lookup(&self, ip: IpAddr) -> Option<(f64, f64)> {
        let city: geoip2::City = self.reader.lookup(ip).ok()?;
        let loc = city.location?;
        let lat = loc.latitude?;
        let lng = loc.longitude?;
        Some((lat, lng))
    }
}
