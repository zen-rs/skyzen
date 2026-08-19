//! Integration tests for the `#[skyzen::error]` attribute macro, asserting that the generated
//! `HttpError::status()` implementation honors enum-level defaults and per-variant overrides.

use skyzen::{HttpError, StatusCode};

/// Enum-level default status with per-variant overrides.
#[skyzen::error(status = StatusCode::BAD_REQUEST)]
enum ApiError {
    /// Inherits the enum-level `BAD_REQUEST` default.
    #[error("bad input")]
    BadInput,

    /// Overrides the default with a named status constant.
    #[error("not found", status = StatusCode::NOT_FOUND)]
    NotFound,

    /// Overrides the default with another named status constant.
    #[error("upstream failed", status = StatusCode::BAD_GATEWAY)]
    Upstream,

    /// Overrides the default with a numeric status literal.
    #[error("teapot", status = 418)]
    Teapot,
}

/// No enum-level status: variants without `status =` fall back to 500.
#[skyzen::error]
enum PlainError {
    /// Uses the implicit `INTERNAL_SERVER_ERROR` default.
    #[error("boom")]
    Boom,

    /// Explicit status still wins over the implicit default.
    #[error("missing", status = StatusCode::NOT_FOUND)]
    Missing,
}

#[test]
fn variant_statuses_resolve_from_attributes() {
    assert_eq!(ApiError::BadInput.status(), StatusCode::BAD_REQUEST);
    assert_eq!(ApiError::NotFound.status(), StatusCode::NOT_FOUND);
    assert_eq!(ApiError::Upstream.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(ApiError::Teapot.status(), StatusCode::IM_A_TEAPOT);
}

#[test]
fn variant_without_status_defaults_to_internal_server_error() {
    assert_eq!(PlainError::Boom.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(PlainError::Missing.status(), StatusCode::NOT_FOUND);
}

#[test]
fn display_messages_match_error_attributes() {
    assert_eq!(ApiError::BadInput.to_string(), "bad input");
    assert_eq!(ApiError::NotFound.to_string(), "not found");
    assert_eq!(ApiError::Upstream.to_string(), "upstream failed");
    assert_eq!(PlainError::Boom.to_string(), "boom");
}
