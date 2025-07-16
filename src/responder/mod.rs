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
mod json;
#[cfg(feature = "json")]
pub use json::PrettyJson;
