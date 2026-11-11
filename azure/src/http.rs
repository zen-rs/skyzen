//! How this crate's REST traffic reaches the network: zenwave's runtime-neutral native backend.
//!
//! A [`NativeBackend`] is a `Send + Sync` handle onto a connection pool that lives on one thread's
//! compio runtime, so a backend future — which must be `Send` — stays `Send` while every socket
//! stays on that runtime, and no tokio context is needed anywhere.

use std::sync::{Arc, OnceLock};

use zenwave::{
    backend::{NativeBackend, NativeIo},
    dns::Getaddrinfo,
    Endpoint as _, Request, Response, Transport,
};

/// The HTTP client every request of a client in this crate rides on: the system transport — the
/// OS proxy settings and trust store — on a connection pool shared by the client's clones.
///
/// The pool runs on one of two threads:
///
/// - [`system`](Self::system): zenwave's own driver thread, shared with every other user of the
///   system transport, and started by the first request — so a client rebound to a thread's own
///   pool before it is used never starts it.
/// - [`system_on_current_thread`](Self::system_on_current_thread): the compio runtime of the
///   thread that built it. A thread-per-core server builds one on each serving thread, and a
///   request made there never leaves it. A request made on another thread — a background task on
///   the global executor — still reaches the pool, over a channel to its thread; once that
///   thread's runtime has stopped, it fails.
#[derive(Debug, Clone)]
pub struct AzureHttp(Arc<OnceLock<NativeBackend>>);

impl AzureHttp {
    /// The system transport on zenwave's driver thread, started by the first request.
    #[must_use]
    pub fn system() -> Self {
        Self(Arc::new(OnceLock::new()))
    }

    /// The system transport, pooled on the current thread's compio runtime and driven by `io`.
    ///
    /// # Panics
    ///
    /// When the current thread is not running a compio runtime.
    #[must_use]
    pub fn system_on_current_thread<I: NativeIo>(io: I) -> Self {
        let backend = compio::runtime::Runtime::with_current(|runtime| {
            // The system resolver, as on zenwave's own driver thread.
            NativeBackend::on_current_thread(Transport::system(), io, Getaddrinfo, runtime)
        });
        Self(Arc::new(OnceLock::from(backend)))
    }

    /// Send `request`; an error status comes back as the response it is.
    ///
    /// zenwave hands back a `4xx` or `5xx` as an ordinary response, and every caller here reads
    /// the service's refusal — its status, its `Retry-After`, its error body — out of it.
    ///
    /// # Errors
    ///
    /// A request that never got an answer: a refused connection, a TLS failure, a timeout.
    ///
    /// # Panics
    ///
    /// On the first request of a [`system`](Self::system) client, if zenwave cannot start its
    /// driver thread.
    pub(crate) async fn send(&self, request: &mut Request) -> Result<Response, zenwave::Error> {
        let mut backend = self
            .0
            .get_or_init(|| NativeBackend::new(Transport::system()))
            .clone();
        backend.respond(request).await
    }
}
