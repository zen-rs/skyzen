#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "json")]
pub use json::{json, Json, JsonValue};

#[cfg(feature = "form")]
pub mod form;
#[cfg(feature = "form")]
pub use form::Form;

#[cfg(feature = "multipart")]
pub mod multipart;
#[cfg(feature = "multipart")]
pub use multipart::{Field, Multipart, MultipartBoundaryError, MultipartError};

pub mod state;
pub use state::State;

pub mod cookie;

/// Error types
pub mod error {
    #[cfg(feature = "form")]
    pub use super::form::FormContentTypeError;
    #[cfg(feature = "json")]
    pub use super::json::JsonContentTypeError;
    #[cfg(feature = "multipart")]
    pub use super::multipart::MultipartBoundaryError;
    pub use super::state::StateNotExist;
}

pub use http_kit::utils::*;
