//! Cloudflare's `request.cf` — the edge metadata attached to an incoming request.
//!
//! This is much of the reason to run code at the edge at all: which colo served the request, which
//! country and city it came from, what TLS it negotiated, and what Cloudflare's bot management
//! made of it. The `WinterCG` request object carries none of it, so the wasm runtime reads the
//! Cloudflare-specific `cf` property during request conversion and puts the result into request
//! extensions for [`CfProperties`] to extract.

use core::fmt;
use core::future::{ready, Future};

use serde::Deserialize;
use skyzen_core::Extractor;

use crate::StatusCode;

/// Cloudflare's bot-management verdict for a request.
///
/// Only present on zones with Bot Management enabled, so the whole struct is optional on
/// [`CfProperties`] and every field inside it is optional too — the plan a zone is on decides which
/// signals are populated.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CfBotManagement {
    /// 1–99, where 1 is "almost certainly a bot" and 99 "almost certainly human".
    pub score: Option<u32>,
    /// Whether this is a bot Cloudflare has verified as well-behaved (a search crawler).
    pub verified_bot: Option<bool>,
    /// Whether the traffic comes from a corporate proxy.
    pub corporate_proxy: Option<bool>,
    /// Whether the request is for a static resource, which scores differently.
    pub static_resource: Option<bool>,
    /// The JA3 TLS client fingerprint.
    pub ja3_hash: Option<String>,
    /// The JA4 TLS client fingerprint.
    pub ja4: Option<String>,
    /// Identifiers of the heuristics that fired.
    pub detection_ids: Vec<u32>,
}

/// The `cf` object Cloudflare attaches to an incoming request.
///
/// # Availability
///
/// `wasm32`-only, deliberately: `request.cf` is a Cloudflare value with no native counterpart, and
/// a native stand-in would either lie about the caller's location or silently return nothing. Code
/// that reads it therefore fails to compile off the edge instead of behaving differently there.
///
/// Every field is optional because the platform populates them per zone, per plan and per
/// connection — `botManagement` needs Bot Management, `city` needs a geolocation match, and
/// `wrangler dev` supplies its own partial object. [`raw`](Self::raw) carries whatever the runtime
/// actually sent, including fields newer than this struct.
///
/// # Example
///
/// ```ignore
/// async fn where_am_i(cf: skyzen::runtime::CfProperties) -> String {
///     format!(
///         "served from {} for {}",
///         cf.colo.as_deref().unwrap_or("an unknown colo"),
///         cf.country.as_deref().unwrap_or("an unknown country"),
///     )
/// }
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CfProperties {
    /// The three-letter IATA code of the Cloudflare data centre that served the request.
    pub colo: Option<String>,
    /// ISO 3166-1 Alpha-2 country code, or `T1` for Tor.
    pub country: Option<String>,
    /// City name, when the client's location resolves to one.
    pub city: Option<String>,
    /// Region (state / province) name.
    pub region: Option<String>,
    /// ISO 3166-2 code for the region.
    pub region_code: Option<String>,
    /// Continent code (`NA`, `EU`, …).
    pub continent: Option<String>,
    /// Approximate latitude, as the platform sends it: a decimal string, not a number.
    pub latitude: Option<String>,
    /// Approximate longitude, as the platform sends it: a decimal string, not a number.
    pub longitude: Option<String>,
    /// IANA timezone name for the client's location.
    pub timezone: Option<String>,
    /// Postal code for the client's location.
    pub postal_code: Option<String>,
    /// Metro code (DMA), US only.
    pub metro_code: Option<String>,
    /// Autonomous system number the client connected from.
    pub asn: Option<u32>,
    /// Name of the organization operating that autonomous system.
    pub as_organization: Option<String>,
    /// HTTP protocol version the client negotiated (`HTTP/2`, `HTTP/3`, …).
    pub http_protocol: Option<String>,
    /// TLS version the client negotiated (`TLSv1.3`, …).
    pub tls_version: Option<String>,
    /// TLS cipher suite the client negotiated.
    pub tls_cipher: Option<String>,
    /// Cloudflare's bot-management verdict, on zones that have it.
    pub bot_management: Option<CfBotManagement>,
    /// The whole `cf` object exactly as the runtime sent it.
    ///
    /// The platform adds fields faster than any wrapper tracks them (`clientTcpRtt`,
    /// `tlsClientAuth`, `verifiedBotCategory`, …), so nothing is discarded: anything this struct
    /// does not name is still readable here.
    #[serde(skip)]
    pub raw: serde_json::Value,
}

/// `request.cf` was not readable for this request.
#[derive(Debug)]
pub struct CfPropertiesUnavailable(&'static str, Option<String>);

impl fmt::Display for CfPropertiesUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)?;
        self.1
            .as_ref()
            .map_or(Ok(()), |detail| write!(f, ": {detail}"))
    }
}

impl std::error::Error for CfPropertiesUnavailable {}

impl http_kit::HttpError for CfPropertiesUnavailable {
    fn status(&self) -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// What the wasm runtime learned about `request.cf` while converting the request.
///
/// A decode failure is kept rather than thrown away so the extractor can report *why* the value is
/// missing — a shape the platform changed reads very differently from "this host is not
/// Cloudflare", and collapsing the two would hide a real regression behind a generic 500. It is
/// also why a bad `cf` does not fail the request outright: optional edge metadata changing shape
/// should break the handlers that read it, not every route on the worker.
#[derive(Debug, Clone)]
pub struct CfPropertiesSlot(pub Result<CfProperties, String>);

impl Extractor for CfProperties {
    type Error = CfPropertiesUnavailable;

    // The properties were decoded during request conversion, so the future is ready on creation
    // rather than an `async` block with nothing to await.
    fn extract(
        request: &mut crate::Request,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(match request.extensions().get::<CfPropertiesSlot>() {
            Some(CfPropertiesSlot(Ok(properties))) => Ok(properties.clone()),
            Some(CfPropertiesSlot(Err(error))) => Err(CfPropertiesUnavailable(
                "the runtime sent a `request.cf` object this build could not decode",
                Some(error.clone()),
            )),
            None => Err(CfPropertiesUnavailable(
                "this request carried no `request.cf`; it is set by Cloudflare and absent on any \
                 other WinterCG host",
                None,
            )),
        })
    }
}

impl CfPropertiesSlot {
    /// Read `request.cf` off an incoming Workers request, ready to be put into request extensions.
    ///
    /// Returns `Ok(None)` when the runtime attached no `cf` at all, which is the normal case off
    /// Cloudflare and not an error. A `cf` that is present but undecodable is kept as the slot's
    /// `Err` — and logged here, once, for whichever runtime read it — so [`CfProperties`] can
    /// report *why* it is unavailable instead of collapsing that into "absent".
    ///
    /// Every entry point into a Worker has to do this: the main `fetch` handler, and the Durable
    /// Object glue in `skyzen-cloudflare`, which receives its own requests from the runtime and
    /// would otherwise serve them with the edge metadata silently missing.
    ///
    /// # Errors
    ///
    /// Returns the runtime's own `JsValue` if the `cf` property cannot be read off the request at
    /// all, which means the object is not the request shape this runtime documents.
    pub fn read(request: &web_sys::Request) -> Result<Option<Self>, wasm_bindgen::JsValue> {
        use wasm_bindgen::JsValue;

        let cf = js_sys::Reflect::get(request.as_ref(), &JsValue::from_str("cf"))?;
        if cf.is_undefined() || cf.is_null() {
            return Ok(None);
        }

        let properties = CfProperties::decode(cf);
        if let Err(error) = &properties {
            tracing::error!(error, "failed to decode `request.cf`");
        }
        Ok(Some(Self(properties)))
    }
}

impl CfProperties {
    /// Turn the raw `cf` value into the typed struct, keeping the whole object alongside it.
    ///
    /// Decoding runs through `serde_json::Value` rather than straight off the JS value so the
    /// untouched object survives into [`raw`](Self::raw) — and so the typed pass is plain serde,
    /// which handles the `#[serde(flatten)]`-shaped problem that `serde-wasm-bindgen` does not.
    fn decode(cf: wasm_bindgen::JsValue) -> Result<Self, String> {
        let raw: serde_json::Value = serde_wasm_bindgen::from_value(cf)
            .map_err(|error| format!("`request.cf` is not a plain JSON object: {error}"))?;
        let mut properties: Self = serde_json::from_value(raw.clone())
            .map_err(|error| format!("`request.cf` has an unexpected field type: {error}"))?;
        properties.raw = raw;
        Ok(properties)
    }
}
