//! The header rewriting a gateway owes the messages it forwards: hop-by-hop fields removed
//! (RFC 9110 §7.6.1), `Via` recorded (RFC 9110 §7.6.3) and the client's hop recorded in `Forwarded`
//! (RFC 7239), with the client-written forwarding fields kept only from a trusted proxy.

use core::fmt::{self, Display, Formatter, Write as _};
use std::net::IpAddr;

use http::Version;

use crate::header::{self, HeaderMap, HeaderName, HeaderValue};

/// Fields a recorded proxy chain is carried in, every one of which a client can write as easily as
/// a proxy. From a peer the trusted-proxy settings do not vouch for, they are removed before the
/// proxy records its own hop; from a trusted one they are kept and extended.
const FORWARDING_FIELDS: [HeaderName; 6] = [
    header::FORWARDED,
    HeaderName::from_static("x-forwarded-for"),
    HeaderName::from_static("x-forwarded-host"),
    HeaderName::from_static("x-forwarded-proto"),
    HeaderName::from_static("x-real-ip"),
    HeaderName::from_static("cf-connecting-ip"),
];

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The hop-by-hop fields RFC 9110 §7.6.1 names, beside whatever `Connection` nominates.
const HOP_BY_HOP: [HeaderName; 6] = [
    header::CONNECTION,
    HeaderName::from_static("proxy-connection"),
    HeaderName::from_static("keep-alive"),
    header::TE,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// The comma-separated list elements of every `name` field, trimmed, empty ones skipped
/// (RFC 9110 §5.6.1).
fn list_elements<'a>(headers: &'a HeaderMap, name: &HeaderName) -> impl Iterator<Item = &'a [u8]> {
    headers
        .get_all(name)
        .iter()
        .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
        .map(<[u8]>::trim_ascii)
        .filter(|element| !element.is_empty())
}

/// Whether `headers` list `token` in `name`, ignoring case and any parameters after a `;`.
fn lists_token(headers: &HeaderMap, name: &HeaderName, token: &str) -> bool {
    list_elements(headers, name).any(|element| {
        let bare = element
            .split(|byte| *byte == b';')
            .next()
            .unwrap_or_default()
            .trim_ascii();
        bare.eq_ignore_ascii_case(token.as_bytes())
    })
}

/// Whether the message asks to switch protocols: an HTTP/1.1 `Upgrade` the `Connection` field
/// nominates (RFC 9110 §7.8).
pub(super) fn requests_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE) && lists_token(headers, &header::CONNECTION, "upgrade")
}

/// Whether the client said it accepts trailer fields, for itself and anything behind it
/// (RFC 9110 §10.1.4).
pub(super) fn accepts_trailers(headers: &HeaderMap) -> bool {
    lists_token(headers, &header::TE, "trailers")
}

/// Whether the message body is chunked, and so may end in a trailer section (RFC 9112 §7.1).
pub(super) fn is_chunked(headers: &HeaderMap) -> bool {
    lists_token(headers, &header::TRANSFER_ENCODING, "chunked")
}

/// Remove every hop-by-hop field: the ones RFC 9110 §7.6.1 names and the ones `Connection`
/// nominates.
pub(super) fn remove_hop_by_hop(headers: &mut HeaderMap) {
    // A nominated name that is not a valid field name cannot name a field the message carries.
    let nominated: Vec<HeaderName> = list_elements(headers, &header::CONNECTION)
        .filter_map(|token| HeaderName::from_bytes(token).ok())
        .collect();
    for name in nominated.iter().chain(HOP_BY_HOP.iter()) {
        headers.remove(name);
    }
}

/// Fold the `Cookie` fields of an HTTP/2 or HTTP/3 request into the one field HTTP/1.1 allows,
/// joined with `"; "` (RFC 9113 §8.2.3, RFC 9114 §4.2.1).
pub(super) fn fold_cookies(headers: &mut HeaderMap) {
    let mut values = headers.get_all(header::COOKIE).iter();
    let Some(first) = values.next() else {
        return;
    };
    let mut folded = first.as_bytes().to_vec();
    let mut several = false;
    for value in values {
        several = true;
        folded.extend_from_slice(b"; ");
        folded.extend_from_slice(value.as_bytes());
    }
    if several {
        let folded = HeaderValue::from_bytes(&folded)
            .expect("cookie fields joined by `; ` are still a valid field value");
        headers.insert(header::COOKIE, folded);
    }
}

/// Whether `value` is an RFC 9110 `token`.
pub(super) fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// An RFC 7239 value: a token as it is, anything else as a quoted string.
struct ForwardedValue<'a>(&'a str);

impl Display for ForwardedValue<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if is_token(self.0) {
            return f.write_str(self.0);
        }
        f.write_char('"')?;
        for character in self.0.chars() {
            if matches!(character, '"' | '\\') {
                f.write_char('\\')?;
            }
            f.write_char(character)?;
        }
        f.write_char('"')
    }
}

/// The RFC 7239 node a hop came from: its address, or `unknown` when the connection has none (a
/// unix-domain socket).
struct Node(Option<IpAddr>);

impl Display for Node {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("unknown"),
            Some(IpAddr::V4(addr)) => Display::fmt(&addr, f),
            // An IPv6 node is bracketed, and the brackets and colons make it a quoted string.
            Some(IpAddr::V6(addr)) => write!(f, "\"[{addr}]\""),
        }
    }
}

/// One `forwarded-element` (RFC 7239 §4): the hop this proxy received the request from.
pub(super) struct ForwardedElement<'a> {
    /// The connection's peer.
    pub(super) client: Option<IpAddr>,
    /// The `Host` the client asked for, when it named one.
    pub(super) host: Option<&'a str>,
    /// The scheme the client used, when the request carries it.
    pub(super) proto: Option<&'a str>,
}

impl Display for ForwardedElement<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "for={}", Node(self.client))?;
        if let Some(host) = self.host {
            write!(f, ";host={}", ForwardedValue(host))?;
        }
        if let Some(proto) = self.proto {
            write!(f, ";proto={}", ForwardedValue(proto))?;
        }
        Ok(())
    }
}

impl ForwardedElement<'_> {
    /// The element as a field value.
    pub(super) fn to_value(&self) -> HeaderValue {
        HeaderValue::try_from(self.to_string())
            .expect("a forwarded-element is rendered from visible ASCII only")
    }
}

/// One `Via` member (RFC 9110 §7.6.3): the protocol the message was received with and the
/// proxy's pseudonym.
pub(super) struct ViaMember<'a> {
    pub(super) version: Version,
    pub(super) pseudonym: &'a str,
}

impl Display for ViaMember<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        // The protocol name is left out for HTTP, leaving the version alone.
        let version = match self.version {
            Version::HTTP_09 => "0.9",
            Version::HTTP_10 => "1.0",
            Version::HTTP_11 => "1.1",
            Version::HTTP_2 => "2",
            Version::HTTP_3 => "3",
            other => unreachable!("`http` defines no HTTP version beyond 3, got {other:?}"),
        };
        write!(f, "{version} {}", self.pseudonym)
    }
}

impl ViaMember<'_> {
    /// The member as a field value.
    pub(super) fn to_value(&self) -> HeaderValue {
        HeaderValue::try_from(self.to_string())
            .expect("a Via member is a version and a validated token")
    }
}

/// Keep or drop the forwarding fields the request arrived with, then record this hop.
///
/// `trusted` says whether the trusted-proxy settings vouch for the peer. Its chain is kept and
/// extended — `X-Forwarded-For` too, when the chain already uses it, so both records keep naming
/// the same hops. From any other peer every forwarding field is a client's claim, and is removed.
pub(super) fn record_forwarding(
    headers: &mut HeaderMap,
    trusted: bool,
    element: &ForwardedElement<'_>,
) {
    if trusted {
        if headers.contains_key(&X_FORWARDED_FOR) {
            let node = element.client.map_or_else(
                || HeaderValue::from_static("unknown"),
                |client| {
                    HeaderValue::try_from(client.to_string()).expect("an address is visible ASCII")
                },
            );
            headers.append(X_FORWARDED_FOR, node);
        }
    } else {
        for name in &FORWARDING_FIELDS {
            headers.remove(name);
        }
    }
    headers.append(header::FORWARDED, element.to_value());
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;

    fn headers(fields: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in fields {
            map.append(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        map
    }

    #[test]
    fn hop_by_hop_fields_and_nominated_ones_are_removed() {
        let mut map = headers(&[
            ("connection", "keep-alive, X-Internal-Hop"),
            ("connection", "close"),
            ("keep-alive", "timeout=5"),
            ("x-internal-hop", "secret"),
            ("te", "trailers"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "h2c"),
            ("proxy-connection", "keep-alive"),
            ("accept-language", "zh-CN, ja-JP, ko-KR"),
        ]);
        remove_hop_by_hop(&mut map);
        assert_eq!(map.len(), 1, "left: {map:?}");
        assert_eq!(map["accept-language"], "zh-CN, ja-JP, ko-KR");
    }

    #[test]
    fn an_upgrade_needs_the_connection_option() {
        assert!(requests_upgrade(&headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "websocket"),
        ])));
        assert!(!requests_upgrade(&headers(&[("upgrade", "websocket")])));
    }

    #[test]
    fn te_trailers_is_read_with_parameters_and_case() {
        assert!(accepts_trailers(&headers(&[(
            "te",
            "gzip;q=0.5, Trailers"
        )])));
        assert!(!accepts_trailers(&headers(&[("te", "gzip")])));
        assert!(is_chunked(&headers(&[(
            "transfer-encoding",
            "gzip, chunked"
        )])));
    }

    #[test]
    fn cookies_of_an_h2_request_fold_into_one_field() {
        // `from_static` rejects non-ASCII, so the CJK cookies are set from bytes.
        let mut map = HeaderMap::new();
        map.insert(
            header::COOKIE,
            HeaderValue::from_bytes("城市=上海".as_bytes()).unwrap(),
        );
        map.append(header::COOKIE, HeaderValue::from_static("lang=ja"));
        map.append(
            header::COOKIE,
            HeaderValue::from_bytes("이름=민준".as_bytes()).unwrap(),
        );
        fold_cookies(&mut map);
        assert_eq!(map.get_all(header::COOKIE).iter().count(), 1);
        assert_eq!(
            map[header::COOKIE].as_bytes(),
            "城市=上海; lang=ja; 이름=민준".as_bytes()
        );
    }

    #[test]
    fn forwarded_elements_quote_what_is_not_a_token() {
        let v4 = ForwardedElement {
            client: Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 60))),
            host: Some("example.com"),
            proto: Some("https"),
        };
        assert_eq!(
            v4.to_string(),
            "for=192.0.2.60;host=example.com;proto=https"
        );

        let v6 = ForwardedElement {
            client: Some(IpAddr::V6(Ipv6Addr::new(
                0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x17,
            ))),
            host: Some("example.com:8443"),
            proto: None,
        };
        assert_eq!(
            v6.to_string(),
            "for=\"[2001:db8::17]\";host=\"example.com:8443\""
        );

        let unix = ForwardedElement {
            client: None,
            host: Some("a\"b"),
            proto: None,
        };
        assert_eq!(unix.to_string(), "for=unknown;host=\"a\\\"b\"");
    }

    #[test]
    fn via_names_the_received_version() {
        let member = ViaMember {
            version: Version::HTTP_3,
            pseudonym: "edge",
        };
        assert_eq!(member.to_value(), "3 edge");
    }

    #[test]
    fn an_untrusted_peer_loses_every_forwarding_field() {
        let mut map = headers(&[
            ("forwarded", "for=198.51.100.1"),
            ("x-forwarded-for", "198.51.100.1"),
            ("x-forwarded-host", "evil.example"),
            ("x-real-ip", "198.51.100.1"),
            ("cf-connecting-ip", "198.51.100.1"),
        ]);
        let element = ForwardedElement {
            client: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))),
            host: None,
            proto: None,
        };
        record_forwarding(&mut map, false, &element);
        assert_eq!(map.len(), 1, "left: {map:?}");
        assert_eq!(map[header::FORWARDED], "for=203.0.113.9");
    }

    #[test]
    fn a_trusted_peer_has_its_chain_extended() {
        let mut map = headers(&[
            ("forwarded", "for=198.51.100.1"),
            ("x-forwarded-for", "198.51.100.1"),
        ]);
        let element = ForwardedElement {
            client: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))),
            host: None,
            proto: None,
        };
        record_forwarding(&mut map, true, &element);
        let forwarded: Vec<_> = map.get_all(header::FORWARDED).iter().collect();
        assert_eq!(forwarded, ["for=198.51.100.1", "for=10.0.0.2"]);
        let xff: Vec<_> = map.get_all(X_FORWARDED_FOR).iter().collect();
        assert_eq!(xff, ["198.51.100.1", "10.0.0.2"]);
    }
}
