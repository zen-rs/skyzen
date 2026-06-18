// WASM is single-threaded: JS types are `!Send` by design, and we use `unsafe impl Send/Sync`
// on wrappers that hold them. These lints are not applicable.
#![allow(clippy::future_not_send, clippy::non_send_fields_in_send_ty)]

//! Cloudflare Workers service implementations for the Skyzen framework.
//!
//! This crate provides Cloudflare Workers implementations of the service traits
//! defined in `skyzen-services`:
//!
//! - [`CfKv`] — Cloudflare KV (implements [`KeyValueStore`])
//! - [`CfR2`] — Cloudflare R2 (implements [`ObjectStorage`])
//! - [`CfQueue`] — Cloudflare Queues (implements [`MessageQueue`])
//! - [`CfD1`] — Cloudflare D1 SQL database
//! - [`CfCache`] — Cloudflare Cache API
//! - [`CfDurableSqlite`] — Durable Object `SQLite` (`state.storage.sql`)
//!
//! **This crate only works on `wasm32` targets.** On native targets it compiles
//! as an empty crate.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_cloudflare::{CfD1, CfKv, CfQueue, CfR2};
//! use skyzen_services::{Kv, Storage, Queue};
//!
//! // From a Workers env binding
//! let kv = Kv::new(CfKv::from_env(&env, "MY_KV")?);
//! let storage = Storage::new(CfR2::from_env(&env, "MY_BUCKET")?);
//! let queue = Queue::new(CfQueue::from_env(&env, "MY_QUEUE")?);
//! let d1 = CfD1::from_env(&env, "MY_D1")?;
//! ```
//!
//! [`KeyValueStore`]: skyzen_services::kv::KeyValueStore
//! [`ObjectStorage`]: skyzen_services::storage::ObjectStorage
//! [`MessageQueue`]: skyzen_services::queue::MessageQueue

#[cfg(target_arch = "wasm32")]
pub mod cache;
#[cfg(target_arch = "wasm32")]
pub mod d1;
#[cfg(target_arch = "wasm32")]
pub mod database_error;
#[cfg(target_arch = "wasm32")]
pub mod durable;
#[cfg(target_arch = "wasm32")]
pub mod durable_sqlite;
#[cfg(target_arch = "wasm32")]
pub mod events;
#[cfg(target_arch = "wasm32")]
pub mod fetch;
#[cfg(target_arch = "wasm32")]
pub mod ffi;
#[cfg(target_arch = "wasm32")]
pub mod http_request;
#[cfg(target_arch = "wasm32")]
pub mod kv;
#[cfg(target_arch = "wasm32")]
pub mod queues;
#[cfg(target_arch = "wasm32")]
pub mod r2;
#[cfg(target_arch = "wasm32")]
pub mod secret;

#[cfg(target_arch = "wasm32")]
pub use cache::{CfCache, CfCacheDeletionOutcome, CfCacheError};
#[cfg(target_arch = "wasm32")]
pub use d1::{CfD1, CfD1Statement};
#[cfg(target_arch = "wasm32")]
pub use database_error::CfDatabaseError;
#[cfg(target_arch = "wasm32")]
pub use durable::{
    invoke_alarm, invoke_websocket_close, invoke_websocket_error, invoke_websocket_message,
    CfAlarm, CfDurableConnections, CfDurableDb, CfDurableKv, CfDurableNamespace,
    CfDurableObjectStub, CfDurableState, CfWebSocketConnection, DurableObjectRuntime,
};
#[cfg(target_arch = "wasm32")]
pub use durable_sqlite::CfDurableSqlite;
#[cfg(target_arch = "wasm32")]
pub use events::{
    CfEventError, CfQueueBatch, CfQueueContext, CfQueueMessage, CfQueueRetryOptions,
    CfScheduleContext, CfScheduledEvent, IntoQueueWorkerResult, IntoWorkerResult,
};
#[cfg(target_arch = "wasm32")]
pub use fetch::{CfFetch, CfFetchError};
#[cfg(target_arch = "wasm32")]
pub use http_request::{bare_request, json_request, CfHttpRequestError};
#[cfg(target_arch = "wasm32")]
pub use kv::CfKv;
#[cfg(target_arch = "wasm32")]
pub use queues::CfQueue;
#[cfg(target_arch = "wasm32")]
pub use r2::CfR2;
#[cfg(target_arch = "wasm32")]
pub use secret::{
    optional_string as optional_secret, required_string as required_secret, CfSecretError,
};
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use worker;
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use worker_sys;

#[cfg(target_arch = "wasm32")]
const _: () = {
    fn assert_send<T: Send>(_: T) {}

    #[allow(dead_code)]
    fn assert_cloudflare_handler_futures_are_send(
        d1: crate::CfD1,
        namespace: crate::CfDurableNamespace,
        cache: crate::CfCache,
        fetch: crate::CfFetch,
        request: worker::Request,
    ) {
        assert_send(d1.exec("SELECT 1"));
        assert_send(
            crate::CfD1::new(wasm_bindgen::JsValue::NULL)
                .prepare("SELECT 1")
                .unwrap()
                .all(),
        );
        assert_send(
            namespace
                .get_by_name("room")
                .unwrap()
                .fetch_url("https://example.com"),
        );
        assert_send(cache.get_url("https://example.com", false));
        assert_send(cache.put_url("https://example.com", worker::Response::empty().unwrap()));
        assert_send(fetch.request(&request));
        assert_send(fetch.request_bytes(&request));
    }
};
