//! Demonstrates the `#[skyzen::openapi]` attribute and router introspection APIs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Route},
    utils::Json,
    OpenApi,
};

#[derive(Debug, Deserialize, JsonSchema)]
struct HelloQuery {
    name: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct HelloResponse {
    message: String,
}

/// Greets the caller and exposes an `OpenAPI` operation.
#[skyzen::openapi]
async fn hello(Json(query): Json<HelloQuery>) -> skyzen::Result<Json<HelloResponse>> {
    Ok(Json(HelloResponse {
        message: format!("Hello, {}!", query.name),
    }))
}

fn log_openapi(spec: &OpenApi) {
    if !spec.is_enabled() {
        println!("OpenAPI instrumentation disabled (release build).");
        return;
    }

    for op in spec.operations() {
        println!(
            "{} {} handled by {}",
            op.method.as_str(),
            op.path,
            op.handler_type
        );

        if let Some(docs) = op.docs {
            println!("  docs: {docs}");
        }

        for (idx, schema) in op.parameters.iter().enumerate() {
            println!("  param[{idx}]: {:?}", schema);
        }

        println!("  response: {:?}", op.response);
    }
}

fn main() {
    let router = Route::new(("/hello".at(hello),)).build();
    let openapi = router.openapi();
    println!("OpenAPI enabled: {}", openapi.is_enabled());
    log_openapi(&openapi);
}
