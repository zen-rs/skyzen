//! Native example that takes advantage of Skyzen's router/extractor/responder
//! APIs. Run with `cargo run --example native`.

use serde::{Deserialize, Serialize};
use skyzen::{
    extract::Query,
    routing::{CreateRouteNode, Params, Route, Router},
    utils::Json,
    Result as SkyResult,
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

async fn greet_from_path(params: Params) -> SkyResult<Json<Greeting>> {
    let name = params.get("name")?;
    Ok(Json(Greeting {
        message: format!("Hello, {name}!"),
    }))
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
