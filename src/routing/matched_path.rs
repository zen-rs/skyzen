//! The route template the request matched.

use core::future::{ready, Future};
use std::sync::Arc;

use http_kit::{Request, StatusCode};
use skyzen_core::Extractor;

/// The route pattern that matched the request: `"/users/{id}"`, not `"/users/42"`.
///
/// The router inserts it next to [`Params`](crate::routing::Params) when a request matches,
/// so handlers can log or emit the low-cardinality template instead of the raw path:
///
/// ```rust
/// use skyzen::{Result, routing::MatchedPath};
///
/// async fn handler(route: MatchedPath) -> Result<String> {
///     Result::Ok(route.as_str().to_owned())
/// }
/// ```
///
/// [`Trace`](crate::middleware::Trace) reads it to fill the `http.route` span field.
/// Requests answered without a match — a custom `Route::fallback` — carry no
/// `MatchedPath`; extract `Option<MatchedPath>` there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedPath(Arc<str>);

impl MatchedPath {
    pub(crate) fn new(path: impl Into<Arc<str>>) -> Self {
        Self(path.into())
    }

    /// The matched template, e.g. `"/users/{id}"`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for MatchedPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MatchedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

http_kit::http_error!(
    /// Raised when a handler extracts [`MatchedPath`] but no route matched.
    ///
    /// A handler is normally only reached *through* a match, so reaching this means the
    /// endpoint was served without one — a `Route::fallback` answering an unmatched
    /// request, or a bare endpoint in a test. Extract `Option<MatchedPath>` where absence
    /// is expected.
    pub NoMatchedPath,
    StatusCode::INTERNAL_SERVER_ERROR,
    "No route matched this request"
);

/// The mount prefix accumulated across nested routers, pushed by `Mounted` before it
/// dispatches into the mounted router. `dispatch_matched` composes it with the registered
/// template so every `MatchedPath` is already the full route (`/api` + `/users/{id}`) at
/// insertion time — inner handlers extract the complete template, not the inner fragment.
#[derive(Debug, Clone)]
pub struct MountPrefix(pub String);

impl Extractor for MatchedPath {
    type Error = NoMatchedPath;

    // Reading the template back out of the extensions is a synchronous clone, so the
    // future is ready on creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or_else(NoMatchedPath::new),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::MatchedPath;
    use crate::{
        middleware::{from_fn, Next},
        routing::{CreateRouteNode, Route},
        Body, Request, Result,
    };
    use std::sync::{Arc, Mutex};

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("valid path");
        request
    }

    async fn route_name(route: MatchedPath) -> Result<String> {
        Result::Ok(route.as_str().to_owned())
    }

    #[tokio::test]
    async fn reports_the_template_not_the_concrete_path() {
        let router = Route::new(("/users/{id}".at(route_name),)).build();
        let response = router.go(get("/users/42")).await.unwrap();
        assert_eq!(
            response.into_body().into_string().await.unwrap(),
            "/users/{id}"
        );
    }

    #[tokio::test]
    async fn nested_mounts_report_the_full_template() {
        let inner = Route::new(("/users/{id}".at(route_name),));
        let router = Route::new(("/api".nest(inner.build()),)).build();
        let response = router.go(get("/api/users/42")).await.unwrap();
        assert_eq!(
            response.into_body().into_string().await.unwrap(),
            "/api/users/{id}"
        );
    }

    #[tokio::test]
    async fn an_unmatched_path_inside_a_nest_leaves_no_matched_path() {
        // The post-route value an outer layer (e.g. `Trace`) reads: `Option<MatchedPath>`
        // inside the inner fallback still sees the mount template, but after the mount
        // returns nothing user-visible matched and the extension is gone.
        let seen: Arc<Mutex<Option<MatchedPath>>> = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&seen);
        let inner = Route::new(("/users/{id}".at(|| async { Result::Ok("u") }),));
        let router = Route::new(("/api".nest(inner.build()),))
            .layer(from_fn(move |request: &mut Request, next: Next<'_>| {
                let captured = Arc::clone(&captured);
                Box::pin(async move {
                    let result = next.run(request).await;
                    *captured.lock().unwrap() = request.extensions().get::<MatchedPath>().cloned();
                    result
                })
            }))
            .build();

        let response = router.go(get("/api/nope")).await.unwrap();
        assert_eq!(response.status(), crate::StatusCode::NOT_FOUND);
        assert!(seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unmatched_path_reports_no_template() {
        let router = Route::new(("/known".at(|| async { Result::Ok("ok") }),))
            .fallback(|route: Option<MatchedPath>| async move {
                Result::Ok(
                    route.map_or_else(|| "<unmatched>".to_owned(), |path| path.as_str().to_owned()),
                )
            })
            .build();

        let response = router.go(get("/nope")).await.unwrap();
        assert_eq!(
            response.into_body().into_string().await.unwrap(),
            "<unmatched>"
        );
    }
}
