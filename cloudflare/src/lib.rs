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
//!
//! **This crate only works on `wasm32` targets.** On native targets it compiles
//! as an empty crate.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_cloudflare::{CfKv, CfR2, CfQueue};
//! use skyzen_services::{Kv, Storage, Queue};
//!
//! // From a Workers env binding
//! let kv = Kv::new(CfKv::from_env(&env, "MY_KV")?);
//! let storage = Storage::new(CfR2::from_env(&env, "MY_BUCKET")?);
//! let queue = Queue::new(CfQueue::from_env(&env, "MY_QUEUE")?);
//! ```
//!
//! [`KeyValueStore`]: skyzen_services::kv::KeyValueStore
//! [`ObjectStorage`]: skyzen_services::storage::ObjectStorage
//! [`MessageQueue`]: skyzen_services::queue::MessageQueue

#[cfg(target_arch = "wasm32")]
pub mod ffi;
#[cfg(target_arch = "wasm32")]
pub mod kv;
#[cfg(target_arch = "wasm32")]
pub mod queues;
#[cfg(target_arch = "wasm32")]
pub mod r2;

#[cfg(target_arch = "wasm32")]
pub use kv::CfKv;
#[cfg(target_arch = "wasm32")]
pub use queues::CfQueue;
#[cfg(target_arch = "wasm32")]
pub use r2::CfR2;
