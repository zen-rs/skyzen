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
//! - [`CfService`] — Cloudflare service bindings (Worker-to-Worker over HTTP)
//! - [`CfAssets`] — Cloudflare Static Assets binding
//! - [`CfSecretStore`] — Cloudflare Secrets Store bindings
//! - [`CfCache`] — Cloudflare Cache API
//! - [`CfDurableDb`] — Durable Object `SQLite` (`state.storage.sql`)
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

/// Implement `Clone` (via the underlying JS handle), `Send`/`Sync`, and an
/// opaque `Debug` for a single-field wrapper around a wasm-bindgen JS handle.
///
/// # Safety
///
/// Workers WASM executes on a single thread, so marking JS handles `Send` and
/// `Sync` is sound in this environment.
#[cfg(target_arch = "wasm32")]
macro_rules! impl_js_handle_traits {
    ($type:ident { $field:ident }) => {
        impl Clone for $type {
            fn clone(&self) -> Self {
                let js: &::wasm_bindgen::JsValue = self.$field.as_ref();
                Self {
                    $field: ::wasm_bindgen::JsCast::unchecked_into(js.clone()),
                }
            }
        }

        // SAFETY: Workers WASM executes on a single thread; JS handles are
        // safe to mark Send/Sync.
        unsafe impl Send for $type {}
        unsafe impl Sync for $type {}

        impl ::std::fmt::Debug for $type {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_struct(stringify!($type)).finish_non_exhaustive()
            }
        }
    };
}

#[cfg(target_arch = "wasm32")]
pub mod cache;
#[cfg(target_arch = "wasm32")]
pub mod d1;
#[cfg(target_arch = "wasm32")]
pub mod database_error;
#[cfg(target_arch = "wasm32")]
pub mod durable;
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
pub mod service;

#[cfg(target_arch = "wasm32")]
pub use cache::{CfCache, CfCacheDeletionOutcome, CfCacheError};
#[cfg(target_arch = "wasm32")]
pub use d1::{CfD1, CfD1Statement};
#[cfg(target_arch = "wasm32")]
pub use database_error::CfDatabaseError;
#[cfg(target_arch = "wasm32")]
pub use durable::{
    invoke_alarm, invoke_websocket_close, invoke_websocket_error, invoke_websocket_message,
    AbortOptions, CfAlarm, CfDurableConnections, CfDurableDb, CfDurableKv, CfDurableNamespace,
    CfDurableObjectStub, CfDurableState, CfJurisdiction, CfSqlCursor, CfWebSocketConnection,
    DurableObjectRuntime, DurableWriteOptions,
};
#[cfg(target_arch = "wasm32")]
pub use events::{
    CfEmailMessage, CfEventContext, CfEventError, CfQueueBatch, CfQueueMessage,
    CfQueueRetryOptions, CfScheduleContext, CfScheduledEvent, CfTailEvent, IntoQueueWorkerResult,
    IntoWorkerResult, TailException, TailLog, TailTraceItem,
};
#[cfg(target_arch = "wasm32")]
pub use fetch::{CfFetch, CfFetchError, CfFetchOptions};
#[cfg(target_arch = "wasm32")]
pub use http_request::{bare_request, json_request, CfHttpRequestError};
#[cfg(target_arch = "wasm32")]
pub use kv::{CfKv, CfKvPutOptions, CfKvValueWithMetadata};
#[cfg(target_arch = "wasm32")]
pub use queues::CfQueue;
#[cfg(target_arch = "wasm32")]
pub use r2::{
    CfR2, CfR2Conditional, CfR2ConditionalGet, CfR2ListOptions, CfR2ListResult,
    CfR2MultipartUpload, CfR2UploadedPart,
};
#[cfg(target_arch = "wasm32")]
pub use secret::{
    optional_string as optional_secret, required_string as required_secret, CfSecretError,
    CfSecretStore,
};
#[cfg(target_arch = "wasm32")]
pub use service::{CfAssets, CfService, CfServiceError};
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use worker;
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use worker_sys;

#[cfg(target_arch = "wasm32")]
const _: () = {
    fn assert_send<T: Send>(_: T) {}

    #[allow(dead_code, clippy::needless_pass_by_value)]
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

    #[allow(dead_code, clippy::needless_pass_by_value)]
    fn assert_wave_e_futures_are_send(
        kv: crate::CfKv,
        r2: crate::CfR2,
        d1: crate::CfD1,
        service: crate::CfService,
        request: worker::Request,
    ) {
        use skyzen_services::storage::ObjectStorage as _;

        assert_send(kv.get_with_cache_ttl("key", ::core::time::Duration::from_mins(1)));
        assert_send(kv.get_with_metadata::<serde_json::Value>("key"));
        assert_send(kv.put_with_options("key", b"value", crate::CfKvPutOptions::new()));
        assert_send(r2.get_if("key", crate::CfR2Conditional::new()));
        assert_send(r2.delete_many(&["key"]));
        assert_send(r2.create_multipart_upload("key"));
        assert_send(r2.get_stream("key"));
        assert_send(r2.get_range("key", skyzen_services::storage::ByteRange::slice(0, 1)));
        assert_send(d1.batch(&[]));
        assert_send(service.fetch(&request));
        assert_send(service.fetch_json::<serde_json::Value>(&request));
        assert_send(crate::CfSecretStore::get(
            &wasm_bindgen::JsValue::NULL,
            "SECRET",
        ));
    }
};
