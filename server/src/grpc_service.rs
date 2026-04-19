//! gRPC front-end over the same `Index` / `Forward` / `Autocomplete` /
//! `IpGeo` stack the REST API uses. Messages mirror the REST JSON shape
//! so clients can pick the wire protocol without semantic drift.

use crate::ip_geo::IpGeo;
use crate::{Address as NativeAddress, AddressDetails as NativeAddressDetails, Index};
use arc_swap::ArcSwap;
use std::sync::Arc;
use tonic::{Request, Response, Status};

#[cfg(feature = "forward")]
use crate::autocomplete::Autocomplete;
#[cfg(feature = "forward")]
use crate::forward::{self as fwd, Forward};

pub mod proto {
    tonic::include_proto!("geocoder.v1");
}

pub use proto::geocoder_server::{Geocoder, GeocoderServer};
use proto::{
    Address as PbAddress, AddressDetails as PbAddressDetails, AddressResponse,
    AutocompleteHit as PbAutocompleteHit, AutocompleteRequest, AutocompleteResponse,
    IpGeocodeRequest, IpGeocodeResponse, ReverseRequest, SearchHit as PbSearchHit, SearchRequest,
    SearchResponse, ValidateRequest, ValidateResponse,
};

/// Shared handler state. Mirrors what the REST handlers have access to,
/// minus auth (gRPC auth is typically done via interceptor in front of
/// this — an incremental follow-up, not a blocker for the service
/// surface).
pub struct GeocoderService {
    pub index: Arc<ArcSwap<Index>>,
    #[cfg(feature = "forward")]
    pub forward: Option<Arc<Forward>>,
    #[cfg(feature = "forward")]
    pub autocomplete: Option<Arc<Autocomplete>>,
    pub ip_db: Option<Arc<IpGeo>>,
}

#[tonic::async_trait]
impl Geocoder for GeocoderService {
    async fn reverse(
        &self,
        req: Request<ReverseRequest>,
    ) -> Result<Response<AddressResponse>, Status> {
        let r = req.into_inner();
        let snap = self.index.load();
        let address = snap.query(r.lat, r.lon);
        let _ = r.lang; // accepted, not yet honoured — mirrors REST
        Ok(Response::new(AddressResponse {
            address: Some(into_pb_address(address)),
        }))
    }

    #[cfg(feature = "forward")]
    async fn search(
        &self,
        req: Request<SearchRequest>,
    ) -> Result<Response<SearchResponse>, Status> {
        let r = req.into_inner();
        let Some(fwd) = self.forward.as_ref() else {
            return Err(Status::unimplemented("forward index not built"));
        };

        let kind_filter = match r.kind.as_str() {
            "place" => Some(fwd::KIND_PLACE),
            "street" => Some(fwd::KIND_STREET),
            "" => None,
            other => {
                return Err(Status::invalid_argument(format!(
                    "invalid kind {other:?}; expected 'place' or 'street'"
                )))
            }
        };
        let limit = match r.limit {
            0 => 10,
            n => n.min(50) as usize,
        };

        let housenumber = if r.housenumber.is_empty() {
            fwd::parse_freeform_query(&r.q).house_number
        } else {
            Some(r.housenumber.clone())
        };

        let structured = fwd::StructuredQuery {
            q: empty_to_none(&r.q),
            street: empty_to_none(&r.street),
            city: empty_to_none(&r.city),
            state: empty_to_none(&r.state),
            country_code: empty_to_none(&r.country_code),
            kind: kind_filter,
            limit,
        };
        let hits = fwd
            .search_structured(structured)
            .map_err(|e| Status::internal(format!("search: {e}")))?;

        let snap = self.index.load();
        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            let cc_bytes = parse_cc(&r.country_code);
            let (lat, lon, matched_hn) = if let Some(hn) = housenumber.as_deref() {
                match snap.find_addr_point_in_country(
                    hn,
                    Some(&hit.name),
                    hit.lat,
                    hit.lng,
                    cc_bytes.as_ref(),
                ) {
                    Some(m) => (m.lat, m.lng, Some(m.housenumber.to_owned())),
                    None => (hit.lat, hit.lng, None),
                }
            } else {
                (hit.lat, hit.lng, None)
            };
            let enriched = snap.query(lat, lon);
            let details = into_pb_details(&enriched.address, matched_hn.as_deref());
            out.push(PbSearchHit {
                name: hit.name,
                kind: hit.kind as u32,
                rank: hit.rank as u32,
                score: hit.score,
                lat,
                lon,
                display_name: enriched.display_name.unwrap_or_default(),
                address: Some(details),
            });
        }
        Ok(Response::new(SearchResponse { results: out }))
    }

    #[cfg(not(feature = "forward"))]
    async fn search(
        &self,
        _req: Request<SearchRequest>,
    ) -> Result<Response<SearchResponse>, Status> {
        Err(Status::unimplemented(
            "build with the `forward` feature to enable Search",
        ))
    }

    #[cfg(feature = "forward")]
    async fn validate(
        &self,
        req: Request<ValidateRequest>,
    ) -> Result<Response<ValidateResponse>, Status> {
        let r = req.into_inner();
        let Some(fwd) = self.forward.as_ref() else {
            return Err(Status::unimplemented("forward index not built"));
        };
        if r.street.is_empty() || r.city.is_empty() || r.country_code.is_empty() {
            return Err(Status::invalid_argument(
                "street, city, country_code are required",
            ));
        }

        let structured = fwd::StructuredQuery {
            street: Some(&r.street),
            city: Some(&r.city),
            state: empty_to_none(&r.state),
            country_code: Some(&r.country_code),
            kind: Some(fwd::KIND_STREET),
            limit: 5,
            ..Default::default()
        };
        let hits = fwd
            .search_structured(structured)
            .map_err(|e| Status::internal(format!("search: {e}")))?;
        let Some(top) = hits.first() else {
            return Ok(Response::new(ValidateResponse {
                verified: false,
                confidence: "fallback".into(),
                ..Default::default()
            }));
        };

        let snap = self.index.load();
        let cc_bytes = parse_cc(&r.country_code);
        let (lat, lon, verified, confidence) = if !r.housenumber.is_empty() {
            match snap.find_addr_point_in_country(
                &r.housenumber,
                Some(&top.name),
                top.lat,
                top.lng,
                cc_bytes.as_ref(),
            ) {
                Some(m) => (m.lat, m.lng, true, "exact".to_string()),
                None => (
                    top.lat,
                    top.lng,
                    false,
                    "fallback: street found, house not in index".to_string(),
                ),
            }
        } else {
            (top.lat, top.lng, false, "interpolated".to_string())
        };
        let canonical = snap.query(lat, lon);
        Ok(Response::new(ValidateResponse {
            verified,
            confidence,
            lat,
            lon,
            normalized: Some(into_pb_address(canonical)),
        }))
    }

    #[cfg(not(feature = "forward"))]
    async fn validate(
        &self,
        _req: Request<ValidateRequest>,
    ) -> Result<Response<ValidateResponse>, Status> {
        Err(Status::unimplemented(
            "build with the `forward` feature to enable Validate",
        ))
    }

    #[cfg(feature = "forward")]
    async fn autocomplete(
        &self,
        req: Request<AutocompleteRequest>,
    ) -> Result<Response<AutocompleteResponse>, Status> {
        let r = req.into_inner();
        let Some(a) = self.autocomplete.as_ref() else {
            return Err(Status::unimplemented("autocomplete FST not built"));
        };
        let limit = match r.limit {
            0 => 10,
            n => n.min(50) as usize,
        };
        let hits = if let Some(cc) = parse_cc(&r.country_code) {
            a.search(&[cc[0].to_ascii_lowercase(), cc[1].to_ascii_lowercase()], &r.q, limit)
        } else {
            a.search_any(&r.q, limit)
        };
        let out = hits
            .into_iter()
            .map(|h| PbAutocompleteHit {
                name: h.name,
                suburb: h.suburb.unwrap_or_default(),
                kind: h.kind as u32,
                rank: h.rank as u32,
                lat: h.lat,
                lon: h.lng,
            })
            .collect();
        Ok(Response::new(AutocompleteResponse { results: out }))
    }

    #[cfg(not(feature = "forward"))]
    async fn autocomplete(
        &self,
        _req: Request<AutocompleteRequest>,
    ) -> Result<Response<AutocompleteResponse>, Status> {
        Err(Status::unimplemented(
            "build with the `forward` feature to enable Autocomplete",
        ))
    }

    async fn ip_geocode(
        &self,
        req: Request<IpGeocodeRequest>,
    ) -> Result<Response<IpGeocodeResponse>, Status> {
        // Snapshot the peer address before consuming the request into its
        // inner message — tonic's Request::into_inner takes self.
        let peer = req.remote_addr();
        let r = req.into_inner();
        let Some(db) = self.ip_db.as_ref() else {
            return Err(Status::unavailable("ip geocoding not enabled"));
        };
        let ip: std::net::IpAddr = if r.ip.is_empty() {
            peer.map(|sa| sa.ip()).ok_or_else(|| {
                Status::invalid_argument("ip is required when no peer address is available")
            })?
        } else {
            r.ip
                .parse()
                .map_err(|_| Status::invalid_argument(format!("invalid ip {:?}", r.ip)))?
        };
        let Some((lat, lon)) = db.lookup(ip) else {
            return Err(Status::not_found("no location for ip"));
        };
        let snap = self.index.load();
        let addr = snap.query(lat, lon);
        Ok(Response::new(IpGeocodeResponse {
            ip: ip.to_string(),
            lat,
            lon,
            address: Some(into_pb_address(addr)),
        }))
    }
}

// --- Converters ---

fn into_pb_address(src: NativeAddress<'_>) -> PbAddress {
    let confidence = src.confidence.unwrap_or("").to_string();
    PbAddress {
        display_name: src.display_name.unwrap_or_default(),
        address: Some(into_pb_details(&src.address, None)),
        confidence,
    }
}

fn into_pb_details(src: &NativeAddressDetails<'_>, override_hn: Option<&str>) -> PbAddressDetails {
    PbAddressDetails {
        house_number: override_hn
            .map(str::to_owned)
            .or_else(|| src.house_number.as_deref().map(str::to_owned))
            .unwrap_or_default(),
        road: src.road.unwrap_or("").to_string(),
        city: src.city.unwrap_or("").to_string(),
        state: src.state.unwrap_or("").to_string(),
        county: src.county.unwrap_or("").to_string(),
        postcode: src.postcode.unwrap_or("").to_string(),
        country: src.country.unwrap_or("").to_string(),
        country_code: src.country_code.clone().unwrap_or_default(),
    }
}

fn empty_to_none(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn parse_cc(s: &str) -> Option<[u8; 2]> {
    let b = s.trim().as_bytes();
    if b.len() != 2 || !b[0].is_ascii_alphabetic() || !b[1].is_ascii_alphabetic() {
        None
    } else {
        Some([b[0].to_ascii_uppercase(), b[1].to_ascii_uppercase()])
    }
}
