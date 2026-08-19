//! Authentication extractors.
//!
//! This module provides extractors for authentication-related data from requests.

mod bearer;

// Only the native-only JWT authenticator consumes this helper directly.
#[cfg(all(feature = "jwt", not(target_arch = "wasm32")))]
pub(crate) use bearer::parse_bearer;
pub use bearer::{BearerToken, BearerTokenError};
