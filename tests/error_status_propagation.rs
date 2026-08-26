//! Regression tests for the status a typed error keeps on its way out of a handler.
//!
//! Every error in the framework carries an HTTP status. These tests pin the whole path — `?` into
//! `skyzen::Result`, the responder, and `error_response`'s redaction policy — so a client error
//! can never silently degrade into a redacted `500` again.

use skyzen::{
    routing::{CreateRouteNode, Params, Route, Router},
    Context, Result, ResultExt, StatusCode,
};
use skyzen_services::{DbError, KvError};
use skyzen_test::TestContext;

/// Stands in for a query whose row is absent; the natural response is 404.
fn lookup_row() -> core::result::Result<String, DbError> {
    Err(DbError::RowNotFound)
}

/// Stands in for a backend that cannot honour the requested operation; the response is 501.
fn store_with_ttl() -> core::result::Result<(), KvError> {
    Err(KvError::Unsupported("time-to-live"))
}

/// Stands in for a backend failure whose detail must never reach the client.
fn read_secret() -> core::result::Result<String, KvError> {
    Err(KvError::backend("connecting to redis://hunter2@cache"))
}

/// Reads a route parameter that the route never declares, so `Params::get` rejects with a 400.
async fn read_missing_param(params: Params) -> Result<String> {
    let id = params.get("id")?;
    Ok(id.to_owned())
}

/// Adds a breadcrumb to a client error; the status must survive the extra layer.
async fn read_missing_param_with_context(params: Params) -> Result<String> {
    let id = params.get("id").context("loading the requested user")?;
    Ok(id.to_owned())
}

async fn read_missing_row() -> Result<String> {
    Ok(lookup_row()?)
}

async fn write_with_ttl() -> Result<&'static str> {
    store_with_ttl()?;
    Ok("stored")
}

async fn read_backend_secret() -> Result<String> {
    Ok(read_secret()?)
}

/// States a status for a value that carries none of its own.
async fn read_missing_state() -> Result<String> {
    let value: Option<String> = None;
    value.status(StatusCode::NOT_FOUND)
}

/// A plain standard error has no HTTP meaning, so the handler states one and adds a breadcrumb.
async fn read_missing_file() -> Result<String> {
    std::fs::read_to_string("/nonexistent/skyzen/config.toml")
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .context("loading the application config")
}

fn app() -> Router {
    Route::new((
        "/param".at(read_missing_param),
        "/param-context".at(read_missing_param_with_context),
        "/row".at(read_missing_row),
        "/ttl".at(write_with_ttl),
        "/secret".at(read_backend_secret),
        "/state".at(read_missing_state),
        "/file".at(read_missing_file),
    ))
    .build()
}

#[tokio::test]
async fn missing_param_keeps_its_bad_request_status_and_message() {
    let response = TestContext::new().client(app()).get("/param").send().await;

    response.assert_status(400);
    response.assert_json_path("error", &serde_json::json!("Missing param `id`"));
}

#[tokio::test]
async fn context_preserves_the_status_and_replaces_the_message() {
    let response = TestContext::new()
        .client(app())
        .get("/param-context")
        .send()
        .await;

    response.assert_status(400);
    response.assert_json_path("error", &serde_json::json!("loading the requested user"));
}

#[tokio::test]
async fn missing_row_becomes_a_not_found() {
    let response = TestContext::new().client(app()).get("/row").send().await;

    response.assert_status(404);
    response.assert_body_contains("row not found");
}

#[tokio::test]
async fn unsupported_backend_operation_becomes_not_implemented() {
    let response = TestContext::new().client(app()).get("/ttl").send().await;

    // 501 is still a 5xx, so the policy redacts the detail while keeping the status honest.
    response.assert_status(501);
    response.assert_json_path("error", &serde_json::json!("Internal server error"));
}

#[tokio::test]
async fn backend_failures_are_still_redacted() {
    let response = TestContext::new().client(app()).get("/secret").send().await;

    response.assert_status(500);
    response.assert_json_path("error", &serde_json::json!("Internal server error"));
    assert!(
        !response.body_text().contains("hunter2"),
        "5xx bodies must never leak backend detail"
    );
}

#[tokio::test]
async fn plain_errors_take_the_status_the_caller_states() {
    let response = TestContext::new().client(app()).get("/file").send().await;

    // 503 is a 5xx, so the breadcrumb stays in the log and the body stays generic.
    response.assert_status(503);
    response.assert_json_path("error", &serde_json::json!("Internal server error"));
}

#[tokio::test]
async fn result_ext_status_states_a_status_for_a_bare_option() {
    let response = TestContext::new().client(app()).get("/state").send().await;

    response.assert_status(404);
    response.assert_json_path("error", &serde_json::json!("Not Found"));
}
