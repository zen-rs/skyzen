//! Mounting an already-built [`Router`] inside a larger route tree.

use std::sync::Arc;

use http_kit::error::BoxHttpError;
use http_kit::uri::PathAndQuery;

use crate::{Endpoint, Request, Response, StatusCode, Uri};

use super::Router;

/// Serves a mounted [`Router`], with the mount prefix stripped from the request path.
///
/// Constructed by [`CreateRouteNode::nest`](super::CreateRouteNode::nest); it is public only
/// because it appears in the routing tree's `Debug` output.
///
/// The inner router matches against paths as if it were mounted at the root, which is what lets a
/// library export a `Router` without knowing where its consumer will hang it. Its fallback and its
/// `405` still belong to it: an unmatched path under the prefix is answered by the *inner* router,
/// not by the outer one.
#[derive(Clone)]
pub struct NestedRouter {
    prefix: Arc<str>,
    router: Router,
}

impl std::fmt::Debug for NestedRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NestedRouter")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl NestedRouter {
    pub(crate) fn new(prefix: &str, router: Router) -> Self {
        Self {
            prefix: Arc::from(prefix),
            router,
        }
    }
}

/// Raised when the mount prefix cannot be stripped from a request's path.
///
/// The router only routes a request here after matching the prefix, so reaching this means the
/// rewritten path was not a usable URI — a bug rather than bad input.
#[skyzen::error(
    message = "Could not rewrite `{0}` for the router mounted at its prefix",
    status = StatusCode::INTERNAL_SERVER_ERROR
)]
pub struct NestedPathError(String);

impl Endpoint for NestedRouter {
    type Error = BoxHttpError;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        let original = request.uri().clone();
        let rewritten = strip_prefix(&original, &self.prefix)
            .ok_or_else(|| Box::new(NestedPathError(original.to_string())) as BoxHttpError)?;

        *request.uri_mut() = rewritten;
        let result = self.router.respond(request).await;
        // Put the path back so an outer layer that inspects the request after the inner router
        // returns still sees the URL the client asked for.
        *request.uri_mut() = original;
        result
    }
}

/// Rewrite `uri` as the mounted router sees it: prefix removed, query kept.
fn strip_prefix(uri: &Uri, prefix: &str) -> Option<Uri> {
    let path = uri.path();
    // The prefix is matched on the raw path, before percent-decoding, so a `%2F` inside a segment
    // cannot smuggle its way past the mount point.
    let rest = path.strip_prefix(prefix)?;
    let rest = if rest.is_empty() { "/" } else { rest };

    let path_and_query = match uri.query() {
        Some(query) => {
            let mut buffer = String::with_capacity(rest.len() + query.len() + 1);
            buffer.push_str(rest);
            buffer.push('?');
            buffer.push_str(query);
            buffer.parse::<PathAndQuery>().ok()?
        }
        None => rest.parse::<PathAndQuery>().ok()?,
    };

    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query);
    Uri::from_parts(parts).ok()
}

#[cfg(test)]
mod tests {
    use super::strip_prefix;
    use crate::Uri;

    fn rewrite(uri: &str, prefix: &str) -> String {
        let uri: Uri = uri.parse().expect("valid uri");
        strip_prefix(&uri, prefix)
            .expect("prefix should strip")
            .to_string()
    }

    #[test]
    fn strips_the_prefix_and_keeps_the_query() {
        assert_eq!(rewrite("/api/users?page=2", "/api"), "/users?page=2");
        assert_eq!(rewrite("/api/users", "/api"), "/users");
    }

    #[test]
    fn the_bare_mount_point_becomes_the_root() {
        assert_eq!(rewrite("/api", "/api"), "/");
        assert_eq!(rewrite("/api/", "/api"), "/");
    }

    #[test]
    fn an_absolute_uri_keeps_its_authority() {
        assert_eq!(
            rewrite("http://localhost/api/users", "/api"),
            "http://localhost/users"
        );
    }
}
