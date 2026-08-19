//! Look up the IP address of client.

use std::{
    net::{AddrParseError, IpAddr, Ipv6Addr},
    str::{FromStr, Utf8Error},
};

use http::StatusCode;

use crate::{extract::Extractor, header, header::HeaderName, Request};

/// The remote peer address of the connection.
///
/// Re-exported from [`skyzen_core`] so that server backends can populate it without depending on
/// the top-level `skyzen` crate. If the server is behind a proxy this is the proxy's address; use
/// [`ClientIp`] to honour `Forwarded`/`X-Forwarded-For`.
pub use skyzen_core::{MissingRemoteAddr, PeerAddr};

/// Extract the IP address of the client.
///
/// This extractor will check for the presence of the `Forwarded` or `X-Forwarded-For` header, ensuring it works even when behind a proxy.
/// The order of determination is as follows: `Forwarded` / `X-Forwarded-For` / the peer address of the client.
/// # Warning
/// This extractor carries the risk of spoofing because the client can send a fake `Forwarded` or `X-Forwarded-For` header.
/// If you're working on something like rate limiting, consider using the `PeerAddr` extractor.
#[derive(Debug, Clone)]
pub struct ClientIp(pub IpAddr);

impl_deref!(ClientIp, IpAddr);

impl Extractor for ClientIp {
    type Error = ClientIpError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        if let Some(v) = request.headers().get(header::FORWARDED) {
            if let Some(addr) = parse_forwarded(v.as_bytes())? {
                return Ok(Self(addr));
            }
        }

        if let Some(v) = request
            .headers()
            .get(HeaderName::from_static("x-forwarded-for"))
        {
            if let Some(addr) = parse_x_forwarded_for(v.as_bytes())? {
                return Ok(Self(addr));
            }
        }

        Ok(Self(
            request
                .extensions()
                .get::<PeerAddr>()
                .ok_or(ClientIpError::MissingRemoteAddr)?
                .0
                .ip(),
        ))

        // It's unnecessary to consume the extension.
    }

    // The client IP is derived server-side from connection metadata and proxy headers, so it is
    // not a client-supplied request parameter and contributes no OpenAPI schema.
}

/// An error occurred while extracting the client's IP.

#[skyzen::error(status = StatusCode::BAD_REQUEST)]
pub enum ClientIpError {
    /// The header is not syntactically valid.
    #[error("Invalid forwarded header")]
    InvalidForwardedHeader,
    /// The header is not a valid UTF-8 string.
    #[error("Invalid UTF-8 in forwarded header")]
    InvalidUtf8(#[from] Utf8Error),
    #[error("Failed to parse address")]
    /// Failed to parse the address.
    AddrParseError(#[from] AddrParseError),
    /// The remote address is missing. This points at a server configuration
    /// problem (the backend did not record a peer address), not a bad request.
    #[error(
        "Missing remote addr, maybe it's not a tcp/udp connection",
        status = StatusCode::INTERNAL_SERVER_ERROR
    )]
    MissingRemoteAddr,
}

fn parse_forwarded(v: &[u8]) -> Result<Option<IpAddr>, ClientIpError> {
    for v in v.split(|b| *b == b';') {
        for v in v.split(|b| *b == b',') {
            let (mut key, mut value) =
                split_once(v, b'=').ok_or(ClientIpError::InvalidForwardedHeader)?;
            trim(&mut key);

            if !key.eq_ignore_ascii_case(b"for") {
                continue;
            }

            trim(&mut value);
            return Ok(Some(parse_forwarded_for_value(value)?));
        }
    }
    Ok(None)
}

fn parse_forwarded_for_value(mut value: &[u8]) -> Result<IpAddr, ClientIpError> {
    trim(&mut value);
    if let Some(unquoted) = value.strip_prefix(b"\"") {
        let Some(unquoted) = unquoted.strip_suffix(b"\"") else {
            return Err(ClientIpError::InvalidForwardedHeader);
        };
        return parse_ip_addr_with_optional_port(unquoted);
    }
    parse_ip_addr_with_optional_port(value)
}

fn parse_ip_addr_with_optional_port(value: &[u8]) -> Result<IpAddr, ClientIpError> {
    let value = std::str::from_utf8(value)?;

    if let Some(ipv6) = parse_bracketed_ipv6(value)? {
        return Ok(ipv6.into());
    }

    match IpAddr::from_str(value) {
        Ok(addr) => Ok(addr),
        Err(parse_err) => {
            if let Some((host, port)) = value.rsplit_once(':') {
                if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
                    return Ok(IpAddr::from_str(host)?);
                }
            }
            Err(parse_err.into())
        }
    }
}

fn parse_bracketed_ipv6(value: &str) -> Result<Option<Ipv6Addr>, ClientIpError> {
    if !value.starts_with('[') {
        return Ok(None);
    }

    let Some(end_bracket) = value.find(']') else {
        return Err(ClientIpError::InvalidForwardedHeader);
    };

    let host = &value[1..end_bracket];
    let remainder = &value[end_bracket + 1..];

    if !remainder.is_empty() {
        let Some(port) = remainder.strip_prefix(':') else {
            return Err(ClientIpError::InvalidForwardedHeader);
        };
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ClientIpError::InvalidForwardedHeader);
        }
    }

    Ok(Some(Ipv6Addr::from_str(host)?))
}

fn parse_x_forwarded_for(v: &[u8]) -> Result<Option<IpAddr>, ClientIpError> {
    // `split` always yields at least one (possibly empty) chunk.
    let mut first = v.split(|v| *v == b',').next().unwrap_or_default();
    trim(&mut first);
    Ok(Some(IpAddr::from_str(std::str::from_utf8(first)?)?))
}

fn split_once(s: &[u8], pat: u8) -> Option<(&[u8], &[u8])> {
    for (i, ss) in s.iter().enumerate() {
        if *ss == pat {
            return Some((&s[..i], (s.get(i + 1..).unwrap_or(&[]))));
        }
    }
    None
}

/// Strip optional whitespace (spaces and horizontal tabs) from both ends.
const fn trim(s: &mut &[u8]) {
    while let Some((first, rest)) = s.split_first() {
        if matches!(first, b' ' | b'\t') {
            *s = rest;
        } else {
            break;
        }
    }

    while let Some((last, rest)) = s.split_last() {
        if matches!(last, b' ' | b'\t') {
            *s = rest;
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::IpAddr, str::FromStr};

    use super::{parse_forwarded, parse_x_forwarded_for, ClientIp, ClientIpError};
    use crate::{Body, Method, Request};
    use http_kit::header::HeaderValue;
    use skyzen_core::Extractor;

    #[test]
    fn test_forwarded_1() {
        let addr=parse_forwarded(b" for =192.0.2.43;  proto=https, for=198.51.100.17 ;by=\"[::1]:1234\";host=\"example.com\"").unwrap().unwrap();
        assert_eq!(addr, IpAddr::from([192, 0, 2, 43]));
    }

    #[test]
    fn test_forwarded_2() {
        let addr = parse_forwarded(b"For=\"[2001:db8:cafe::17]:4711\"")
            .unwrap()
            .unwrap();
        assert_eq!(addr, IpAddr::from_str("2001:db8:cafe::17").unwrap());
    }

    #[test]
    fn test_forwarded_3() {
        let addr = parse_forwarded(b"  fOr =\"192.0.2.55\" ,  for =   \"192.0.2.43\" ,")
            .unwrap()
            .unwrap();
        assert_eq!(addr, IpAddr::from([192, 0, 2, 55]));
    }

    #[test]
    fn test_forwarded_with_ipv4_port() {
        let addr = parse_forwarded(b"for=198.51.100.17:443").unwrap().unwrap();
        assert_eq!(addr, IpAddr::from([198, 51, 100, 17]));
    }

    #[test]
    fn test_forwarded_with_unquoted_ipv6_port() {
        let addr = parse_forwarded(b"for=[2001:db8:cafe::17]:4711")
            .unwrap()
            .unwrap();
        assert_eq!(addr, IpAddr::from_str("2001:db8:cafe::17").unwrap());
    }

    #[test]
    fn test_x_forwarded_for_1() {
        let addr = parse_x_forwarded_for(b"192.0.2.21,192.0.2.34,2001:db8:cafe::17")
            .unwrap()
            .unwrap();
        assert_eq!(addr, IpAddr::from([192, 0, 2, 21]));
    }

    #[test]
    fn test_x_forwarded_for_2() {
        let addr = parse_x_forwarded_for(b" 2001:db8:cafe::17, 192.0.2.21")
            .unwrap()
            .unwrap();
        assert_eq!(addr, IpAddr::from_str("2001:db8:cafe::17").unwrap());
    }

    #[tokio::test]
    async fn rejects_invalid_forwarded_header() {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        request
            .headers_mut()
            .insert(crate::header::FORWARDED, HeaderValue::from_static("for"));

        let error = ClientIp::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, ClientIpError::InvalidForwardedHeader));
    }

    #[tokio::test]
    async fn rejects_unparsable_forwarded_address() {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        request.headers_mut().insert(
            crate::header::FORWARDED,
            HeaderValue::from_static("for=_hidden"),
        );

        let error = ClientIp::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, ClientIpError::AddrParseError(_)));
    }
}
