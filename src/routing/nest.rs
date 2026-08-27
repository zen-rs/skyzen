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
    // A prefix of `/`, or one written with a trailing slash, takes the leading slash with it —
    // and a rootless remainder is not a path the mounted router could ever match. Put it back.
    let mut rewritten = String::with_capacity(rest.len() + uri.query().map_or(1, |q| q.len() + 2));
    if !rest.starts_with('/') {
        rewritten.push('/');
    }
    rewritten.push_str(rest);

    if let Some(query) = uri.query() {
        rewritten.push('?');
        rewritten.push_str(query);
    }

    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(rewritten.parse::<PathAndQuery>().ok()?);
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
    fn a_prefix_that_takes_the_leading_slash_with_it_leaves_a_rooted_path() {
        // Mounted at the root, or written with a trailing slash: the remainder must not come back
        // rootless, or the mounted router could never match it.
        assert_eq!(rewrite("/users", "/"), "/users");
        assert_eq!(rewrite("/users?page=2", "/"), "/users?page=2");
        assert_eq!(rewrite("/api/users", "/api/"), "/users");
        assert_eq!(rewrite("/api/", "/api/"), "/");
    }

    #[test]
    fn an_absolute_uri_keeps_its_authority() {
        assert_eq!(
            rewrite("http://localhost/api/users", "/api"),
            "http://localhost/users"
        );
    }
}
