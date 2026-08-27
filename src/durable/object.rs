//! The core `DurableObject` trait.

use std::future::Future;

use serde::{de::DeserializeOwned, Serialize};

use super::context::DurableContext;
use super::error::DurableObjectError;
use super::websocket::{WebSocketConnection, WebSocketEvent};
use crate::routing::Router;

/// A Durable Object with hibernation-first WebSocket support.
///
/// # Design
///
/// - **Struct IS the state**: Your struct must be `Serialize + DeserializeOwned + Default`.
///   On first creation, `Default::default()` produces the initial state.
///   On subsequent activations, the struct is deserialized from storage.
///   After each event, it is re-serialized.
///   See [`PERSIST`](Self::PERSIST) for where that model stops scaling and how to opt out.
///
/// - **`&mut self` everywhere**: Durable Objects have single-threaded serial execution.
///   No locks needed.
///
/// - **Two methods**: `fetch` for HTTP (returns a [`Router`]),
///   `websocket` for all hibernation WS events.
///
/// # Example
///
/// ```ignore
/// use serde::{Serialize, Deserialize};
/// use skyzen::durable::*;
/// use skyzen::routing::{CreateRouteNode, Route, Router};
///
/// #[derive(Serialize, Deserialize, Default)]
/// struct Counter {
///     count: u64,
/// }
///
/// impl DurableObject for Counter {
///     fn fetch(&mut self) -> Router {
///         Route::new((
///             "/increment".at(increment),
///         ))
///         .build()
///     }
/// }
///
/// async fn increment() -> skyzen::Result<String> {
///     Ok("incremented".to_string())
/// }
/// ```
pub trait DurableObject: Serialize + DeserializeOwned + Default + Sized + 'static {
    /// Whether the framework loads and stores `Self` around every event.
    ///
    /// # What `true` costs
    ///
    /// With the default, the whole object is read from one storage value and JSON-parsed before
    /// **every** event — each fetch, each alarm, each websocket message — and serialized again
    /// afterwards. For a counter that is nothing. For a chat room holding a message history it is
    /// a full parse and a full serialize per websocket frame, and the object lives in a single
    /// storage value, so it is bounded by the per-value limit rather than by the storage size.
    ///
    /// # Setting it to `false`
    ///
    /// An object that keeps its state in storage directly — through the
    /// [`DurableKv`](skyzen_services::durable::DurableKv) or
    /// [`DurableDb`](skyzen_services::durable::DurableDb) extractors — has nothing for the
    /// framework to serialize, and setting `PERSIST = false` skips the load/parse/serialize/save
    /// round trip entirely. The struct then holds only per-activation scratch, and
    /// `Default::default()` produces it on every event.
    ///
    /// This is the path to take for anything that grows: `DurableDb` is backed by the `SQLite`
    /// storage Cloudflare now provisions for new Durable Object classes, so rows are read and
    /// written individually instead of the whole object being rewritten to change one field.
    ///
    /// ```ignore
    /// #[derive(Serialize, Deserialize, Default)]
    /// struct Room;
    ///
    /// impl DurableObject for Room {
    ///     // Messages live in SQLite via `DurableDb`; there is no blob to round-trip.
    ///     const PERSIST: bool = false;
    ///
    ///     fn fetch(&mut self) -> Router { /* … */ }
    /// }
    /// ```
    const PERSIST: bool = true;

    /// Build the [`Router`] handling HTTP requests (and, via [`Route::on_alarm`](crate::routing::Route::on_alarm),
    /// alarm events).
    ///
    /// Services (`DurableKv`, `DurableDb`, `Alarm`, `DurableConnections`)
    /// are available as extractors in the handlers.
    fn fetch(&mut self) -> Router;

    /// Handle all WebSocket Hibernation events.
    ///
    /// Called by the runtime on `webSocketMessage`, `webSocketClose`, `webSocketError`.
    /// `ctx` provides service access (not routed through Router, so no extractors).
    ///
    /// Default: no-op. DOs without WebSocket don't need to implement this.
    fn websocket(
        &mut self,
        _ws: &WebSocketConnection,
        _event: WebSocketEvent,
        _ctx: &DurableContext,
    ) -> impl Future<Output = Result<(), DurableObjectError>> + Send {
        async { Ok(()) }
    }
}
