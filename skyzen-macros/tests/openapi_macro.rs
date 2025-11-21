//! Tests for the openapi macro

#[test]
fn openapi_macro_accepts_functions_only() {
    let t = trybuild::TestCases::new();
    t.pass("tests/fixtures/openapi_fn.rs");
    t.compile_fail("tests/fixtures/openapi_struct.rs");
}
