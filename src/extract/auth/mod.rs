//! Authentication extractors.
//!
//! This module provides extractors for authentication-related data from requests.

mod api_key;
mod basic;
mod bearer;

pub use api_key::{ApiKey, ApiKeyError, DEFAULT_API_KEY_HEADER};
pub use basic::{parse_basic, BasicAuth, BasicAuthError};

#[cfg(feature = "jwt")]
pub(crate) use bearer::parse_bearer;
pub use bearer::{BearerToken, BearerTokenError};
