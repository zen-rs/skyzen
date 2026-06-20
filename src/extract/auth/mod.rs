//! Authentication extractors.
//!
//! This module provides extractors for authentication-related data from requests.

mod bearer;

pub(crate) use bearer::parse_bearer;
pub use bearer::{BearerToken, BearerTokenError};
