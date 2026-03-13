//! The core `DurableObject` trait.

use std::future::Future;

use http_kit::Endpoint;
use serde::{de::DeserializeOwned, Serialize};

use super::context::DurableContext;
use super::error::DurableObjectError;
use super::websocket::{WebSocketConnection, WebSocketEvent};

/// A Durable Object with hibernation-first WebSocket support.
///
/// # Design
///
/// - **Struct IS the state**: Your struct must be `Serialize + DeserializeOwned + Default`.
///   On first creation, `Default::default()` produces the initial state.
///   On subsequent activations, the struct is deserialized from storage.
///   After each event, it is re-serialized.
///
/// - **`&mut self` everywhere**: Durable Objects have single-threaded serial execution.
///   No locks needed.
///
/// - **Two methods**: `fetch` for HTTP (returns `impl Endpoint`),
///   `websocket` for all hibernation WS events.
///
/// # Example
///
/// ```ignore
/// use serde::{Serialize, Deserialize};
/// use skyzen::durable::*;
/// use skyzen::routing::{CreateRouteNode, Route};
///
/// #[derive(Serialize, Deserialize, Default)]
/// struct Counter {
///     count: u64,
/// }
///
/// impl DurableObject for Counter {
///     fn fetch(&mut self) -> impl Endpoint {
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
    /// Build the endpoint for HTTP requests.
    ///
    /// Services (`DurableKv`, `DurableDb`, `Alarm`, `DurableConnections`)
    /// are available as extractors in the handlers.
    fn fetch(&mut self) -> impl Endpoint + 'static;

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

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::DurableObject;
    use crate::{
        routing::{CreateRouteNode, Route},
        Endpoint,
    };

    #[derive(Default, Serialize, Deserialize)]
    #[skyzen::durable_object]
    struct Counter;

    impl DurableObject for Counter {
        fn fetch(&mut self) -> impl Endpoint + 'static {
            Route::new(("/".at(|| async { "ok" }),)).build()
        }
    }

    #[test]
    fn durable_object_macro_compiles_on_native_targets() {
        let _ = Counter;
    }
}
