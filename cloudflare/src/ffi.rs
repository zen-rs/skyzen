//! Low-level FFI bindings for Cloudflare Workers APIs.
//!
//! These bindings provide access to Cloudflare Workers service APIs
//! (KV, R2, Queues, D1, Durable Object SQLite) via `wasm-bindgen`.

use js_sys::{Promise, Uint8Array};
use wasm_bindgen::prelude::*;

// ── KV Namespace ──

/// Cloudflare KV Namespace binding.
#[wasm_bindgen]
extern "C" {
    /// KV namespace type from the Workers runtime.
    pub type KvNamespace;

    /// Get a value by key. Returns a promise that resolves to the value or null.
    #[wasm_bindgen(method, catch)]
    pub fn get(this: &KvNamespace, key: &str, options: &JsValue) -> Result<Promise, JsValue>;

    /// Put a key-value pair. Returns a promise.
    #[wasm_bindgen(method, catch)]
    pub fn put(this: &KvNamespace, key: &str, value: &JsValue) -> Result<Promise, JsValue>;

    /// Delete a key. Returns a promise.
    #[wasm_bindgen(method, catch)]
    pub fn delete(this: &KvNamespace, key: &str) -> Result<Promise, JsValue>;

    /// List keys. Returns a promise that resolves to a list result.
    #[wasm_bindgen(method, catch)]
    pub fn list(this: &KvNamespace, options: &JsValue) -> Result<Promise, JsValue>;
}

// ── R2 Bucket ──

/// Cloudflare R2 Bucket binding.
#[wasm_bindgen]
extern "C" {
    /// R2 bucket type from the Workers runtime.
    pub type R2Bucket;

    /// Get an object by key.
    #[wasm_bindgen(method, catch)]
    pub fn get(this: &R2Bucket, key: &str) -> Result<Promise, JsValue>;

    /// Put an object.
    #[wasm_bindgen(method, catch)]
    pub fn put(this: &R2Bucket, key: &str, value: &Uint8Array) -> Result<Promise, JsValue>;

    /// Delete an object by key.
    #[wasm_bindgen(method, catch)]
    pub fn delete(this: &R2Bucket, key: &str) -> Result<Promise, JsValue>;

    /// List objects.
    #[wasm_bindgen(method, catch)]
    pub fn list(this: &R2Bucket, options: &JsValue) -> Result<Promise, JsValue>;

    /// Head (get metadata only).
    #[wasm_bindgen(method, catch)]
    pub fn head(this: &R2Bucket, key: &str) -> Result<Promise, JsValue>;
}

// ── R2 Object ──

/// R2 object returned from get operations.
#[wasm_bindgen]
extern "C" {
    /// R2 object type.
    pub type R2Object;

    /// The object's key.
    #[wasm_bindgen(method, getter)]
    pub fn key(this: &R2Object) -> String;

    /// The object's size in bytes.
    #[wasm_bindgen(method, getter)]
    pub fn size(this: &R2Object) -> f64;

    /// The object's HTTP metadata (content type, etc.).
    #[wasm_bindgen(method, getter, js_name = httpMetadata)]
    pub fn http_metadata(this: &R2Object) -> JsValue;

    /// The object's custom metadata.
    #[wasm_bindgen(method, getter, js_name = customMetadata)]
    pub fn custom_metadata(this: &R2Object) -> JsValue;

    /// Read the body as an `ArrayBuffer`. Returns a promise.
    #[wasm_bindgen(method, catch, js_name = arrayBuffer)]
    pub fn array_buffer(this: &R2Object) -> Result<Promise, JsValue>;
}

// ── R2 Object metadata (from head) ──

/// R2 object metadata returned from head operations.
#[wasm_bindgen]
extern "C" {
    /// `R2ObjectBody` is returned from head operations (no body).
    #[wasm_bindgen(js_name = "R2Object")]
    pub type R2ObjectHead;

    /// The object's key.
    #[wasm_bindgen(method, getter)]
    pub fn key(this: &R2ObjectHead) -> String;

    /// The object's size in bytes.
    #[wasm_bindgen(method, getter)]
    pub fn size(this: &R2ObjectHead) -> f64;

    /// The object's HTTP metadata (content type, etc.).
    #[wasm_bindgen(method, getter, js_name = httpMetadata)]
    pub fn http_metadata(this: &R2ObjectHead) -> JsValue;

    /// The object's custom metadata.
    #[wasm_bindgen(method, getter, js_name = customMetadata)]
    pub fn custom_metadata(this: &R2ObjectHead) -> JsValue;
}

// ── Queue ──

/// Cloudflare Queue binding.
#[wasm_bindgen]
extern "C" {
    /// Queue type from the Workers runtime.
    pub type Queue;

    /// Send a single message to the queue. Returns a promise.
    #[wasm_bindgen(method, catch)]
    pub fn send(this: &Queue, message: &Uint8Array) -> Result<Promise, JsValue>;

    /// Send a batch of messages. Returns a promise.
    #[wasm_bindgen(method, catch, js_name = sendBatch)]
    pub fn send_batch(this: &Queue, messages: &js_sys::Array) -> Result<Promise, JsValue>;
}

// ── D1 Database ──

/// Cloudflare D1 database binding.
#[wasm_bindgen]
extern "C" {
    /// D1 database type from the Workers runtime.
    pub type D1Database;

    /// Prepare a SQL statement.
    #[wasm_bindgen(method, catch)]
    pub fn prepare(this: &D1Database, query: &str) -> Result<D1PreparedStatement, JsValue>;

    /// Execute SQL directly.
    #[wasm_bindgen(method, catch)]
    pub fn exec(this: &D1Database, query: &str) -> Result<Promise, JsValue>;
}

/// Prepared D1 SQL statement.
#[wasm_bindgen]
extern "C" {
    /// D1 prepared statement type.
    pub type D1PreparedStatement;

    /// Execute and return all rows.
    #[wasm_bindgen(method, catch)]
    pub fn all(this: &D1PreparedStatement) -> Result<Promise, JsValue>;

    /// Execute and return first row.
    #[wasm_bindgen(method, catch)]
    pub fn first(this: &D1PreparedStatement) -> Result<Promise, JsValue>;

    /// Execute and return raw row arrays.
    #[wasm_bindgen(method, catch)]
    pub fn raw(this: &D1PreparedStatement) -> Result<Promise, JsValue>;

    /// Execute write statement.
    #[wasm_bindgen(method, catch)]
    pub fn run(this: &D1PreparedStatement) -> Result<Promise, JsValue>;
}

// ── Durable Object SQLite ──

/// Durable Object SQLite storage (`state.storage.sql`).
#[wasm_bindgen]
extern "C" {
    /// SqlStorage type from Durable Object runtime.
    pub type SqlStorage;

    /// Execute SQL.
    #[wasm_bindgen(method, catch)]
    pub fn exec(this: &SqlStorage, query: &str) -> Result<JsValue, JsValue>;
}

// ── Helpers ──

/// Get a binding from the Workers env object by name.
///
/// # Errors
///
/// Returns `JsValue` error if the binding cannot be accessed.
pub fn get_binding(env: &JsValue, name: &str) -> Result<JsValue, JsValue> {
    js_sys::Reflect::get(env, &JsValue::from_str(name))
}
