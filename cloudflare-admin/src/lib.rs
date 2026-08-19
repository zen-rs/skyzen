//! Shared Cloudflare API response types and token-source configuration for
//! Rust tools that talk to the Cloudflare control-plane API.
//!
//! This is **not** a full API client — it carries no HTTP transport and no
//! endpoint wrappers. The crate exposes:
//!
//! - [`CfApiResponse`] / [`CfApiError`] — the common `{success, errors,
//!   result}` envelope Cloudflare wraps every response in, with
//!   [`CfApiResponse::into_result`] to unwrap it.
//! - [`CloudflareError`] — the error type `into_result` produces, with
//!   `From` conversions for IO and JSON errors.
//! - [`TokenSource`] — marker indicating whether the caller holds a proper
//!   API token (full permissions) or a wrangler-derived OAuth token
//!   (limited to zones the user owns through dashboard login).
//!
//! Concrete clients (HTTP transport, endpoint orchestration) live in
//! downstream crates that build on these types.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced when unwrapping Cloudflare API response envelopes.
#[derive(Debug, Error)]
pub enum CloudflareError {
    /// Cloudflare returned one or more API-level errors.
    #[error("API error: {0}")]
    Api(String),

    /// Filesystem IO failed while reading Cloudflare configuration.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Standard Cloudflare API response envelope: `{ success, errors, result }`.
#[derive(Debug, Deserialize)]
pub struct CfApiResponse<T> {
    /// Whether Cloudflare considered the request successful.
    pub success: bool,
    /// API errors returned by Cloudflare when `success` is false.
    #[serde(default)]
    pub errors: Vec<CfApiError>,
    /// Successful response payload.
    pub result: Option<T>,
}

impl<T> CfApiResponse<T> {
    /// Convert into `Result<T, CloudflareError>`, joining every error
    /// message into one string for display.
    ///
    /// # Errors
    ///
    /// Returns an error when Cloudflare reported failure or omitted the
    /// expected result payload.
    pub fn into_result(self) -> Result<T, CloudflareError> {
        if self.success {
            self.result
                .ok_or_else(|| CloudflareError::Api("missing result payload".into()))
        } else if self.errors.is_empty() {
            Err(CloudflareError::Api(
                "Cloudflare reported failure without error details".into(),
            ))
        } else {
            let joined = self
                .errors
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join(", ");
            Err(CloudflareError::Api(joined))
        }
    }
}

/// Cloudflare API error entry from a response envelope.
#[derive(Debug, Deserialize)]
pub struct CfApiError {
    /// Cloudflare numeric error code.
    pub code: i32,
    /// Human-readable Cloudflare error message.
    pub message: String,
}

/// Where the API token came from. Affects which endpoints will succeed
/// against the authenticated account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenSource {
    /// Proper API token from `CLOUDFLARE_API_TOKEN` (full permissions).
    ApiToken,
    /// OAuth token read from wrangler config. Lacks DNS/Access edit
    /// permissions; only works against endpoints the interactive login
    /// scopes covered.
    WranglerOAuth,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn into_result_returns_payload_on_success() {
        let response = CfApiResponse {
            success: true,
            errors: Vec::new(),
            result: Some(42_u32),
        };
        assert_eq!(response.into_result().expect("payload"), 42);
    }

    #[test]
    fn into_result_errors_when_success_lacks_payload() {
        let response: CfApiResponse<u32> = CfApiResponse {
            success: true,
            errors: Vec::new(),
            result: None,
        };
        let error = response.into_result().expect_err("missing payload");
        assert!(error.to_string().contains("missing result payload"));
    }

    #[test]
    fn into_result_joins_error_envelope_messages() {
        let response: CfApiResponse<u32> = CfApiResponse {
            success: false,
            errors: vec![
                CfApiError {
                    code: 7003,
                    message: "no such zone".into(),
                },
                CfApiError {
                    code: 9109,
                    message: "invalid token".into(),
                },
            ],
            result: None,
        };
        let error = response.into_result().expect_err("error envelope");
        assert_eq!(error.to_string(), "API error: no such zone, invalid token");
    }

    #[test]
    fn into_result_explains_empty_error_envelope() {
        let response: CfApiResponse<u32> = CfApiResponse {
            success: false,
            errors: Vec::new(),
            result: None,
        };
        let error = response.into_result().expect_err("failure without details");
        assert!(error.to_string().contains("failure without error details"));
    }
}
