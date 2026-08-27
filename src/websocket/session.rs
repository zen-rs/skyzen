//! What a websocket session handler is, and what happens when one fails.
//!
//! A session handler is the callback [`WebSocketUpgrade::on_upgrade`](super::WebSocketUpgrade)
//! and [`.ws()`](crate::routing::CreateRouteNode::ws) run once the handshake succeeded. It owns
//! the socket for the lifetime of the connection, which is why its failures cannot travel the
//! normal response path: by the time it runs, the `101` response has already been sent.
//!
//! So a session reports failure by returning one. Returning `()` still means "the session ended";
//! returning [`Result`] lets the handler use `?` and hand the framework whatever ended it. The
//! framework logs a returned error with its whole `source()` chain and tries to close the
//! connection with [`INTERNAL_ERROR`], so a peer learns the session died rather than watching the
//! socket go silent.

use core::future::Future;

use skyzen_core::error::Error;

use super::{WebSocket, WebSocketCloseFrame};

/// Close code `1011`: the server hit a condition that stopped it fulfilling the request.
///
/// This is the code the framework sends on behalf of a session handler that returned an error,
/// mirroring what a `500` does for an ordinary handler.
pub const INTERNAL_ERROR: u16 = 1011;

/// The close frame sent when a session handler returns an error.
///
/// The reason is deliberately generic for the same rationale as the `5xx` response body: the
/// detail belongs in the server's logs, not on the wire.
pub fn internal_error_frame() -> WebSocketCloseFrame {
    WebSocketCloseFrame::new(INTERNAL_ERROR, "internal error")
}

mod sealed {
    pub trait Sealed {}
}

/// What a websocket session handler is allowed to return.
///
/// Implemented for `()` — the session ended, and said nothing about how — and for
/// `Result<(), E>` where `E` converts into [`Error`], which covers
/// [`WebSocketError`](super::WebSocketError), every [`HttpError`](skyzen_core::error::HttpError)
/// and [`Error`] itself. The trait is sealed: it exists to name those two shapes, not to be
/// extended.
pub trait IntoWebSocketOutcome: sealed::Sealed {
    /// Reduce the handler's return value to "ended cleanly" or the error that ended it.
    ///
    /// # Errors
    ///
    /// Returns whatever the session handler reported, converted into [`Error`]; `()` never does.
    fn into_outcome(self) -> Result<(), Error>;
}

impl sealed::Sealed for () {}

impl IntoWebSocketOutcome for () {
    fn into_outcome(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<E: Into<Error>> sealed::Sealed for Result<(), E> {}

impl<E: Into<Error>> IntoWebSocketOutcome for Result<(), E> {
    fn into_outcome(self) -> Result<(), Error> {
        self.map_err(Into::into)
    }
}

/// `Send`, except on `wasm32`.
///
/// A session handler on a multi-threaded native runtime has to be `Send`: the executor may poll it
/// on any thread. Nothing about a WebAssembly isolate makes that true or useful — there is no
/// second thread — and requiring it there would reject every real edge session, because the socket
/// itself is built from `Rc`s and JS handles that no `Send` future can hold across an `await`.
///
/// The relaxation stops at the builder. What the router stores still has to satisfy
/// `http_kit::Endpoint`'s unconditional `Send`, which is why the session is carried across that
/// boundary in a cell whose `unsafe impl Send` is justified by the isolate being single-threaded.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}

#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// `Send`, except on `wasm32` — where it asks for nothing, because a Worker isolate has no second
/// thread to send anything to. See the native definition for the full rationale.
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}

#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync`, except on `wasm32`. The counterpart of [`MaybeSend`], for the session callback itself:
/// the native upgrade path shares it across threads, a Worker isolate never can.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Sync {}

#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync + ?Sized> MaybeSync for T {}

/// `Sync`, except on `wasm32` — where it asks for nothing. See [`MaybeSend`].
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}

#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSync for T {}

/// Carries a session across the router's `Send + Sync` bounds.
///
/// Native sessions already are `Send + Sync`, so this is a plain newtype there.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
struct SessionCell<S>(S);

/// Carries a session across the router's `Send + Sync` bounds.
///
/// A wasm session is routinely `!Send`: it holds the socket's `Rc<RefCell<..>>` event closures and
/// raw JS handles across every `await`. The bound it has to cross is not the framework's to relax
/// — `http_kit::Endpoint` requires `Send` unconditionally, and the router's type erasure boxes
/// every endpoint future as `dyn Send + Future` — so the session is carried through it in this
/// cell instead of being rejected at compile time on the one target the framework exists for.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
struct SessionCell<S>(S);

// SAFETY: a `wasm32` Worker (and every other WinterCG isolate Skyzen targets) is single-threaded:
// there is no second thread for the value to be sent to or shared with, so the `Send`/`Sync` these
// bounds ask for cannot be observed. The cell is private and never leaves this crate, so the only
// values it ever carries are session callbacks the router immediately runs on the same isolate.
#[cfg(target_arch = "wasm32")]
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<S> Send for SessionCell<S> {}

// SAFETY: see the `Send` implementation above.
#[cfg(target_arch = "wasm32")]
unsafe impl<S> Sync for SessionCell<S> {}

/// Build the request handler `.ws()` registers: extract the upgrade, then run `session` on the
/// socket it produces.
///
/// The returned closure is what the router type-erases, so it — not the session — is what must
/// satisfy [`Handler`](crate::handler::Handler)'s `Send + Sync + Clone + 'static`.
pub fn session_handler<F, Fut>(
    session: F,
) -> impl Fn(super::WebSocketUpgrade) -> core::future::Ready<super::WebSocketUpgradeResponder>
       + Clone
       + Send
       + Sync
       + 'static
where
    F: Fn(WebSocket) -> Fut + Clone + MaybeSend + MaybeSync + 'static,
    Fut: Future + MaybeSend + 'static,
    Fut::Output: IntoWebSocketOutcome + 'static,
{
    let session = SessionCell(session);
    move |upgrade: super::WebSocketUpgrade| {
        let session = session.clone().0;
        // The handshake is answered from the request's own headers, so there is nothing to await
        // before the responder is ready; the session itself runs after the `101` is written.
        core::future::ready(upgrade.on_upgrade(session))
    }
}

/// Compile-time proof that [`.ws()`](crate::routing::CreateRouteNode::ws) accepts the sessions
/// this target actually produces: ones holding an [`Rc`](std::rc::Rc) — and, in real code, the
/// socket's own JS handles — across every `await`.
///
/// It lives in the library rather than in `#[cfg(test)]` because the wasm CI leg is a plain
/// `cargo check` with no `--all-targets`: a test module would never be compiled, and the bound
/// this guards against regressing is exactly the one that only fails on this target. Nothing calls
/// it; building it is the whole point.
#[cfg(all(target_arch = "wasm32", feature = "ws"))]
#[allow(dead_code)]
mod wasm_session_bounds {
    use crate::routing::{CreateRouteNode, Route, Router};
    use futures_util::StreamExt;
    use std::rc::Rc;

    fn router() -> Router {
        let greeting = Rc::new(String::from("hello"));

        Route::new((
            // A session that reports nothing, as every session could before outcomes existed.
            "/quiet".ws(|mut socket| async move {
                while let Some(Ok(message)) = socket.next().await {
                    let _ = message;
                }
            }),
            // A session that uses `?` and keeps a `!Send` value across every await.
            "/chat".ws(move |mut socket| {
                let greeting = Rc::clone(&greeting);
                async move {
                    socket.send_text(greeting.as_str()).await?;
                    while let Some(message) = socket.next().await {
                        if let Some(text) = message?.into_text() {
                            socket.send_text(format!("{greeting}:{text}")).await?;
                        }
                    }
                    Ok::<_, crate::Error>(())
                }
            }),
        ))
        .build()
    }
}
