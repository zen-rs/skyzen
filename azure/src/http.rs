//! How this crate's REST traffic reaches the network: zenwave's runtime-neutral native backend.
//!
//! A [`NativeBackend`] is a `Send + Sync` handle onto a connection pool that lives on one thread's
//! compio driver, so a backend future — which must be `Send` — stays `Send` while every socket stays
//! on that driver, and no tokio context is needed anywhere.

use zenwave::{backend::NativeBackend, Endpoint as _, Request, Response, Transport};

/// The backend every request rides on: the system transport — the OS proxy settings and trust
/// store — whose pool runs on zenwave's own compio driver thread, shared with every other user of
/// that transport.
///
/// # Panics
///
/// If zenwave cannot start its driver thread.
pub fn system_backend() -> NativeBackend {
    NativeBackend::new(Transport::system())
}

/// Send `request`, an error status coming back as the response it is.
///
/// zenwave reports a `4xx` or `5xx` as [`zenwave::Error::Http`], but every caller here reads the
/// service's refusal — its status, its `Retry-After`, its error body — out of the response, which
/// the error carries whole.
///
/// # Errors
///
/// A request that never got an answer: a refused connection, a TLS failure, a timeout.
pub async fn send(
    backend: &mut NativeBackend,
    request: &mut Request,
) -> Result<Response, zenwave::Error> {
    match backend.respond(request).await {
        Err(zenwave::Error::Http { response, .. }) => Ok(response.response),
        answered => answered,
    }
}
