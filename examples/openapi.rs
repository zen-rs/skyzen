//! Demonstrates the `#[skyzen::openapi]` attribute and router introspection APIs.

use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::to_string;
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    OpenApi, ToSchema,
};

#[derive(Debug, Deserialize, ToSchema)]
struct HelloQuery {
    name: String,
}

#[derive(Debug, Serialize, ToSchema)]
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
    fn schema_to_string<T: Serialize>(schema: &T) -> String {
        to_string(schema).unwrap_or_else(|err| format!("<invalid schema: {err}>"))
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
            println!("  param[{idx}]: {}", schema_to_string(schema));
        }

        match &op.response {
            Some(schema) => println!("  response: {}", schema_to_string(schema)),
            None => println!("  response: <ignored>"),
        }
    }
}

#[skyzen::main]
fn main() -> Router {
    let redoc_endpoint = Route::new(("/hello".at(hello),)).openapi().redoc();
    let router = Route::new((
        "/hello".at(hello),
        // Serve interactive docs at GET /docs via utoipa-redoc.
        "/docs".endpoint(Method::GET, redoc_endpoint),
    ))
    .build();
    let openapi = router.openapi();
    println!("OpenAPI enabled: {}", openapi.is_enabled());
    println!("ReDoc endpoint mounted at GET /docs");
    log_openapi(&openapi);
    router
}
