//! Shared conditional-request evaluation — RFC 9110 §13.
//!
//! One implementation of the freshness and precondition rules, used by `static_files`,
//! [`Conditional`], [`Preconditions`] and [`AutoETag`] alike, so they cannot disagree on
//! precedence (`If-None-Match` before `If-Modified-Since`, `If-Match` before
//! `If-Unmodified-Since`).

use core::fmt;

use headers::{ETag, HeaderMapExt, IfModifiedSince, IfNoneMatch};
use http_kit::header::HeaderMap;
use std::time::SystemTime;

/// An `ETag` value a handler can build from anything `Display` — a revision counter, a
/// content hash, a version string.
///
/// `headers::ETag` only parses a quoted header value, which is awkward to produce by hand;
/// `EntityTag` quotes and escapes for you. Quotes and backslashes inside the value are
/// dropped rather than producing an invalid tag.
#[derive(Debug, Clone)]
pub struct EntityTag {
    etag: ETag,
}

impl EntityTag {
    /// A strong validator — safe for byte-range and byte-identity comparisons.
    ///
    /// # Panics
    /// Cannot in practice — [`sanitize`] strips every character an `ETag` forbids.
    #[must_use]
    pub fn strong(value: impl fmt::Display) -> Self {
        let raw = sanitize(&value.to_string());
        format!("\"{raw}\"")
            .parse::<ETag>()
            .map_or_else(|_| panic!("{raw:?} is a valid ETag"), |etag| Self { etag })
    }

    /// A weak validator — semantic equivalence only, no byte-range guarantees.
    ///
    /// # Panics
    /// Cannot in practice — [`sanitize`] strips every character an `ETag` forbids.
    #[must_use]
    pub fn weak(value: impl fmt::Display) -> Self {
        let raw = sanitize(&value.to_string());
        format!("W/\"{raw}\"")
            .parse::<ETag>()
            .map_or_else(|_| panic!("{raw:?} is a valid ETag"), |etag| Self { etag })
    }

    /// The `headers` crate's tag, for `TypedHeader` work.
    #[must_use]
    pub const fn etag(&self) -> &ETag {
        &self.etag
    }
}

/// `ETag` opaque values are `etagc` — visible ASCII plus `obs-text`. Anything outside that
/// charset is percent-encoded instead of dropped (`%` itself encodes first, so the mapping
/// stays injective): `a"b` and `ab` produce distinct tags, never a panic on hostile input.
fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'!' | b'#'..=b'~' if byte != b'%' && byte != b'\\' => out.push(byte as char),
            _ => {
                out.push('%');
                out.push(
                    char::from_digit(u32::from(byte >> 4), 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
                out.push(
                    char::from_digit(u32::from(byte & 0xf), 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    out
}

/// Would a `GET`/`HEAD` reuse the client's cached copy? — `true` means emit `304`.
///
/// `If-None-Match` is consulted first; `If-Modified-Since` only when the stronger validator
/// is absent, per RFC 9110 §13.2.2.
pub(crate) fn fresh(
    headers: &HeaderMap,
    etag: Option<&ETag>,
    last_modified: Option<SystemTime>,
) -> bool {
    // `If-None-Match` is the sole freshness signal when present — `If-Modified-Since` only
    // applies when the stronger validator is absent (RFC 9110 §13.2.2). With nothing to
    // compare against, no member can match, so the answer is simply "not fresh".
    if let Some(if_none_match) = headers.typed_get::<IfNoneMatch>() {
        // Without a tag, `*` fails whenever a representation exists — and reaching `fresh`
        // at all means the handler produced one, tag or no tag.
        return etag.map_or_else(
            || if_none_match == IfNoneMatch::any(),
            |tag| !if_none_match.precondition_passes(tag),
        );
    }
    if let (Some(if_modified_since), Some(modified)) =
        (headers.typed_get::<IfModifiedSince>(), last_modified)
    {
        return !if_modified_since.is_modified(modified);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{fresh, EntityTag};
    use http_kit::header::{HeaderMap, HeaderValue, IF_NONE_MATCH};

    #[test]
    fn if_none_match_short_circuits_freshness() {
        let etag = EntityTag::strong(7).etag().clone();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("\"7\""));
        assert!(fresh(&headers, Some(&etag), None));

        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("\"8\""));
        assert!(!fresh(&headers, Some(&etag), None));

        // A wildcard matches anything.
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(fresh(&headers, Some(&etag), None));
    }

    #[test]
    fn hostile_bytes_encode_into_tag_values_injectively() {
        use headers::HeaderMapExt;
        use http_kit::header::ETAG;
        let tag = EntityTag::strong("a\"b\\c");
        let mut map = HeaderMap::new();
        map.typed_insert(tag.etag().clone());
        assert_eq!(map[ETAG].to_str().expect("ascii etag"), "\"a%22b%5Cc\"");
        // Injectivity: the encoded form must not collide with a literal that looks like it.
        let mut other = HeaderMap::new();
        other.typed_insert(EntityTag::strong("a%22b%5Cc").etag().clone());
        assert_ne!(map[ETAG].to_str().unwrap(), other[ETAG].to_str().unwrap());
    }
}
