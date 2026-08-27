//! Cloudflare Durable Object runtime and service adapters.
//!
//! # Transactions, and why there is no `transaction()` here
//!
//! Durable Object storage has a `transaction(closure)` method, and this crate deliberately does
//! not bind it. Cloudflare's own storage documentation says explicit transactions "are no longer
//! necessary. Any series of write operations with no intervening `await` will automatically be
//! submitted atomically", and calls the transaction object "obsolete" for the SQLite-backed
//! storage every new Durable Object uses. Binding it would add an API whose whole job is to
//! restate a guarantee the platform already gives.
//!
//! What to use instead, in the order you should reach for them:
//!
//! 1. **Write without awaiting in between.** A run of `put`/`delete` calls with no `await`
//!    between them is already atomic. This covers most of what a transaction was for.
//! 2. **[`CfDurableState::block_concurrency_while`]** when the atomic section has to span an
//!    `await` — initialization that must finish before the first request is served, or a
//!    read-modify-write that calls out and back. It closes the object's input gates, so no other
//!    event interleaves.
//! 3. **The input and output gates you already have.** Between events, the runtime holds incoming
//!    events during a storage operation and holds outgoing messages until writes are confirmed;
//!    [`DurableWriteOptions`] is what opts *out* of those, and is the only way to lose them.
//!
//! See <https://developers.cloudflare.com/durable-objects/api/storage-api/>.

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
pub use kv::{CfDurableKv, DurableWriteOptions};
pub use namespace::{CfDurableNamespace, CfDurableObjectStub, CfJurisdiction};
pub use sql::{CfDurableDb, CfSqlCursor};
pub use state::{AbortOptions, CfDurableState};
pub use websocket::{CfDurableConnections, CfWebSocketConnection};
