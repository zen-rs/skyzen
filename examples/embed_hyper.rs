//! Embedding Skyzen into your own application using the `hyper` feature.
//!
//! This example demonstrates that Skyzen supports multiple async runtimes.
//! Here we use **smol** instead of tokio to show runtime flexibility.
//!
//! # When to use this approach
//!
//! Use this approach when you:
//! - Already have an existing smol/async-std/tokio application
//! - Need fine-grained control over the server lifecycle
//! - Want to integrate Skyzen alongside other services in the same process
//! - Cannot use the `#[skyzen::main]` macro
//!
//! # Feature flags
//!
//! - `rt` - Enables the built-in runtime for `#[skyzen::main]`. This provides:
//!   - Logging setup via tracing
//!   - Signal handling (ctrl+c graceful shutdown)
//!   - HTTP server via hyper + tokio
//!   - CLI argument parsing for port configuration
//!
//! - `hyper` - Enables only the Hyper server adapter (`skyzen::hyper`). Use this when
//!   embedding Skyzen into your own application. You provide your own:
//!   - Async runtime (smol, tokio, async-std, etc.)
//!   - TCP listener
//!   - Executor for background tasks
//!
//! # Cargo.toml configuration
//!
//! ```toml
//! [dependencies]
//! # Only enable hyper feature, not rt
//! skyzen = { version = "0.1", default-features = false, features = ["json", "hyper"] }
//! smol = "2.0"
//! async-net = "2.0"
//! futures-lite = "2.0"
//! ```
//!
//! Run with: `cargo run --example embed_hyper`

use async_net::TcpListener;
use futures_lite::stream;
use serde::Serialize;
use skyzen::{
    hyper::Hyper,
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    Server,
};

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    runtime: &'static str,
}

async fn health() -> &'static str {
    "OK"
}

async fn status() -> Json<StatusResponse> {
    Json(StatusResponse {
        status: "running",
        runtime: "smol",
    })
}

async fn hello() -> &'static str {
    "Hello from Skyzen on smol runtime!"
}

/// Build the Skyzen router with your application's routes
fn build_router() -> Router {
    Route::new((
        "/health".at(health),
        "/status".at(status),
        "/hello".at(hello),
    ))
    .build()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use smol's global executor
    smol::block_on(async {
        // Build the Skyzen router
        let router = build_router();

        // Bind to a TCP listener using async-net (smol-compatible)
        let addr = "127.0.0.1:3000";
        let listener = TcpListener::bind(addr).await?;
        println!("Embedded Skyzen server listening on http://{addr}");
        println!("Using smol runtime to demonstrate multi-runtime support");
        println!();
        println!("Try these endpoints:");
        println!("  curl http://{addr}/health");
        println!("  curl http://{addr}/status");
        println!("  curl http://{addr}/hello");

        // Convert TcpListener to an owned Stream of connections
        let connections = Box::pin(stream::unfold(listener, |listener| async move {
            let result = listener.accept().await;
            Some((result.map(|(stream, _addr)| stream), listener))
        }));

        // Serve using the Hyper backend with smol's executor
        Hyper
            .serve(
                smol::Executor::new(),
                |err| eprintln!("Connection error: {err}"),
                connections,
                router,
            )
            .await;

        Ok(())
    })
}
