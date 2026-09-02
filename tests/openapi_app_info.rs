//! The identity a document is titled with comes from a registration, not from an argument.
//!
//! `#[skyzen::main]` emits `__register_app_info!` in the application's crate, where
//! `env!("CARGO_PKG_NAME")` reads the application rather than skyzen. This file stands in for that
//! application: it registers an identity the same way, and checks the document picks it up with
//! nothing passed through the routing API.
//!
//! It is a file of its own because a registration is per *binary*. The anonymous fallback — what a
//! library under test or an embedded runtime gets — can only be observed in a binary that
//! registered nothing, and is asserted in the crate's own unit tests instead.

use skyzen::{
    openapi::AppInfo,
    routing::{CreateRouteNode, Route},
};

skyzen::__register_app_info!(
    APP_INFO,
    AppInfo {
        name: "orders-service",
        version: "4.2.0",
        description: Some("Order placement and fulfilment"),
    }
);

async fn health() -> &'static str {
    "OK"
}

#[test]
fn a_document_is_titled_by_what_was_registered_not_by_an_argument() {
    let spec = Route::new(("/health".at(health),))
        .openapi()
        .to_utoipa_spec();

    assert_eq!(spec.info.title, "orders-service");
    assert_eq!(spec.info.version, "4.2.0");
    assert_eq!(
        spec.info.description.as_deref(),
        Some("Order placement and fulfilment")
    );
}

#[test]
fn with_info_overrides_the_registration() {
    // For an API whose public name is not its crate's.
    let spec = Route::new(("/health".at(health),))
        .openapi()
        .with_info(AppInfo {
            name: "Orders API",
            version: "2",
            description: None,
        })
        .to_utoipa_spec();

    assert_eq!(spec.info.title, "Orders API");
    assert_eq!(spec.info.version, "2");
    assert_eq!(spec.info.description, None);
}
