//! Tests for the openapi macro

#[test]
fn openapi_macro_accepts_functions_only() {
    let t = trybuild::TestCases::new();
    t.pass("tests/fixtures/openapi_fn.rs");
    t.compile_fail("tests/fixtures/openapi_struct.rs");
}

#[test]
fn openapi_macro_handles_ignore_and_proxy_overrides() {
    let t = trybuild::TestCases::new();
    t.pass("tests/fixtures/openapi_ignore_and_proxy.rs");
}

#[test]
fn openapi_macro_requires_schema_without_overrides() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/fixtures/openapi_missing_schema.rs");
}
