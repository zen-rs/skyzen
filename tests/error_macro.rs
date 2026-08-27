//! Integration tests for the `#[skyzen::error]` attribute macro: Display
//! interpolation behavior and the generated `HttpError::status()`
//! implementation (enum-level defaults and per-variant overrides).

use skyzen::{HttpError, StatusCode};

#[skyzen::error]
enum SampleError {
    #[error("bad request", status = BAD_REQUEST)]
    BadRequest,
    #[error("internal server error: {0}")]
    WithPositional(String),
    #[error("lookup failed for {name} (attempt {attempt})", status = NOT_FOUND)]
    WithNamed { name: String, attempt: u32 },
    #[error("mixed {1} then {0}")]
    Reordered(u32, &'static str),
    #[error("literal {{braces}} stay")]
    EscapedBraces,
}

#[skyzen::error(message = "shim rejected {0}", status = BAD_GATEWAY)]
struct StructError(String);

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
fn unit_variant_renders_plain_message() {
    assert_eq!(SampleError::BadRequest.to_string(), "bad request");
}

#[test]
fn positional_placeholder_renders_field() {
    assert_eq!(
        SampleError::WithPositional("d1 query failed".to_owned()).to_string(),
        "internal server error: d1 query failed"
    );
}

#[test]
fn named_placeholders_render_fields() {
    assert_eq!(
        SampleError::WithNamed {
            name: "serde".to_owned(),
            attempt: 3,
        }
        .to_string(),
        "lookup failed for serde (attempt 3)"
    );
}

#[test]
fn positional_placeholders_may_reorder() {
    assert_eq!(
        SampleError::Reordered(7, "seven").to_string(),
        "mixed seven then 7"
    );
}

#[test]
fn escaped_braces_render_literally() {
    assert_eq!(
        SampleError::EscapedBraces.to_string(),
        "literal {braces} stay"
    );
}

#[test]
fn struct_error_renders_field() {
    assert_eq!(
        StructError("payload".to_owned()).to_string(),
        "shim rejected payload"
    );
}

#[test]
fn shorthand_statuses_resolve_from_attributes() {
    assert_eq!(SampleError::BadRequest.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        SampleError::WithNamed {
            name: String::new(),
            attempt: 0,
        }
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(StructError(String::new()).status(), StatusCode::BAD_GATEWAY);
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

/// A wrapped cause, so `source()` has something concrete to hand back.
#[derive(Debug)]
struct Cause(&'static str);

impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Cause {}

/// `#[from]` and `#[source]` both surface the wrapped error as the cause.
#[skyzen::error]
enum WrappingError {
    /// Converts from `Cause` and reports it as the cause.
    #[error("upstream call failed", status = BAD_GATEWAY)]
    Upstream(#[from] Cause),

    /// Reports the cause without generating a conversion.
    #[error("writing {key} failed")]
    Write {
        key: &'static str,
        #[source]
        cause: Cause,
    },

    /// Carries no cause at all.
    #[error("nothing to report", status = NOT_FOUND)]
    Bare,
}

/// `#[from]` on a struct error generates the conversion and the cause.
#[skyzen::error(message = "shim failed", status = BAD_GATEWAY)]
struct ShimError(#[from] Cause);

#[test]
fn from_variant_reports_the_wrapped_error_as_source() {
    let error = WrappingError::from(Cause("socket closed"));
    assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(error.to_string(), "upstream call failed");
    assert_eq!(
        std::error::Error::source(&error)
            .expect("#[from] field is the source")
            .to_string(),
        "socket closed"
    );
}

#[test]
fn source_marked_field_reports_the_wrapped_error_as_source() {
    let error = WrappingError::Write {
        key: "meta:1",
        cause: Cause("disk full"),
    };
    assert_eq!(error.to_string(), "writing meta:1 failed");
    assert_eq!(
        std::error::Error::source(&error)
            .expect("#[source] field is the source")
            .to_string(),
        "disk full"
    );
}

#[test]
fn variant_without_a_marked_field_has_no_source() {
    assert!(std::error::Error::source(&WrappingError::Bare).is_none());
}

#[test]
fn struct_error_reports_its_from_field_as_source() {
    let error = ShimError::from(Cause("handshake rejected"));
    assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(error.to_string(), "shim failed");
    assert_eq!(
        std::error::Error::source(&error)
            .expect("#[from] field is the source")
            .to_string(),
        "handshake rejected"
    );
}
