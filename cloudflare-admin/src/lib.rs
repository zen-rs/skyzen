//! Cloudflare control-plane API primitives used by any project that needs
//! to manage Cloudflare resources (DNS records, Pages projects, Access
//! applications, Workers routes, …) from a Rust host tool.
//!
//! The crate exposes:
//!
//! - [`CloudflareError`] — error type shared across every API call.
//! - [`CfApiResponse`] / [`CfApiError`] — the common `{success, errors,
//!   result}` envelope Cloudflare wraps every response in.
//! - [`TokenSource`] — marker indicating whether the caller holds a proper
//!   API token (full permissions) or a wrangler-derived OAuth token
//!   (limited to zones the user owns through dashboard login).
//!
//! Concrete high-level clients (DNS record CRUD, Access app lifecycle,
//! Pages project creation) compose these primitives. This crate is
//! deliberately kept as a small, stable kernel so that project-specific
//! orchestration can live in downstream crates without pulling the
//! primitives in through a private vendoring path.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors returned by Cloudflare admin operations.
#[derive(Debug, Error)]
pub enum CloudflareError {
    /// An HTTP request failed before Cloudflare returned a structured API response.
    #[error("HTTP request failed: {0}")]
    Http(String),

    /// Cloudflare returned one or more API-level errors.
    #[error("API error: {0}")]
    Api(String),

    /// The `wrangler` executable was not available on `PATH`.
    #[error("wrangler not installed. Install with: npm install -g wrangler")]
    WranglerNotInstalled,

    /// A `wrangler` command exited unsuccessfully.
    #[error("wrangler command failed: {0}")]
    WranglerFailed(String),

    /// Wrangler did not have an authenticated Cloudflare session.
    #[error("not logged in to Cloudflare. Run: wrangler login")]
    NotLoggedIn,

    /// A response or configuration file could not be parsed.
    #[error("failed to parse response: {0}")]
    Parse(String),

    /// The Cloudflare configuration directory could not be found.
    #[error("config directory not found")]
    ConfigDirNotFound,

    /// Filesystem IO failed while reading Cloudflare configuration.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The provided API token lacks permissions required by the requested endpoint.
    #[error(
        "API token missing required permissions for {0}. \
         Create a token at https://dash.cloudflare.com/profile/api-tokens \
         with the appropriate scopes and set it as CLOUDFLARE_API_TOKEN."
    )]
    InsufficientPermissions(String),
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
