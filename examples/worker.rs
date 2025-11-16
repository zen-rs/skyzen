//! A simple example.

use skyzen::{
    routing::{CreateRouteNode, Params, Route, Router},
    Result,
};

async fn health() -> &'static str {
    "OK"
}

async fn greet(params: Params) -> Result<String> {
    let name = params.get("name")?;
    Ok(format!("Hello, {name}!"))
}

#[skyzen::main]
fn main() -> Router {
    Route::new(("/health".at(health), "/hello".route(("/{name}".at(greet),)))).build()
}
