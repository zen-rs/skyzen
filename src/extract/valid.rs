//! `Valid<E>` — payload extraction plus declarative validation through `garde`.
//!
//! Deserialisation proves a body has the right *shape*; `Valid` proves it satisfies the rules.
//! Wrap any payload extractor — `Json`, `Form`, `Query`, `Path`, or a third-party extractor
//! implementing [`ValidatePayload`] — and the handler only runs on a valid value:
//!
//! ```rust,ignore
//! async fn sign_up(Valid(Json(body), _): Valid<Json<SignUp>>) -> StatusCode { .. }
//! ```
//!
//! A [`garde`] `context` type gets its context from a second type parameter — `State<Limits>`
//! is extracted first and `&Limits` reaches the rules:
//!
//! ```rust,ignore
//! async fn create(Valid(Json(post), _): Valid<Json<Post>, State<Limits>>) -> StatusCode { .. }
//! ```

use core::{fmt, marker::PhantomData, ops::Deref};

use garde::Validate;
use http_kit::{HttpError, Request, StatusCode};
use skyzen_core::{ErrorDetails, Extractor, Requirement};

use crate::{
    extract::{Path, Query},
    utils::{Form, Json, State},
};

/// How a validation context reaches `garde`.
///
/// `()` is the default — a `#[garde(context(..))]` payload takes `State<T>` instead and the
/// shared state value is borrowed as `&T` while the rules run.
pub trait ValidContext {
    /// The context type `#[garde(context(..))]` payloads validate against.
    type Context: Sync;
    /// Borrow the extracted context.
    fn context(&self) -> &Self::Context;
}

impl ValidContext for () {
    type Context = ();
    fn context(&self) -> &() {
        &()
    }
}

impl<T: Send + Sync + Clone + 'static> ValidContext for State<T> {
    type Context = T;
    fn context(&self) -> &T {
        &self.0
    }
}

/// Opt-in for extractors [`Valid`] can wrap — exposes the decoded payload to `garde`.
///
/// Implemented for `Json`, `Form`, `Query` and `Path`. A third-party extractor opts in by
/// returning its decoded value from [`payload`][ValidatePayload::payload].
pub trait ValidatePayload {
    /// The decoded payload type `garde` validates.
    type Payload;
    /// The value the rules run against.
    fn payload(&self) -> &Self::Payload;
}

impl<T: Send + Sync + 'static> ValidatePayload for Json<T> {
    type Payload = T;
    fn payload(&self) -> &T {
        &self.0
    }
}

impl<T: Send + Sync> ValidatePayload for Form<T> {
    type Payload = T;
    fn payload(&self) -> &T {
        &self.0
    }
}

impl<T> ValidatePayload for Query<T> {
    type Payload = T;
    fn payload(&self) -> &T {
        &self.0
    }
}

impl<T> ValidatePayload for Path<T> {
    type Payload = T;
    fn payload(&self) -> &T {
        &self.0
    }
}

/// An extractor that validates its payload after decoding.
///
/// Decodes exactly like `E`, then runs [`garde::Validate`] before the handler sees the value;
/// a failed rule rejects with `422` and every field's violation. `Ctx` is the validation
/// context extractor — `()` by default, `State<T>` for `#[garde(context(T))]` payloads.
///
/// The second tuple field is a `PhantomData` marker for `Ctx`, so destructuring looks like
/// `Valid(Json(body), _)`.
#[derive(Debug)]
pub struct Valid<E, Ctx = ()>(pub E, #[doc(hidden)] pub PhantomData<Ctx>);

impl<E, Ctx> Deref for Valid<E, Ctx> {
    type Target = E;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<E, Ctx> Extractor for Valid<E, Ctx>
where
    E: Extractor + ValidatePayload,
    E::Payload: Validate<Context = <Ctx as ValidContext>::Context>,
    Ctx: Extractor + ValidContext,
{
    type Error = ValidRejection<E::Error, Ctx::Error>;

    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let context = Ctx::extract(request)
            .await
            .map_err(ValidRejection::Context)?;
        let inner = E::extract(request).await.map_err(ValidRejection::Extract)?;
        inner
            .payload()
            .validate_with(context.context())
            .map_err(|report| ValidRejection::Invalid(ValidationRejection::new(&report)))?;
        Ok(Self(inner, PhantomData))
    }

    fn requirements() -> Vec<Requirement> {
        let mut requirements = E::requirements();
        requirements.extend(Ctx::requirements());
        requirements
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        E::openapi()
    }
}

/// Why a [`Valid`] extraction failed — the inner extractor's error, the context extractor's
/// error, or the validation rejection itself.
#[derive(Debug)]
pub enum ValidRejection<E, C> {
    /// The payload could not be decoded — `E`'s own error and status.
    Extract(E),
    /// The validation context could not be extracted — `Ctx`'s own error and status.
    Context(C),
    /// The decoded payload broke a rule.
    Invalid(ValidationRejection),
}

impl<E: HttpError, C: HttpError> fmt::Display for ValidRejection<E, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Extract(error) => fmt::Display::fmt(error, f),
            Self::Context(error) => fmt::Display::fmt(error, f),
            Self::Invalid(rejection) => fmt::Display::fmt(rejection, f),
        }
    }
}

impl<E: HttpError, C: HttpError> std::error::Error for ValidRejection<E, C> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Extract(error) => Some(error),
            Self::Context(error) => Some(error),
            Self::Invalid(rejection) => Some(rejection),
        }
    }
}

impl<E: HttpError, C: HttpError> HttpError for ValidRejection<E, C> {
    fn status(&self) -> StatusCode {
        match self {
            Self::Extract(error) => error.status(),
            Self::Context(error) => error.status(),
            Self::Invalid(rejection) => rejection.status(),
        }
    }
}

/// One field's failed rule.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FieldViolation {
    /// Where the rule failed — `email`, `items[2].name`, and so on.
    pub path: String,
    /// What the rule says — `garde`'s own message.
    pub message: String,
}

/// `422 Unprocessable Content` — the decoded payload broke one or more rules.
///
/// `errors()` gives the per-field violations, for handlers and `ErrorHandlingMiddleware`
/// layers that want to render the list themselves (an HTML form with inline messages, an
/// RFC 9457 problem document, and so on).
#[derive(Debug)]
pub struct ValidationRejection {
    errors: Vec<FieldViolation>,
    details: ErrorDetails,
}

impl ValidationRejection {
    fn new(report: &garde::Report) -> Self {
        let errors: Vec<FieldViolation> = report
            .iter()
            .map(|(path, error)| FieldViolation {
                path: path.to_string(),
                message: error.to_string(),
            })
            .collect();
        Self {
            details: ErrorDetails(serde_json::json!({ "violations": errors })),
            errors,
        }
    }

    /// Every field that failed a rule, in the order `garde` reported them.
    #[must_use]
    pub fn errors(&self) -> &[FieldViolation] {
        &self.errors
    }
}

impl fmt::Display for ValidationRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("validation failed: ")?;
        for (index, violation) in self.errors.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{}: {}", violation.path, violation.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationRejection {
    /// The violations ride in the `source()` chain where `error_response` finds them: it
    /// merges an [`ErrorDetails`] node into the body as `details`.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.details)
    }
}

impl HttpError for ValidationRejection {
    fn status(&self) -> StatusCode {
        StatusCode::UNPROCESSABLE_ENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::{Valid, ValidRejection};
    use crate::{
        extract::Query,
        routing::{CreateRouteNode, Route},
        utils::{Json, State},
        Body, Method, Request, StatusCode,
    };
    use garde::Validate;
    use http_kit::{header::HeaderValue, header::CONTENT_TYPE, HttpError};
    use serde::Deserialize;

    #[derive(Debug, Deserialize, Validate, crate::ToSchema)]
    struct SignUp {
        #[garde(email)]
        email: String,
        #[garde(length(min = 12))]
        password: String,
    }

    async fn sign_up(Valid(Json(body), _): Valid<Json<SignUp>>) -> String {
        body.email
    }

    fn json_request(body: &str) -> Request {
        let mut request = Request::new(Body::from_bytes(body.as_bytes().to_vec()));
        *request.method_mut() = Method::POST;
        request
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        request
    }

    #[tokio::test]
    async fn a_valid_payload_reaches_the_handler() {
        let router = Route::new(("/signup".post(sign_up),)).build();
        let mut request = json_request(r#"{"email":"a@b.co","password":"long-enough-pass"}"#);
        *request.uri_mut() = "/signup".parse().unwrap();
        let response = router.go(request).await.unwrap();
        assert_eq!(response.into_body().into_string().await.unwrap(), "a@b.co");
    }

    #[tokio::test]
    async fn a_rule_violation_rejects_with_422_and_names_every_field() {
        let router = Route::new(("/signup".post(sign_up),)).build();
        let mut request = json_request(r#"{"email":"not-an-email","password":"short"}"#);
        *request.uri_mut() = "/signup".parse().unwrap();
        let error = router.go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        // Every violation names its field in the rejection message.
        assert!(error.to_string().contains("email"), "{error}");
        assert!(error.to_string().contains("password"), "{error}");
    }

    #[tokio::test]
    async fn a_malformed_body_keeps_the_inner_extractor_error() {
        let router = Route::new(("/signup".post(sign_up),)).build();
        let mut request = json_request("not json at all");
        *request.uri_mut() = "/signup".parse().unwrap();
        let error = router.go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[derive(Debug, Deserialize, Validate, crate::ToSchema)]
    #[garde(context(Limits as ctx))]
    struct Post {
        #[garde(length(max = ctx.max_title))]
        title: String,
    }

    #[derive(Clone)]
    struct Limits {
        max_title: usize,
    }

    async fn create(Valid(Json(post), _): Valid<Json<Post>, State<Limits>>) -> String {
        post.title
    }

    #[tokio::test]
    async fn the_context_comes_from_state() {
        let router = Route::new(("/posts".post(create),))
            .layer(State(Limits { max_title: 5 }))
            .build();

        let mut request = json_request(r#"{"title":"a very long title indeed"}"#);
        *request.uri_mut() = "/posts".parse().unwrap();
        let error = router.go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let mut request = json_request(r#"{"title":"tiny"}"#);
        *request.uri_mut() = "/posts".parse().unwrap();
        let response = router.go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[derive(Debug, Deserialize, Validate, crate::ToSchema)]
    struct Search {
        #[garde(length(min = 2))]
        q: String,
    }

    async fn search(Valid(Query(q), _): Valid<Query<Search>>) -> String {
        q.q
    }

    #[tokio::test]
    async fn query_params_validate_too() {
        let router = Route::new(("/search".at(search),)).build();

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/search?q=x".parse().unwrap();
        let error = router.go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/search?q=hello".parse().unwrap();
        let response = router.go(request).await.unwrap();
        assert_eq!(response.into_body().into_string().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn the_rejection_exposes_structured_violations() {
        let mut request = json_request(r#"{"email":"bad","password":"x"}"#);
        let error = <Valid<Json<SignUp>> as skyzen_core::Extractor>::extract(&mut request)
            .await
            .unwrap_err();
        match error {
            ValidRejection::Invalid(rejection) => {
                assert_eq!(rejection.status(), StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(rejection.errors().len(), 2);
                assert_eq!(rejection.errors()[0].path, "email");

                // The machine-readable list reaches the HTTP body too, through the
                // `ErrorDetails` node in `source()` — an RFC 9457 problem document.
                let response = skyzen_core::error_response(&rejection);
                assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
                let body = response.into_body().into_string().await.unwrap();
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["status"], 422);
                assert_eq!(body["title"], "Unprocessable Entity");
                assert_eq!(body["violations"][0]["path"], "email");
            }
            other => panic!("expected a validation rejection, got {other:?}"),
        }
    }
}
