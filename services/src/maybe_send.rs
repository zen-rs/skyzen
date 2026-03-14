//! `Send` bounds shared by service wrappers.
//!
//! Skyzen handlers always require `Send` futures, even on wasm targets.
//! Service abstractions therefore keep the same contract so backends cannot
//! silently degrade a `Send` future into a `!Send` wrapper future.

/// Marker trait for futures that satisfy Skyzen's `Send` contract.
pub trait MaybeSend: Send {}

impl<T: Send> MaybeSend for T {}

/// Boxed future type used by service wrapper trait objects.
pub type BoxFuture<'a, T> = futures_core::future::BoxFuture<'a, T>;
