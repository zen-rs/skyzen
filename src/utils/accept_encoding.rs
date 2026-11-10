//! `Accept-Encoding` evaluation — RFC 9110 §12.5.3.
//!
//! One reading of the header, shared by the response [`CompressionMiddleware`] and the static
//! file server's choice among precompressed variants, so the two cannot disagree on how a
//! wildcard, a zero quality or `identity` is treated.
//!
//! [`CompressionMiddleware`]: crate::middleware::CompressionMiddleware

use http_kit::header::{HeaderMap, ACCEPT_ENCODING};
use smallvec::SmallVec;

/// How much a client wants one content coding, as its `Accept-Encoding` said it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preference {
    /// The quality value, in `(0, 1]`: a coding announced with `q=0` has no preference at all.
    pub quality: f32,
    /// Where in the header the deciding entry stood; earlier means the client listed it first.
    pub position: usize,
}

/// Whether the client accepts the response without any content coding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Identity {
    /// `identity` (or `*`) was listed with a non-zero quality.
    Listed(Preference),
    /// Nothing in the header mentions it, which leaves it acceptable (§12.5.3).
    Implied,
    /// `identity;q=0`, or `*;q=0` with no `identity` entry of its own.
    Refused,
}

/// A request's `Accept-Encoding` entries, in the order the client sent them.
#[derive(Debug)]
pub struct AcceptEncoding<'h> {
    entries: SmallVec<[(&'h str, f32); 6]>,
}

impl<'h> AcceptEncoding<'h> {
    /// Read every `Accept-Encoding` field of `headers`; a value that is not visible ASCII is
    /// skipped, as if the field had not been sent.
    pub fn from_headers(headers: &'h HeaderMap) -> Self {
        let entries = headers
            .get_all(ACCEPT_ENCODING)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(parse_part)
            .collect();
        Self { entries }
    }

    /// The client's preference for the coding spelled by any of `aliases`, or `None` when it is
    /// not acceptable.
    ///
    /// A token the client listed decides on its own — the first occurrence wins, and a `q=0`
    /// shadows a wildcard; `*` speaks only for codings the client did not name.
    pub fn preference(&self, aliases: &[&str]) -> Option<Preference> {
        self.find(|token| {
            aliases
                .iter()
                .any(|alias| token.eq_ignore_ascii_case(alias))
        })
        .or_else(|| self.find(|token| token == "*"))
        .and_then(|(quality, position)| (quality > 0.0).then_some(Preference { quality, position }))
    }

    /// Whether the unencoded representation is acceptable.
    pub fn identity(&self) -> Identity {
        let named = self.find(|token| token.eq_ignore_ascii_case("identity"));
        match named.or_else(|| self.find(|token| token == "*")) {
            Some((quality, position)) if quality > 0.0 => {
                Identity::Listed(Preference { quality, position })
            }
            Some(_) => Identity::Refused,
            None => Identity::Implied,
        }
    }

    fn find(&self, matches: impl Fn(&str) -> bool) -> Option<(f32, usize)> {
        self.entries
            .iter()
            .enumerate()
            .find(|(_, (token, _))| matches(token))
            .map(|(position, (_, quality))| (*quality, position))
    }
}

/// One `coding;q=value` entry. A quality that does not parse, or lies outside `[0, 1]`, counts as
/// `0` — the entry is kept so it still shadows a wildcard, but it never selects anything.
fn parse_part(part: &str) -> (&str, f32) {
    let mut sections = part.split(';');
    let token = sections.next().unwrap_or_default().trim();
    let quality = sections
        .filter_map(|parameter| parameter.split_once('='))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("q"))
        .map_or(1.0, |(_, value)| {
            value
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|quality| (0.0..=1.0).contains(quality))
                .unwrap_or(0.0)
        });
    (token, quality)
}

#[cfg(test)]
mod tests {
    use super::{AcceptEncoding, Identity, Preference};
    use http_kit::header::{HeaderMap, HeaderValue, ACCEPT_ENCODING};

    fn headers(values: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(ACCEPT_ENCODING, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn a_named_coding_carries_its_quality_and_position() {
        let headers = headers(&["gzip;q=0.5, br"]);
        let accept = AcceptEncoding::from_headers(&headers);
        assert_eq!(
            accept.preference(&["br"]),
            Some(Preference {
                quality: 1.0,
                position: 1
            })
        );
        assert_eq!(accept.preference(&["gzip", "x-gzip"]).unwrap().quality, 0.5);
        assert_eq!(accept.preference(&["zstd"]), None);
    }

    #[test]
    fn a_zero_quality_shadows_the_wildcard() {
        let headers = headers(&["*;q=0.8", "br;q=0"]);
        let accept = AcceptEncoding::from_headers(&headers);
        assert_eq!(accept.preference(&["br"]), None);
        assert_eq!(accept.preference(&["zstd"]).unwrap().quality, 0.8);
    }

    #[test]
    fn identity_is_implied_listed_or_refused() {
        assert_eq!(
            AcceptEncoding::from_headers(&headers(&[])).identity(),
            Identity::Implied
        );
        assert_eq!(
            AcceptEncoding::from_headers(&headers(&["gzip"])).identity(),
            Identity::Implied
        );
        assert_eq!(
            AcceptEncoding::from_headers(&headers(&["identity;q=0"])).identity(),
            Identity::Refused
        );
        assert_eq!(
            AcceptEncoding::from_headers(&headers(&["*;q=0"])).identity(),
            Identity::Refused
        );
        // A named `identity` overrides the wildcard that would refuse it.
        assert!(matches!(
            AcceptEncoding::from_headers(&headers(&["*;q=0, identity;q=0.1"])).identity(),
            Identity::Listed(_)
        ));
    }

    #[test]
    fn an_invalid_quality_counts_as_zero() {
        let headers = headers(&["br;q=2, gzip;q=abc"]);
        let accept = AcceptEncoding::from_headers(&headers);
        assert_eq!(accept.preference(&["br"]), None);
        assert_eq!(accept.preference(&["gzip"]), None);
    }
}
