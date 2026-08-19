//! Cloudflare Durable Object runtime and service adapters.

/// Storage key under which the framework persists serialized Durable Object
/// state. It lives in the same `state.storage` keyspace as user data, so
/// [`CfDurableKv`] hides it from `list` results and preserves it across
/// `delete_all`.
pub(crate) const STATE_KEY: &str = "__skyzen_do_state";

pub mod alarm;
pub mod glue;
pub mod kv;
pub mod namespace;
pub mod sql;
pub mod state;
pub mod websocket;

pub use alarm::CfAlarm;
pub use glue::{
    invoke_alarm, invoke_websocket_close, invoke_websocket_error, invoke_websocket_message,
    DurableObjectRuntime,
};
pub use kv::CfDurableKv;
pub use namespace::{CfDurableNamespace, CfDurableObjectStub};
pub use sql::CfDurableDb;
pub use state::CfDurableState;
pub use websocket::{CfDurableConnections, CfWebSocketConnection};
