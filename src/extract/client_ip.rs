//! Look up the IP address of client.

use std::{
    fmt::Display,
    net::{AddrParseError, IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::{FromStr, Utf8Error},
};

use crate::{extract::Extractor, header, header::HeaderName, Request};

use std::error::Error as StdError;

impl_error!(
    MissingRemoteAddr,
    "Missing remote addr, maybe it's not a tcp/udp connection",
    "This error occurs when the remote addr is missed."
);

/// Extract the apparent address of the client.
/// If the server is behind a proxy, you may obtain the proxy's address instead of the actual user's.
#[derive(Debug, Clone)]
pub struct PeerAddr(pub SocketAddr);

impl Extractor for PeerAddr {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        Ok(Self(
            request.extensions().get::<Self>().ok_or(http_kit::Error::msg("Missing remote address"))?.0,
        ))
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
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        if let Some(v) = request.headers().get(header::FORWARDED) {
            if let Some(addr) = parse_forwarded(v.as_bytes())? {
                return Ok(Self(addr));
            }
        }

        if let Some(v) = request.headers().get(HeaderName::from_static("x-forwarded-for")) {
            if let Some(addr) = parse_x_forwarded_for(v.as_bytes())? {
                return Ok(Self(addr));
            }
        }

        Ok(Self(
            request
                .extensions()
                .get::<PeerAddr>()
                .ok_or(http_kit::Error::msg("Missing remote address"))?
                .0
                .ip(),
        ))

        // It's unnecessary to consume the extension.
    }
}

/// An error occurred while extracting the client's IP.
#[derive(Debug)]
pub enum Error {
    /// The header is not syntactically valid.
    InvalidForwardedHeader,
    /// The header is not a valid UTF-8 string.
    InvalidUtf8(Utf8Error),
    /// Failed to parse the address.
    AddrParseError(AddrParseError),
}

impl From<AddrParseError> for Error {
    fn from(error: AddrParseError) -> Self {
        Self::AddrParseError(error)
    }
}

impl From<Utf8Error> for Error {
    fn from(error: Utf8Error) -> Self {
        Self::InvalidUtf8(error)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid forwarded header")
    }
}

impl StdError for Error {}

fn parse_forwarded(v: &[u8]) -> Result<Option<IpAddr>, Error> {
    for v in v.split(|b| *b == b';') {
        for v in v.split(|b| *b == b',') {
            let (mut key, mut value) = split_once(v, b'=').ok_or(Error::InvalidForwardedHeader)?;
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

fn parse_x_forwarded_for(v: &[u8]) -> Result<Option<IpAddr>, Error> {
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
    if !*s.first()? == b'[' {
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
