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
//!   - An `http-kit-io` backend (`AsyncIo`, `Tokio` or `Compio`) and the listener it registers
//!   - A thread-local executor (any `executor_core::LocalExecutor`) for the connections
//!   - The shutdown signal
//!
//! # Cargo.toml configuration
//!
//! ```toml
//! [dependencies]
//! # Only enable hyper feature, not rt
//! skyzen = { version = "0.1", default-features = false, features = ["json", "hyper"] }
//! smol = "2.0"
//! async-executor = "1.13"
//! http-kit-io = { version = "0.2", default-features = false, features = ["async-io"] }
//! async-channel = "2.5"
//! ctrlc = "3"
//! # Without `rt` nothing installs a log subscriber, so this application installs its own.
//! tracing = "0.1"
//! tracing-subscriber = "0.3"
//! ```
//!
//! Run with: `cargo run --example embed_hyper`

use async_executor::LocalExecutor;
use http_kit_io::{async_io::AsyncIo, FromStd as _};
use serde::Serialize;
use skyzen::{
    hyper::Hyper,
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    Server, ToSchema,
};
use std::rc::Rc;

#[derive(Serialize, ToSchema)]
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
    // `#[skyzen::main]` installs a tracing subscriber; this example does not use it, so it
    // installs one itself. Without this, every `tracing` event below — and every one the
    // framework emits — is dropped.
    tracing_subscriber::fmt().init();

    // The router is `!Send`: it lives on this thread, and so does everything that serves it — a
    // thread-local executor for the connections, and smol's reactor for the sockets.
    let router = build_router();
    let executor = Rc::new(LocalExecutor::new());

    let addr = "127.0.0.1:3000";
    let listener = AsyncIo.tcp_listener(std::net::TcpListener::bind(addr)?)?;
    tracing::info!("embedded Skyzen server listening on http://{addr}");
    tracing::info!("using the smol runtime, to demonstrate multi-runtime support");
    tracing::info!("try: curl http://{addr}/health");
    tracing::info!("try: curl http://{addr}/status");
    tracing::info!("try: curl http://{addr}/hello");

    // Serve until Ctrl+C: the accept loop stops, in-flight requests finish, then `serve` returns.
    let (stop, stopped) = async_channel::bounded::<()>(1);
    ctrlc::set_handler(move || {
        stop.close();
    })?;
    let serve = Hyper.serve(
        AsyncIo,
        Rc::clone(&executor),
        listener,
        router,
        async move {
            let _ = stopped.recv().await;
        },
    );
    smol::block_on(executor.run(serve));
    Ok(())
}
