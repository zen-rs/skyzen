//! `Conditional<R>` — validator-aware responder.

use std::time::SystemTime;

use headers::{HeaderMapExt, LastModified};
use http_kit::{Request, Response, StatusCode};
use skyzen_core::Responder;

use crate::utils::conditional::{fresh, EntityTag};

/// A responder carrying `ETag`/`Last-Modified` validators.
///
/// When the request's `If-None-Match`/`If-Modified-Since` say the client's copy is current,
/// it emits `304` with the validators and **no body** — `R` is dropped without being
/// serialised. Otherwise it delegates to `R` and appends the validators. `HEAD` answers the
/// same way.
///
/// ```rust,ignore
/// Conditional::new(Json(post))
///     .etag(EntityTag::strong(post.revision))
///     .last_modified(post.updated_at)
/// ```
#[derive(Debug)]
pub struct Conditional<R> {
    inner: Option<R>,
    etag: Option<EntityTag>,
    last_modified: Option<SystemTime>,
}

impl<R> Conditional<R> {
    /// Wrap `inner` in a validator-aware responder.
    pub const fn new(inner: R) -> Self {
        Self {
            inner: Some(inner),
            etag: None,
            last_modified: None,
        }
    }

    /// Defer building the inner responder until the validators prove it is needed — a `304`
    /// skips `build` entirely, so an expensive render never happens on a cache hit.
    pub fn lazy<F>(etag: EntityTag, build: F) -> Conditional<Lazy<R, F>>
    where
        F: FnOnce() -> R + Send + Sync + 'static,
    {
        Conditional::new(Lazy {
            build: Some(build),
            _inner: core::marker::PhantomData,
        })
        .etag(etag)
    }

    /// The `ETag` to validate against — `EntityTag::strong(..)` for byte-identity resources.
    #[must_use]
    pub fn etag(mut self, etag: EntityTag) -> Self {
        self.etag = Some(etag);
        self
    }

    /// The `Last-Modified` to validate against — weaker than the `ETag`, honoured only when
    /// the request carries no `If-None-Match`.
    #[must_use]
    pub const fn last_modified(mut self, at: SystemTime) -> Self {
        self.last_modified = Some(at);
        self
    }

    /// Write the validator headers the response advertises, `304` or not.
    fn write_validators(&self, response: &mut Response) {
        if let Some(etag) = &self.etag {
            response.headers_mut().typed_insert(etag.etag().clone());
        }
        if let Some(at) = self.last_modified {
            response.headers_mut().typed_insert(LastModified::from(at));
        }
    }
}

impl<R: Responder> Responder for Conditional<R> {
    type Error = R::Error;
    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        // `If-None-Match`/`If-Modified-Since` only negotiate freshness on GET and HEAD —
        // on unsafe methods they are write preconditions (Preconditions's job, answering
        // 412), and a 304 here would silently drop the write.
        let cacheable = matches!(
            request.method(),
            &http_kit::Method::GET | &http_kit::Method::HEAD
        );
        if cacheable
            && fresh(
                request.headers(),
                self.etag.as_ref().map(EntityTag::etag),
                self.last_modified,
            )
        {
            *response.status_mut() = StatusCode::NOT_MODIFIED;
            self.write_validators(response);
            // `inner` drops here — the body is never built for a 304.
            return Ok(());
        }
        self.write_validators(response);
        self.inner
            .expect("responded once")
            .respond_to(request, response)
    }
}

/// The inner responder [`Conditional::lazy`] builds on demand — `F` runs only when the
/// validators let the response through.
#[derive(Debug)]
pub struct Lazy<R, F: FnOnce() -> R> {
    build: Option<F>,
    _inner: core::marker::PhantomData<R>,
}

impl<R: Responder, F: FnOnce() -> R + Send + Sync + 'static> Responder for Lazy<R, F> {
    type Error = R::Error;
    fn respond_to(mut self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        self.build.take().expect("built once")().respond_to(request, response)
    }
}

#[cfg(test)]
mod tests {
    use super::{Conditional, EntityTag};
    use crate::{
        extract::Preconditions,
        middleware::AutoETag,
        routing::{CreateRouteNode, Route},
        Body, Request, StatusCode,
    };
    use http_kit::header::{ETAG, IF_MATCH, IF_NONE_MATCH};

    async fn tagged() -> Conditional<&'static str> {
        Conditional::new("the body").etag(EntityTag::strong(1))
    }

    #[tokio::test]
    async fn if_none_match_answers_304_with_no_body() {
        let router = Route::new(("/thing".at(tagged),)).build();

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/thing".parse().unwrap();
        let response = router.go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[ETAG].to_str().unwrap(), "\"1\"");

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/thing".parse().unwrap();
        request
            .headers_mut()
            .insert(IF_NONE_MATCH, "\"1\"".parse().unwrap());
        let response = router.go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[ETAG].to_str().unwrap(), "\"1\"");
        assert!(response.into_body().into_bytes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unsafe_method_never_answers_304() {
        // If-None-Match on POST is a write precondition, not a freshness negotiation —
        // a 304 here would silently swallow the write.
        let router = Route::new(("/thing".post(tagged),)).build();

        let mut request = Request::new(Body::empty());
        *request.method_mut() = http_kit::Method::POST;
        *request.uri_mut() = "/thing".parse().unwrap();
        request
            .headers_mut()
            .insert(IF_NONE_MATCH, "\"1\"".parse().unwrap());
        let response = router.go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().into_string().await.unwrap(),
            "the body"
        );
    }

    async fn update(pre: Preconditions) -> Result<String, skyzen_core::Error> {
        let current = EntityTag::strong(2);
        pre.check(&current).map_err(crate::Error::from)?;
        Ok("updated".to_owned())
    }

    #[tokio::test]
    async fn a_stale_if_match_rejects_the_write_with_412() {
        let router = Route::new(("/thing".post(update),)).build();

        let mut request = Request::new(Body::empty());
        *request.method_mut() = http_kit::Method::POST;
        *request.uri_mut() = "/thing".parse().unwrap();
        request
            .headers_mut()
            .insert(IF_MATCH, "\"2\"".parse().unwrap());
        let response = router.go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut request = Request::new(Body::empty());
        *request.method_mut() = http_kit::Method::POST;
        *request.uri_mut() = "/thing".parse().unwrap();
        request
            .headers_mut()
            .insert(IF_MATCH, "\"9\"".parse().unwrap());
        let error = router.go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::PRECONDITION_FAILED);
    }

    async fn hello() -> &'static str {
        "hello"
    }

    #[tokio::test]
    async fn auto_etag_tags_buffered_gets_and_leaves_streaming_alone() {
        let router = Route::new(("/hello".at(hello),))
            .layer(AutoETag::new())
            .build();

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/hello".parse().unwrap();
        let response = router.go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers()[ETAG].to_str().unwrap().to_owned();
        assert!(etag.starts_with("W/\""), "{etag}");

        // The same request twice yields the same tag — the hash is stable.
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/hello".parse().unwrap();
        let response = router.go(request).await.unwrap();
        assert_eq!(response.headers()[ETAG].to_str().unwrap(), etag);

        // And a matching If-None-Match answers 304 with no body — the tag gates the
        // response, it isn't decorative.
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/hello".parse().unwrap();
        request
            .headers_mut()
            .insert(IF_NONE_MATCH, etag.parse().unwrap());
        let response = router.go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(response.into_body().into_bytes().await.unwrap().is_empty());
    }
}
