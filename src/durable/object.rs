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
/// - **One value per instance**: the platform constructs a Durable Object instance once and
///   delivers every event to it until it is evicted, and the struct follows the same lifetime.
///   It is built on the instance's first event — from storage when it [persists](Self::PERSIST),
///   from `Default` otherwise — and every later event on that instance sees the same value, so a
///   field set by one event is there for the next.
///
/// - **`&self` everywhere**: events on one instance interleave at every `await` — the platform
///   serializes them only around storage operations — so a handler holds the object shared, never
///   exclusively. State an event changes lives behind interior mutability, and [`fetch`](Self::fetch)
///   hands handlers what they need as `State<…>` clones of the object's own handles.
///
/// - **Struct IS the state**, when it persists: a type with the default
///   [`PERSIST`](Self::PERSIST) must be `Serialize + DeserializeOwned + Default`, is read from
///   storage when the instance first receives an event, and is written back after every
///   successful event whose serialization changed.
///
/// - **Two methods**: `fetch` for HTTP (returns a [`Router`]),
///   `websocket` for all hibernation WS events.
///
/// # Example
///
/// ```ignore
/// use std::sync::atomic::{AtomicU64, Ordering};
///
/// use serde::{Serialize, Deserialize};
/// use skyzen::durable::*;
/// use skyzen::routing::{CreateRouteNode, Route, Router};
///
/// #[derive(Serialize, Deserialize, Default)]
/// struct Counter {
///     count: AtomicU64,
/// }
///
/// impl DurableObject for Counter {
///     fn fetch(&self) -> Router {
///         self.count.fetch_add(1, Ordering::Relaxed);
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
    /// Whether the framework loads `Self` from storage and stores it back.
    ///
    /// # What `true` costs
    ///
    /// With the default, the whole object is read from one storage value and JSON-parsed on the
    /// instance's first event, and serialized again after **every** event — each fetch, each
    /// alarm, each websocket message — to see whether it changed. For a counter that is nothing.
    /// For a chat room holding a message history it is a full serialize per websocket frame, and
    /// the object lives in a single storage value, so it is bounded by the per-value limit rather
    /// than by the storage size.
    ///
    /// # Setting it to `false`
    ///
    /// An object that keeps its state in storage directly — through the
    /// [`DurableKv`](skyzen_services::durable::DurableKv) or
    /// [`DurableDb`](skyzen_services::durable::DurableDb) extractors — has nothing for the
    /// framework to serialize, and setting `PERSIST = false` skips the load/parse/serialize/save
    /// round trip entirely. The struct then holds only what the instance keeps in memory —
    /// caches, subscriptions, handles a held stream waits on — and `Default::default()` produces
    /// it once, on the instance's first event.
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
    ///     fn fetch(&self) -> Router { /* … */ }
    /// }
    /// ```
    const PERSIST: bool = true;

    /// Build the [`Router`] handling HTTP requests (and, via [`Route::on_alarm`](crate::routing::Route::on_alarm),
    /// alarm events).
    ///
    /// Services (`DurableKv`, `DurableDb`, `Alarm`, `DurableConnections`)
    /// are available as extractors in the handlers.
    fn fetch(&self) -> Router;

    /// Handle all WebSocket Hibernation events.
    ///
    /// Called by the runtime on `webSocketMessage`, `webSocketClose`, `webSocketError`.
    /// `ctx` provides service access (not routed through Router, so no extractors).
    ///
    /// Default: no-op. DOs without WebSocket don't need to implement this.
    fn websocket(
        &self,
        _ws: &WebSocketConnection,
        _event: WebSocketEvent,
        _ctx: &DurableContext,
    ) -> impl Future<Output = Result<(), DurableObjectError>> + Send {
        async { Ok(()) }
    }
}
