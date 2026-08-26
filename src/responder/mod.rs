//! Modify response or make a response,but in a strong-typed way.
//!
//! [`Responder`](crate::responder::Responder) is a trait modifying or generating response
//! ```
//! # use skyzen::{utils::Json,Responder};
//! async fn handler() -> impl Responder{
//!     Json("Hello,world")
//! }
//!
//! ```
//!
//! Responder can be combined by tuple easily,
//! ```
//! # use skyzen::{utils::Json,header::{CONTENT_TYPE,HeaderValue},Responder};
//! async fn handler() -> impl Responder{
//!     (r#""Hello,world""#,(CONTENT_TYPE,HeaderValue::from_static("application/json")))
//! }
//! ```
//! Result<T> is also a responder, it allows you handle error conveniently in handler.
//!
//! ```
//! # use skyzen::{utils::Json,Result,routing::Params,Responder};
//! async fn handler(params:Params) -> Result<impl Responder>{
//!     let name=params.get("name")?;
//!     Ok(format!("Hello,{name}"))
//! }
//!
//! ```
//!
pub use skyzen_core::Responder;

#[cfg(feature = "sse")]
pub mod sse;
#[cfg(feature = "sse")]
pub use sse::Sse;

#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "json")]
pub use json::PrettyJson;

#[cfg(test)]
mod tests {
    use crate::{
        header::CONTENT_TYPE,
        routing::{CreateRouteNode, Route},
        utils::{Html, Json, Redirect},
        Body, Request, Result, StatusCode,
    };
    use serde::Serialize;

    #[derive(Debug, Serialize)]
    struct Article {
        id: u32,
    }

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("valid path");
        request
    }

    #[tokio::test]
    async fn a_status_composes_with_a_body_in_a_tuple() {
        async fn create() -> Result<(StatusCode, Json<Article>)> {
            Ok((StatusCode::CREATED, Json(Article { id: 1 })))
        }

        let router = Route::new(("/articles".at(create),)).build();
        let response = router.go(get("/articles")).await.unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, r#"{"id":1}"#);
    }

    #[tokio::test]
    async fn a_redirect_and_html_reach_the_response_through_a_handler() {
        async fn moved() -> Redirect {
            Redirect::see_other("/articles/1")
        }

        async fn page() -> Result<Html<&'static str>> {
            Ok(Html("<h1>Article</h1>"))
        }

        let router = Route::new(("/old".at(moved), "/page".at(page))).build();

        let response = router.clone().go(get("/old")).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(crate::header::LOCATION).unwrap(),
            "/articles/1"
        );

        let response = router.clone().go(get("/page")).await.unwrap();
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
    }
}
