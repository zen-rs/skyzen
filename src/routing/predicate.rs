//! Host and header routing predicates.

use crate::{
    header::{HeaderMap, HeaderName, HeaderValue, HOST},
    Request,
};

/// A host pattern matched against the request's `Host` header or URI authority.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostPattern {
    /// Exact hostname match (lowercased, port stripped from incoming request).
    Exact(String),
    /// Wildcard subdomain match (lowercased suffix starting with `.`, e.g. `.example.com`).
    Wildcard(String),
}

impl HostPattern {
    pub(crate) fn parse(pattern: &str) -> Self {
        let lower = pattern.trim().to_ascii_lowercase();
        lower.strip_prefix("*.").map_or_else(
            || Self::Exact(lower.clone()),
            |rest| Self::Wildcard(format!(".{rest}")),
        )
    }

    pub(crate) fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(expected) => host.eq_ignore_ascii_case(expected),
            Self::Wildcard(suffix) => {
                let host_bytes = host.as_bytes();
                let suffix_bytes = suffix.as_bytes();
                host_bytes.len() > suffix_bytes.len()
                    && host_bytes[host_bytes.len() - suffix_bytes.len()..]
                        .eq_ignore_ascii_case(suffix_bytes)
            }
        }
    }

    const fn specificity(&self) -> usize {
        match self {
            Self::Exact(_) => 2,
            Self::Wildcard(_) => 1,
        }
    }
}

/// A header predicate matched against the request's headers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HeaderPredicate {
    /// Header must be present (with any value).
    Exists(HeaderName),
    /// Header must be present with at least one value equal to the given value.
    Exact(HeaderName, HeaderValue),
}

impl HeaderPredicate {
    pub(crate) fn matches(&self, headers: &HeaderMap) -> bool {
        match self {
            Self::Exists(name) => headers.contains_key(name),
            Self::Exact(name, expected) => headers.get_all(name).iter().any(|v| v == expected),
        }
    }

    const fn specificity(&self) -> usize {
        match self {
            Self::Exact(_, _) => 2,
            Self::Exists(_) => 1,
        }
    }

    fn sort_key(&self) -> (&str, Option<&[u8]>) {
        match self {
            Self::Exists(name) => (name.as_str(), None),
            Self::Exact(name, value) => (name.as_str(), Some(value.as_bytes())),
        }
    }
}

/// Set of predicates attached to a route endpoint.
#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Predicates {
    hosts: Vec<HostPattern>,
    headers: Vec<HeaderPredicate>,
}

impl Predicates {
    pub(crate) fn add_host(&mut self, pattern: HostPattern) {
        self.hosts.push(pattern);
        self.hosts.sort();
        self.hosts.dedup();
    }

    pub(crate) fn add_header(&mut self, predicate: HeaderPredicate) {
        self.headers.push(predicate);
        self.headers.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        self.headers.dedup();
    }

    pub(crate) fn merge_from(&mut self, other: &Self) {
        if !other.hosts.is_empty() {
            self.hosts.extend(other.hosts.iter().cloned());
            self.hosts.sort();
            self.hosts.dedup();
        }
        if !other.headers.is_empty() {
            self.headers.extend(other.headers.iter().cloned());
            self.headers.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
            self.headers.dedup();
        }
    }

    pub(crate) fn matches(&self, request: &Request) -> bool {
        if !self.hosts.is_empty() {
            let Some(host) = extract_host(request) else {
                return false;
            };
            if !self.hosts.iter().all(|pattern| pattern.matches(host)) {
                return false;
            }
        }
        self.headers
            .iter()
            .all(|predicate| predicate.matches(request.headers()))
    }

    pub(crate) fn specificity(&self) -> usize {
        self.hosts
            .iter()
            .map(HostPattern::specificity)
            .sum::<usize>()
            + self
                .headers
                .iter()
                .map(HeaderPredicate::specificity)
                .sum::<usize>()
    }
}

/// Extract the hostname (without port) from the `Host` header or URI authority.
fn extract_host(request: &Request) -> Option<&str> {
    let raw = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(http_kit::uri::Authority::as_str)
        })?;
    Some(strip_port(raw.trim()))
}

fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &host[..=end + 1];
        }
        return host;
    }
    if host.bytes().filter(|&b| b == b':').count() == 1 {
        return host.split(':').next().unwrap_or(host);
    }
    host
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Body;

    #[test]
    fn host_matching_strips_port_and_is_case_insensitive() {
        let exact = HostPattern::parse("API.Example.COM");
        assert!(exact.matches("api.example.com"));
        assert!(exact.matches("API.example.com"));
        assert!(!exact.matches("sub.api.example.com"));

        let wildcard = HostPattern::parse("*.Example.COM");
        assert!(wildcard.matches("api.example.com"));
        assert!(wildcard.matches("v2.api.example.com"));
        assert!(!wildcard.matches("example.com"));
        assert!(!wildcard.matches("notexample.com"));
    }

    #[test]
    fn extract_host_from_header_and_uri_authority() {
        let mut req = Request::new(Body::empty());
        req.headers_mut()
            .insert(HOST, HeaderValue::from_static("api.example.com:8080"));
        assert_eq!(extract_host(&req), Some("api.example.com"));

        let mut req_uri = Request::new(Body::empty());
        *req_uri.uri_mut() = "http://sub.example.com:3000/items".parse().unwrap();
        assert_eq!(extract_host(&req_uri), Some("sub.example.com"));

        let mut req_v6 = Request::new(Body::empty());
        req_v6
            .headers_mut()
            .insert(HOST, HeaderValue::from_static("[::1]:8080"));
        assert_eq!(extract_host(&req_v6), Some("[::1]"));
    }
}
