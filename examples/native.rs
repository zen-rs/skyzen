//! Native example that takes advantage of Skyzen's router/extractor/responder
//! APIs. Run with `cargo run --example native`.

use serde::{Deserialize, Serialize};
use skyzen::{
    extract::{Path, Query},
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
};

#[derive(Debug, Serialize)]
struct Greeting {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GreetingQuery {
    name: Option<String>,
    excited: Option<bool>,
}

async fn home() -> &'static str {
    "Visit /hello?name=Skyzen or /hello/Skyzen for a personalized greeting."
}

/// `Path<T>` deserializes the captured segment, so a handler never parses a string itself. Here
/// `T` is `String`, but `Path<u64>` or `Path<(String, u64)>` works the same way and answers a
/// malformed segment with a `400` naming it.
async fn greet_from_path(Path(name): Path<String>) -> Json<Greeting> {
    Json(Greeting {
        message: format!("Hello, {name}!"),
    })
}

async fn greet_from_query(Query(query): Query<GreetingQuery>) -> Json<Greeting> {
    let name = query.name.unwrap_or_else(|| "friend".to_owned());
    let mut message = format!("Hello, {name}");
    if query.excited.unwrap_or(false) {
        message.push('!');
    }
    Json(Greeting { message })
}

async fn healthz() -> &'static str {
    "OK"
}

fn build_router() -> Router {
    Route::new((
        "/".at(home),
        "/healthz".at(healthz),
        "/hello".at(greet_from_query),
        "/hello".route(("/{name}".at(greet_from_path),)),
    ))
    .build()
}

#[skyzen::main]
fn main() -> Router {
    build_router()
}
