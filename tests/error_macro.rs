//! Display interpolation behavior of `#[skyzen::error]`.

#[skyzen::error(message = "artifact error")]
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
