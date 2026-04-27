//! gRPC front-end over the same `Index` / `Forward` / `Autocomplete` /
//! `IpGeo` stack the REST API uses. Messages mirror the REST JSON shape
//! so clients can pick the wire protocol without semantic drift.

use crate::h3_cell;
use crate::ip_geo::IpGeo;
use crate::{Address as NativeAddress, AddressDetails as NativeAddressDetails, Index};
use arc_swap::ArcSwap;
use std::collections::HashMap;
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
    AutocompleteHit as PbAutocompleteHit, AutocompleteRequest, AutocompleteResponse, H3Request,
    H3Response, IpGeocodeRequest, IpGeocodeResponse, ReverseRequest, SearchHit as PbSearchHit,
    SearchRequest, SearchResponse, ValidateRequest, ValidateResponse,
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
    #[tracing::instrument(
        name = "grpc.reverse",
        skip_all,
        fields(
            geocoder.lat = tracing::field::Empty,
            geocoder.lon = tracing::field::Empty,
            geocoder.lang = tracing::field::Empty,
        )
    )]
    async fn reverse(
        &self,
        req: Request<ReverseRequest>,
    ) -> Result<Response<AddressResponse>, Status> {
        let r = req.into_inner();
        let span = tracing::Span::current();
        span.record("geocoder.lat", r.lat);
        span.record("geocoder.lon", r.lon);
        check_text("lang", &r.lang, crate::limits::LANG)?;
        if !r.lang.is_empty() {
            span.record("geocoder.lang", r.lang.as_str());
        }
        let h3_res = validate_h3_res(&r.h3_res)?;
        let snap = self.index.load();
        // Honour `lang` the same way the REST `/reverse` handler does
        // (main.rs `reverse_geocode`). Empty string falls through to
        // the default — `query_with_lang` treats `None` as "no
        // override" and `pack_lang_code` rejects sub-2-char tags.
        let lang = if r.lang.is_empty() { None } else { Some(r.lang.as_str()) };
        let address = snap.query_with_lang(r.lat, r.lon, lang);
        let mut pb = into_pb_address(address);
        pb.h3 = build_h3_proto(r.lat, r.lon, &h3_res);
        Ok(Response::new(AddressResponse {
            address: Some(pb),
        }))
    }

    #[cfg(feature = "forward")]
    #[tracing::instrument(
        name = "grpc.search",
        skip_all,
        fields(
            geocoder.match_count = tracing::field::Empty,
        )
    )]
    async fn search(
        &self,
        req: Request<SearchRequest>,
    ) -> Result<Response<SearchResponse>, Status> {
        let r = req.into_inner();
        check_text("q", &r.q, crate::limits::SEARCH_Q)?;
        check_text("street", &r.street, crate::limits::STRUCTURED_FIELD)?;
        check_text("housenumber", &r.housenumber, crate::limits::HOUSENUMBER)?;
        check_text("city", &r.city, crate::limits::STRUCTURED_FIELD)?;
        check_text("state", &r.state, crate::limits::STRUCTURED_FIELD)?;
        check_text("country_code", &r.country_code, crate::limits::COUNTRY_CODE_LIST)?;
        let h3_res = validate_h3_res(&r.h3_res)?;
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
        tracing::Span::current().record("geocoder.match_count", hits.len());

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
            let h3 = build_h3_proto(lat, lon, &h3_res);
            out.push(PbSearchHit {
                name: hit.name,
                kind: hit.kind as u32,
                rank: hit.rank as u32,
                score: hit.score,
                lat,
                lon,
                display_name: enriched.display_name.unwrap_or_default(),
                address: Some(details),
                h3,
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
    #[tracing::instrument(
        name = "grpc.validate",
        skip_all,
        fields(
            geocoder.country_code = tracing::field::Empty,
            geocoder.has_housenumber = tracing::field::Empty,
            geocoder.outcome = tracing::field::Empty,
        )
    )]
    async fn validate(
        &self,
        req: Request<ValidateRequest>,
    ) -> Result<Response<ValidateResponse>, Status> {
        let r = req.into_inner();
        check_text("housenumber", &r.housenumber, crate::limits::HOUSENUMBER)?;
        check_text("street", &r.street, crate::limits::STRUCTURED_FIELD)?;
        check_text("city", &r.city, crate::limits::STRUCTURED_FIELD)?;
        check_text("state", &r.state, crate::limits::STRUCTURED_FIELD)?;
        check_text("postcode", &r.postcode, crate::limits::POSTCODE)?;
        check_text("country_code", &r.country_code, crate::limits::COUNTRY_CODE_LIST)?;
        let span = tracing::Span::current();
        if !r.country_code.is_empty() {
            span.record("geocoder.country_code", r.country_code.as_str());
        }
        span.record("geocoder.has_housenumber", !r.housenumber.is_empty());
        let h3_res = validate_h3_res(&r.h3_res)?;
        let Some(fwd) = self.forward.as_ref() else {
            span.record("geocoder.outcome", "forward_disabled");
            return Err(Status::unimplemented("forward index not built"));
        };
        if r.street.is_empty() || r.city.is_empty() || r.country_code.is_empty() {
            span.record("geocoder.outcome", "missing_required_field");
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
            tracing::Span::current().record("geocoder.outcome", "street_not_found");
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
        tracing::Span::current()
            .record("geocoder.outcome", if verified { "exact" } else { "fallback" });
        let h3 = build_h3_proto(lat, lon, &h3_res);
        Ok(Response::new(ValidateResponse {
            verified,
            confidence,
            lat,
            lon,
            normalized: Some(into_pb_address(canonical)),
            h3,
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
    #[tracing::instrument(
        name = "grpc.autocomplete",
        skip_all,
        fields(
            geocoder.match_count = tracing::field::Empty,
        )
    )]
    async fn autocomplete(
        &self,
        req: Request<AutocompleteRequest>,
    ) -> Result<Response<AutocompleteResponse>, Status> {
        let r = req.into_inner();
        check_text("q", &r.q, crate::limits::AUTOCOMPLETE_Q)?;
        check_text("country_code", &r.country_code, crate::limits::COUNTRY_CODE)?;
        let h3_res = validate_h3_res(&r.h3_res)?;
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
        tracing::Span::current().record("geocoder.match_count", hits.len());
        let out = hits
            .into_iter()
            .map(|h| {
                let h3 = build_h3_proto(h.lat, h.lng, &h3_res);
                PbAutocompleteHit {
                    name: h.name,
                    suburb: h.suburb.unwrap_or_default(),
                    kind: h.kind as u32,
                    rank: h.rank as u32,
                    lat: h.lat,
                    lon: h.lng,
                    h3,
                }
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

    #[tracing::instrument(
        name = "grpc.h3",
        skip_all,
        fields(
            geocoder.lat = tracing::field::Empty,
            geocoder.lon = tracing::field::Empty,
            geocoder.h3_resolutions = tracing::field::Empty,
        )
    )]
    async fn h3(&self, req: Request<H3Request>) -> Result<Response<H3Response>, Status> {
        let r = req.into_inner();
        let span = tracing::Span::current();
        span.record("geocoder.lat", r.lat);
        span.record("geocoder.lon", r.lon);

        // Reuse the REST-side validator so the two surfaces enforce the
        // same 0..=15 + max-4 rule. The h3_res field is required here
        // (unlike the enrichment-field shape) — an empty list means the
        // request had no work to do.
        let resolutions = validate_h3_res(&r.h3_res)?;
        if resolutions.is_empty() {
            return Err(Status::invalid_argument(
                "h3_res: at least one resolution required",
            ));
        }
        span.record("geocoder.h3_resolutions", resolutions.len());

        let h3 = build_h3_proto(r.lat, r.lon, &resolutions);
        if h3.is_empty() {
            // build_h3_map returns None (→ empty proto map here) when
            // every resolution failed to resolve at the input coord.
            // NaN/infinity inputs trip this; range-out-of-bounds coords
            // get normalised by h3o so they shouldn't.
            return Err(Status::invalid_argument(
                "h3: no resolution produced a cell — check the coord (NaN/infinity rejected)",
            ));
        }

        Ok(Response::new(H3Response {
            lat: r.lat,
            lon: r.lon,
            h3,
        }))
    }

    #[tracing::instrument(
        name = "grpc.ip_geocode",
        skip_all,
        fields(
            geocoder.client_ip = tracing::field::Empty,
            geocoder.outcome = tracing::field::Empty,
        )
    )]
    async fn ip_geocode(
        &self,
        req: Request<IpGeocodeRequest>,
    ) -> Result<Response<IpGeocodeResponse>, Status> {
        // Snapshot the peer address before consuming the request into its
        // inner message — tonic's Request::into_inner takes self.
        let peer = req.remote_addr();
        let r = req.into_inner();
        check_text("ip", &r.ip, crate::limits::IP)?;
        let h3_res = validate_h3_res(&r.h3_res)?;
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
        tracing::Span::current().record("geocoder.client_ip", tracing::field::display(&ip));
        let Some((lat, lon)) = db.lookup(ip) else {
            tracing::Span::current().record("geocoder.outcome", "not_found");
            return Err(Status::not_found("no location for ip"));
        };
        tracing::Span::current().record("geocoder.outcome", "ok");
        let snap = self.index.load();
        let addr = snap.query(lat, lon);
        let h3 = build_h3_proto(lat, lon, &h3_res);
        Ok(Response::new(IpGeocodeResponse {
            ip: ip.to_string(),
            lat,
            lon,
            address: Some(into_pb_address(addr)),
            h3,
        }))
    }
}

// --- Converters ---

fn into_pb_address(src: NativeAddress<'_>) -> PbAddress {
    let confidence = src.confidence.unwrap_or("").to_string();
    // h3 is populated by callers after they know the source coord;
    // this converter doesn't have coords in scope.
    PbAddress {
        display_name: src.display_name.unwrap_or_default(),
        address: Some(into_pb_details(&src.address, None)),
        confidence,
        h3: HashMap::new(),
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

/// Apply a length cap to a single text field, mapping overflow to
/// `Status::invalid_argument`. Caps live in
/// `query_server::limits` — same set the REST handlers use, so the
/// two surfaces can't drift on what they accept.
fn check_text(name: &str, val: &str, max: usize) -> Result<(), Status> {
    crate::limits::check(name, val, max).map_err(Status::invalid_argument)
}

/// Validate H3 resolutions coming off the wire (proto carries them as
/// `uint32` but H3 cells only exist at 0–15). Mirrors the REST path's
/// `parse_h3_res` validation so the two surfaces can't diverge.
fn validate_h3_res(raw: &[u32]) -> Result<Vec<u8>, Status> {
    if raw.len() > h3_cell::MAX_RESOLUTIONS {
        return Err(Status::invalid_argument(format!(
            "h3_res: too many resolutions ({}, max {})",
            raw.len(),
            h3_cell::MAX_RESOLUTIONS
        )));
    }
    let mut out = Vec::with_capacity(raw.len());
    for &r in raw {
        if r > 15 {
            return Err(Status::invalid_argument(format!(
                "h3_res: {r} is out of range 0–15"
            )));
        }
        out.push(r as u8);
    }
    Ok(out)
}

/// Build the proto-shaped `map<uint32, string>` h3 field from a coord.
/// Returns an empty map when no resolutions were requested so the
/// default proto value round-trips cleanly.
fn build_h3_proto(lat: f64, lng: f64, resolutions: &[u8]) -> HashMap<u32, String> {
    let Some(m) = h3_cell::build_h3_map(lat, lng, resolutions) else {
        return HashMap::new();
    };
    m.into_iter()
        .filter_map(|(k, v)| k.parse::<u32>().ok().map(|r| (r, v)))
        .collect()
}

fn parse_cc(s: &str) -> Option<[u8; 2]> {
    let b = s.trim().as_bytes();
    if b.len() != 2 || !b[0].is_ascii_alphabetic() || !b[1].is_ascii_alphabetic() {
        None
    } else {
        Some([b[0].to_ascii_uppercase(), b[1].to_ascii_uppercase()])
    }
}
