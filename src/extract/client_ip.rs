//! Look up the IP address of client.

use core::future::{ready, Future};
use std::{
    net::{AddrParseError, IpAddr, Ipv6Addr},
    str::{FromStr, Utf8Error},
};

use http::StatusCode;
use ipnet::IpNet;

use crate::{
    extract::Extractor,
    header::{self, HeaderName},
    middleware::{Middleware, Next},
    Error, Request, Response,
};

/// The remote peer address of the connection.
///
/// Re-exported from [`skyzen_core`] so that server backends can populate it without depending on
/// the top-level `skyzen` crate. If the server is behind a proxy this is the proxy's address; use
/// [`ClientIp`] to resolve the address of the original client through trusted proxy headers.
pub use skyzen_core::{MissingRemoteAddr, PeerAddr};

const CF_CONNECTING_IP: HeaderName = HeaderName::from_static("cf-connecting-ip");

/// The Cloudflare edge network's published IPv4 ranges, for [`TrustedProxies::cloudflare`].
const CLOUDFLARE_IPV4: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
];

/// The Cloudflare edge network's published IPv6 ranges, for [`TrustedProxies::cloudflare`].
const CLOUDFLARE_IPV6: &[&str] = &[
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

/// Which proxies may set the client address through `Forwarded`/`X-Forwarded-For` headers.
///
/// Attach it as a middleware — usually with [`Route::layer`](crate::routing::Route::layer) so it
/// reaches every extractor — and [`ClientIp`] honours the proxy headers only for the hops it
/// trusts. Without it `ClientIp` reports the connection's [`PeerAddr`] and ignores the headers
/// entirely, because a client can write them as easily as a proxy can.
///
/// ```rust
/// use skyzen::{extract::TrustedProxies, routing::{CreateRouteNode, Route}};
///
/// let router = Route::new(("/".at(|| async { "ok" }),))
///     .layer(TrustedProxies::cloudflare())
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct TrustedProxies {
    nets: Vec<IpNet>,
    /// Take the client address from `CF-Connecting-IP` when the peer is a Cloudflare edge
    /// address; Cloudflare overwrites whatever the client sent, so the header cannot be spoofed
    /// through it.
    cf_connecting_ip: bool,
}

impl TrustedProxies {
    /// Trust the proxies inside `nets`. An empty list restores the default: the peer address
    /// only.
    #[must_use]
    pub fn new(nets: impl IntoIterator<Item = IpNet>) -> Self {
        Self {
            nets: nets.into_iter().collect(),
            cf_connecting_ip: false,
        }
    }

    /// Trust the published Cloudflare edge ranges and read the client address from
    /// `CF-Connecting-IP`. For a server sitting directly behind Cloudflare this is the whole
    /// configuration.
    ///
    /// # Panics
    ///
    /// Never: the ranges are fixed published literals.
    #[must_use]
    pub fn cloudflare() -> Self {
        let mut proxies = Self::new(
            CLOUDFLARE_IPV4
                .iter()
                .chain(CLOUDFLARE_IPV6)
                .map(|net| IpNet::from_str(net).expect("published ranges parse")),
        );
        proxies.cf_connecting_ip = true;
        proxies
    }

    /// Trust every hop, restoring the pre-hardening behaviour: the client address is whatever the
    /// leftmost `Forwarded`/`X-Forwarded-For` entry claims. Correct only where an outer layer —
    /// a platform that rewrites the headers on the way in, like an API gateway — guarantees the
    /// chain.
    ///
    /// # Panics
    ///
    /// Never: the ranges are fixed literals.
    #[must_use]
    pub fn any() -> Self {
        Self::new([
            IpNet::from_str("0.0.0.0/0").expect("valid"),
            IpNet::from_str("::/0").expect("valid"),
        ])
    }

    /// Whether `addr` belongs to a trusted proxy hop.
    #[must_use]
    pub fn trusts(&self, addr: &IpAddr) -> bool {
        self.nets.iter().any(|net| net.contains(addr))
    }

    /// Resolve the client from the proxy headers, walking the chains back from the trusted peer
    /// and skipping every hop that is itself trusted.
    fn forwarded_client(&self, request: &Request, peer: Option<IpAddr>) -> Option<IpAddr> {
        if self.cf_connecting_ip {
            if let Some(addr) = request
                .headers()
                .get(CF_CONNECTING_IP)
                .and_then(|value| value.to_str().ok())
                .and_then(|raw| IpAddr::from_str(raw.trim()).ok())
            {
                return Some(addr);
            }
        }

        // Order is trust-aware: a recorded peer provably appended the rightmost
        // `X-Forwarded-For` entry, while a `Forwarded` it merely passed through stays
        // client-written — so with a peer, XFF answers first and `Forwarded` cannot shadow
        // it. With no peer (serverless) `Forwarded` leads per RFC 7239, and the chain still
        // has to contain a trusted hop to count.
        if peer.is_some() {
            return x_forwarded_for_chain(request)
                .and_then(|addrs| self.anchored_client(&addrs, peer))
                .or_else(|| {
                    forwarded_chain(request).and_then(|addrs| self.anchored_client(&addrs, peer))
                });
        }
        forwarded_chain(request)
            .and_then(|addrs| self.anchored_client(&addrs, peer))
            .or_else(|| {
                x_forwarded_for_chain(request).and_then(|addrs| self.anchored_client(&addrs, peer))
            })
    }

    /// The client from a recorded chain — but only when a recorded peer exists. With no
    /// peer there is nothing to vouch for *any* entry: an attacker can forge a trusted hop
    /// as easily as a client address, so peerless `Forwarded`/`X-Forwarded-For` are
    /// unverifiable and never honored. (Platform headers like `cf-connecting-ip` answer
    /// before this point — those the host itself supplies.)
    fn anchored_client(&self, addrs: &[IpAddr], peer: Option<IpAddr>) -> Option<IpAddr> {
        peer.map(|peer| trusted_client(addrs, self, Some(peer)))
    }
}

/// The first hop not owned by a proxy, walking a recorded chain away from the server.
///
/// Each entry right-to-left is trusted only while it is itself a trusted proxy; the first
/// entry that is not is the address the trusted edge actually received the connection from —
/// everything further left is what that client *claims*, so the walk stops there. The peer
/// itself is not part of the recorded chain: the caller has already vouched for it.
fn trusted_client(addrs: &[IpAddr], proxies: &TrustedProxies, peer: Option<IpAddr>) -> IpAddr {
    for addr in addrs.iter().rev() {
        if !proxies.trusts(addr) {
            return *addr;
        }
    }
    // Every recorded hop is trusted, so the client is the leftmost entry — or, when the chain
    // is empty, the peer itself.
    addrs
        .first()
        .copied()
        .or(peer)
        .unwrap_or(IpAddr::V6(Ipv6Addr::LOCALHOST))
}

/// The `for=` chain of every `Forwarded` header field, leftmost first — each header
/// instance contributes in received order, exactly as a comma-join of them would
/// (RFC 7230 §3.2.2). `None` when no `for=` is present.
fn forwarded_chain(request: &Request) -> Option<Vec<IpAddr>> {
    let mut addrs = Vec::new();
    for value in request.headers().get_all(header::FORWARDED).iter() {
        for entry in value.as_bytes().split(|b| *b == b',') {
            for pair in entry.split(|b| *b == b';') {
                let Some((mut key, value)) = split_once(pair, b'=') else {
                    continue;
                };
                trim(&mut key);
                if !key.eq_ignore_ascii_case(b"for") {
                    continue;
                }
                if let Ok(addr) = parse_forwarded_for_value(value) {
                    addrs.push(addr);
                }
            }
        }
    }
    (!addrs.is_empty()).then_some(addrs)
}

/// The address list of every `X-Forwarded-For` header field, leftmost first — each
/// instance's entries extend the list in received order.
fn x_forwarded_for_chain(request: &Request) -> Option<Vec<IpAddr>> {
    let addrs: Vec<IpAddr> = request
        .headers()
        .get_all(HeaderName::from_static("x-forwarded-for"))
        .iter()
        .flat_map(|value| value.as_bytes().split(|b| *b == b','))
        .filter_map(|mut raw| {
            trim(&mut raw);
            std::str::from_utf8(raw)
                .ok()
                .and_then(|raw| IpAddr::from_str(raw).ok())
        })
        .collect();
    (!addrs.is_empty()).then_some(addrs)
}

impl Middleware for TrustedProxies {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        request.extensions_mut().insert(self.clone());
        next.run(request).await
    }

    fn provisions(&self) -> Vec<std::any::TypeId> {
        vec![std::any::TypeId::of::<Self>()]
    }
}

/// Extract the IP address of the client.
///
/// The connection's [`PeerAddr`] is the answer unless a [`TrustedProxies`] layer says the hop it
/// belongs to is trusted, in which case the `Forwarded` and `X-Forwarded-For` chains are walked
/// right-to-left past every trusted hop. On Cloudflare (`wasm32`) the platform's
/// `CF-Connecting-IP` is authoritative and always wins.
#[derive(Debug, Clone)]
pub struct ClientIp(pub IpAddr);

impl_deref!(ClientIp, IpAddr);

impl Extractor for ClientIp {
    type Error = ClientIpError;
    // Every source is already on the request, so the future is ready on creation rather than an
    // `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(client_ip(request))
    }

    // The client IP is derived server-side from connection metadata and proxy headers, so it is
    // not a client-supplied request parameter and contributes no OpenAPI schema.
}

/// Resolve the client address from the peer and whichever proxy headers the peer may write.
fn client_ip(request: &Request) -> Result<ClientIp, ClientIpError> {
    let peer = request
        .extensions()
        .get::<PeerAddr>()
        .map(|peer| peer.0.ip());
    let trusted = request.extensions().get::<TrustedProxies>();

    // Cloudflare sets `CF-Connecting-IP` at the edge and removes the client's own, so on a Worker
    // the header is the connection metadata `PeerAddr` would have been elsewhere.
    #[cfg(target_arch = "wasm32")]
    if let Some(value) = request.headers().get(CF_CONNECTING_IP) {
        let raw = std::str::from_utf8(value.as_bytes())?;
        return Ok(ClientIp(IpAddr::from_str(raw.trim())?));
    }

    if let Some(trusted) = trusted {
        // A host that recorded no peer at all (a serverless one) trusts the chain it was given:
        // that is what configuring proxies means there.
        let peer_trusted = peer.is_none_or(|peer| trusted.trusts(&peer));
        if peer_trusted {
            if let Some(addr) = trusted.forwarded_client(request, peer) {
                return Ok(ClientIp(addr));
            }
        }
    }

    peer.map_or(Err(ClientIpError::MissingRemoteAddr), |addr| {
        Ok(ClientIp(addr))
    })
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
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::{ClientIp, ClientIpError, TrustedProxies};
    use crate::{Body, Method, Request};
    use http_kit::header::HeaderValue;
    use ipnet::IpNet;
    use skyzen_core::{Extractor, PeerAddr};
    use std::str::FromStr;

    fn request_with_peer(ip: IpAddr) -> Request {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        request
            .extensions_mut()
            .insert(PeerAddr(SocketAddr::new(ip, 12345)));
        request
    }

    fn with_header(mut request: Request, name: &'static str, value: &'static str) -> Request {
        request.headers_mut().insert(
            http_kit::header::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
        request
    }

    fn trusted(nets: &[&str]) -> TrustedProxies {
        TrustedProxies::new(nets.iter().map(|net| IpNet::from_str(net).unwrap()))
    }

    #[tokio::test]
    async fn untrusted_peer_ignores_spoofed_forwarding_headers() {
        let mut request = with_header(
            request_with_peer(Ipv4Addr::new(203, 0, 113, 9).into()),
            "x-forwarded-for",
            "198.51.100.1",
        );

        let ip = ClientIp::extract(&mut request).await.unwrap();
        assert_eq!(ip.0, IpAddr::from([203, 0, 113, 9]));
    }

    #[tokio::test]
    async fn trusted_peer_walks_the_chain_past_trusted_hops() {
        let mut request = with_header(
            request_with_peer(Ipv4Addr::new(10, 0, 0, 2).into()),
            "x-forwarded-for",
            "198.51.100.1, 10.0.0.1",
        );
        request.extensions_mut().insert(trusted(&["10.0.0.0/8"]));

        let ip = ClientIp::extract(&mut request).await.unwrap();
        assert_eq!(ip.0, IpAddr::from([198, 51, 100, 1]));
    }

    #[tokio::test]
    async fn an_all_trusted_chain_reports_the_leftmost_entry() {
        let mut request = with_header(
            request_with_peer(Ipv4Addr::new(10, 0, 0, 2).into()),
            "x-forwarded-for",
            "10.0.0.1, 10.0.0.9",
        );
        request.extensions_mut().insert(trusted(&["10.0.0.0/8"]));

        let ip = ClientIp::extract(&mut request).await.unwrap();
        assert_eq!(ip.0, IpAddr::from([10, 0, 0, 1]));
    }

    #[tokio::test]
    async fn forwarded_header_is_walked_the_same_way() {
        let mut request = with_header(
            request_with_peer(Ipv4Addr::new(10, 0, 0, 2).into()),
            "forwarded",
            "for=198.51.100.1, for=10.0.0.1;proto=https",
        );
        request.extensions_mut().insert(trusted(&["10.0.0.0/8"]));

        let ip = ClientIp::extract(&mut request).await.unwrap();
        assert_eq!(ip.0, IpAddr::from([198, 51, 100, 1]));
    }

    #[tokio::test]
    async fn cloudflare_reads_cf_connecting_ip_from_a_cf_peer() {
        let mut request = with_header(
            request_with_peer(IpAddr::from_str("172.70.1.1").unwrap()),
            "cf-connecting-ip",
            "198.51.100.7",
        );
        request
            .extensions_mut()
            .insert(TrustedProxies::cloudflare());

        let ip = ClientIp::extract(&mut request).await.unwrap();
        assert_eq!(ip.0, IpAddr::from([198, 51, 100, 7]));
    }

    #[tokio::test]
    async fn cf_connecting_ip_from_a_non_cf_peer_is_not_read() {
        let mut request = with_header(
            request_with_peer(Ipv4Addr::new(203, 0, 113, 9).into()),
            "cf-connecting-ip",
            "198.51.100.7",
        );
        request
            .extensions_mut()
            .insert(TrustedProxies::cloudflare());

        let ip = ClientIp::extract(&mut request).await.unwrap();
        assert_eq!(ip.0, IpAddr::from([203, 0, 113, 9]));
    }

    #[tokio::test]
    async fn no_peer_and_no_config_still_fails() {
        let mut request = Request::new(Body::empty());
        let error = ClientIp::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, ClientIpError::MissingRemoteAddr));
    }

    #[tokio::test]
    async fn no_peer_honors_no_forwarded_chain() {
        // A serverless host records no TCP peer, so nothing vouches for any entry — even a
        // "trusted" hop can be client-forged. Forwarded headers are unverifiable here; only
        // platform-supplied headers (cf-connecting-ip et al.) count.
        let mut request = with_header(
            Request::new(Body::empty()),
            "x-forwarded-for",
            "198.51.100.1, 10.0.0.1",
        );
        request.extensions_mut().insert(trusted(&["10.0.0.0/8"]));

        let error = ClientIp::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, ClientIpError::MissingRemoteAddr));
    }

    #[tokio::test]
    async fn no_peer_rejects_a_fabricated_chain() {
        // Serverless with no anchor in the chain: every entry is client-supplied, so the
        // header cannot vouch for anything.
        let mut request = with_header(
            Request::new(Body::empty()),
            "x-forwarded-for",
            "198.51.100.1, 198.51.100.2",
        );
        request.extensions_mut().insert(trusted(&["10.0.0.0/8"]));

        let error = ClientIp::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, ClientIpError::MissingRemoteAddr));
    }

    #[tokio::test]
    async fn an_attacker_prepended_ip_does_not_win_the_trust_boundary() {
        // The rightmost untrusted entry is what the trusted edge observed; entries left of it
        // are the sender's own claims and must never resolve.
        let mut request = with_header(
            request_with_peer(Ipv4Addr::new(10, 0, 0, 2).into()),
            "x-forwarded-for",
            "9.9.9.9, 198.51.100.1, 10.0.0.1",
        );
        request.extensions_mut().insert(trusted(&["10.0.0.0/8"]));

        let ip = ClientIp::extract(&mut request).await.unwrap();
        assert_eq!(ip.0, IpAddr::from([198, 51, 100, 1]));
    }
}
