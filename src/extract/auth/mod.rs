//! Authentication extractors.
//!
//! This module provides extractors for authentication-related data from requests.

mod bearer;

pub use bearer::{BearerToken, BearerTokenError};
