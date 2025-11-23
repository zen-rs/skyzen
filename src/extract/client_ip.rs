//! Look up the IP address of client.

use std::{
    net::{AddrParseError, IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::{FromStr, Utf8Error},
};

use http::StatusCode;
use http_kit::http_error;

use crate::{extract::Extractor, header, header::HeaderName, Request};

http_error!(/// Raised when the connection metadata does not expose the remote address.
pub MissingRemoteAddr,
StatusCode::INTERNAL_SERVER_ERROR, 
"Missing remote addr, maybe it's not a tcp/udp connection");

/// Extract the apparent address of the client.
/// If the server is behind a proxy, you may obtain the proxy's address instead of the actual user's.
#[derive(Debug, Clone)]
pub struct PeerAddr(pub SocketAddr);

impl Extractor for PeerAddr {
    type Error = MissingRemoteAddr;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(Self(
            request
                .extensions()
                .get::<Self>()
                .ok_or(MissingRemoteAddr::new())?
                .0,
        ))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        crate::openapi::schema_of::<Self>().map(|schema| crate::openapi::ExtractorSchema {
            content_type: None,
            schema: Some(schema),
        })
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::register_schema_for::<Self>(defs);
    }
}

/// Extract the IP address of the client.
///
/// This extractor will check for the presence of the `Forwarded` or `X-Forwarded-For` header, ensuring it works even when behind a proxy.
/// The order of determination is as follows: `Forwarded` / `X-Forwarded-For` / the peer address of the client.
/// # Warning
/// This extractor carries the risk of spoofing because the client can send a fake `Forwarded` or `X-Forwarded-For` header.
/// If you're working on something like rate limiting, consider using the `PeerAddr` extractor.
#[derive(Debug, Clone)]
pub struct ClientIp(pub IpAddr);

impl_deref!(PeerAddr, SocketAddr);

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

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        crate::openapi::schema_of::<Self>().map(|schema| crate::openapi::ExtractorSchema {
            content_type: None,
            schema: Some(schema),
        })
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::register_schema_for::<Self>(defs);
    }
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
    /// The remote address is missing.
    #[error("Missing remote addr, maybe it's not a tcp/udp connection")]
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

            if strip_once(&mut value, b"\"") {
                if let Some(value) = get_ipv6_str(value) {
                    return Ok(Some(
                        Ipv6Addr::from_str(std::str::from_utf8(value)?)?.into(),
                    ));
                }
                return Ok(Some(
                    Ipv4Addr::from_str(std::str::from_utf8(value)?)?.into(),
                ));
            }
            return Ok(Some(
                Ipv4Addr::from_str(std::str::from_utf8(value)?)?.into(),
            ));
        }
    }
    Ok(None)
}

fn parse_x_forwarded_for(v: &[u8]) -> Result<Option<IpAddr>, ClientIpError> {
    if let Some(mut v) = v.split(|v| *v == b',').next() {
        trim(&mut v);
        Ok(Some(IpAddr::from_str(std::str::from_utf8(v)?)?))
    } else {
        Ok(None)
    }
}

fn split_once(s: &[u8], pat: u8) -> Option<(&[u8], &[u8])> {
    for (i, ss) in s.iter().enumerate() {
        if *ss == pat {
            return Some((&s[..i], (s.get(i + 1..).unwrap_or(&[]))));
        }
    }
    None
}

fn trim(s: &mut &[u8]) {
    while let Some(s2) = s.strip_prefix(b" ") {
        *s = s2;
    }

    while let Some(s2) = s.strip_suffix(b" ") {
        *s = s2;
    }
}

fn strip_once(s: &mut &[u8], pat: &[u8]) -> bool {
    if let Some(s2) = s.strip_prefix(pat) {
        *s = s2;
    } else {
        return false;
    }

    if let Some(s2) = s.strip_suffix(pat) {
        *s = s2;
    } else {
        return false;
    }

    true
}

fn get_ipv6_str(s: &[u8]) -> Option<&[u8]> {
    if *s.first()? != b'[' {
        return None;
    }

    for (i, ss) in s.iter().enumerate() {
        if *ss == b']' {
            return Some(&s[1..i]);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::{net::IpAddr, str::FromStr};

    use super::{parse_forwarded, parse_x_forwarded_for};

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
}
