//! G-NAF address-point index — exact geocodes for every AU address.
//!
//! Thin wrapper around the generic `address_points::AddressPointIndex`.
//! G-NAF is AU-specific; the on-disk layout is the same format used by
//! OpenAddresses (and by any future national dataset we ingest). The
//! only reason this is a separate type at all is so we can keep G-NAF's
//! freshness path (direct from data.gov.au) distinct from OpenAddresses
//! (community aggregator) in the query-routing layer.
//!
//! # Attribution
//!
//! Derived from G-NAF © Commonwealth of Australia (Geoscape Australia),
//! licensed under CC-BY 4.0. Any redistribution must preserve attribution.

use crate::address_points::AddressPointIndex;
use std::path::Path;

// Re-export the record + match types under their historical names so
// existing call sites (builders, tests, struct_layout pin) don't change.
pub use crate::address_points::AddressMatch as GnafMatch;
pub use crate::address_points::AddressPoint as GnafPoint;

pub struct Gnaf {
    inner: AddressPointIndex,
}

impl Gnaf {
    /// Open `<dir>/gnaf_{points,cells,entries,strings}.bin`. Returns
    /// `Ok(None)` when the files aren't present — lets callers treat
    /// G-NAF as optional.
    pub fn open(dir: &Path) -> Result<Option<Self>, String> {
        Ok(AddressPointIndex::open_with_prefix_labeled(dir, "gnaf", "gnaf")?
            .map(|inner| Gnaf { inner }))
    }

    pub fn find_by_housenumber(
        &self,
        housenumber: &str,
        street_hint: Option<&str>,
        near_lat: f64,
        near_lng: f64,
        street_level: u64,
    ) -> Option<GnafMatch<'_>> {
        self.inner
            .find_by_housenumber(housenumber, street_hint, near_lat, near_lng, street_level)
    }

    pub fn find_nearest(&self, lat: f64, lng: f64, street_level: u64) -> Option<GnafMatch<'_>> {
        self.inner.find_nearest(lat, lng, street_level)
    }
}
