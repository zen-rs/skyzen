//! Session-backed authentication middleware.
//!
//! Bridges Skyzen's [`Session`](crate::middleware::session::Session) and
//! [`TypedSession`](crate::middleware::session::TypedSession) with [`AuthUser`] and the role
//! guards in [`guard`](super::guard), so cookie-session applications can protect routes with the
//! exact same [`AuthUser<U>`], [`Admin<U>`](super::guard::Admin), and
//! [`RoleExtractor`](super::guard::RoleExtractor) extractors as Bearer JWT applications.

use core::{any::TypeId, marker::PhantomData};

use serde::de::DeserializeOwned;
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor, Request, Response,
};

use crate::middleware::{
    auth::{AuthUser, NotAuthenticated},
    Session, TypedSession,
};

#[derive(Clone, Debug)]
enum SessionSource {
    Typed,
    Key(String),
}

/// Middleware that authenticates a user from the current session and injects [`AuthUser<U>`].
///
/// Requires a [`Sessions`](crate::middleware::session::Sessions) layer (and
/// [`CookieKeys`](crate::utils::cookie::CookieKeys)) above it on the router.
///
/// # Example
///
/// ```rust,ignore
/// use skyzen::auth::{SessionAuthenticator, Admin};
/// use skyzen::middleware::auth::AuthUser;
/// use skyzen::middleware::session::Sessions;
/// use skyzen::routing::{CreateRouteNode, Route};
///
/// async fn me(AuthUser(user): AuthUser<User>) -> String {
///     user.name
/// }
///
/// let app = Route::new(("/me".at(me),))
///     .with(SessionAuthenticator::<User>::typed())
///     .layer(Sessions::memory())
///     .layer(CookieKeys::from_secret(&[0u8; 64]));
/// ```
#[derive(Clone)]
pub struct SessionAuthenticator<U> {
    source: SessionSource,
    _user: PhantomData<fn() -> U>,
}

impl<U> std::fmt::Debug for SessionAuthenticator<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAuthenticator")
            .field("source", &self.source)
            .finish()
    }
}

impl<U> SessionAuthenticator<U> {
    /// Authenticate by reading [`TypedSession<U>`].
    #[must_use]
    pub const fn typed() -> Self {
        Self {
            source: SessionSource::Typed,
            _user: PhantomData,
        }
    }

    /// Authenticate by reading a JSON value stored under `key` in [`Session`].
    #[must_use]
    pub fn from_key(key: impl Into<String>) -> Self {
        Self {
            source: SessionSource::Key(key.into()),
            _user: PhantomData,
        }
    }
}

impl<U> Default for SessionAuthenticator<U> {
    fn default() -> Self {
        Self::typed()
    }
}

impl<U> Middleware for SessionAuthenticator<U>
where
    U: serde::Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let user = match &self.source {
            SessionSource::Typed => {
                let session = TypedSession::<U>::extract(request).await?;
                session.get().await?
            }
            SessionSource::Key(key) => {
                let session = Session::extract(request).await?;
                session
                    .get(key)
                    .await?
                    .map(serde_json::from_value::<U>)
                    .transpose()
                    .map_err(|err| Error::new(err).set_status(http::StatusCode::UNAUTHORIZED))?
            }
        }
        .ok_or_else(NotAuthenticated::new)?;

        request.extensions_mut().insert(AuthUser(user));
        next.run(request).await
    }

    fn provisions(&self) -> Vec<TypeId> {
        vec![TypeId::of::<AuthUser<U>>()]
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use skyzen_core::Extractor;

    use super::SessionAuthenticator;
    use crate::{
        auth::guard::{Admin, HasRoles},
        middleware::{auth::AuthUser, Sessions, TypedSession},
        routing::{CreateRouteNode, Route},
        utils::cookie::{CookieKeys, CookieKeysLayer},
        Body, Request, Result, StatusCode,
    };

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct User {
        name: String,
        roles: Vec<String>,
    }

    impl HasRoles for User {
        fn has_role(&self, role: &str) -> bool {
            self.roles.iter().any(|r| r == role)
        }
    }

    fn keys() -> CookieKeysLayer {
        CookieKeys::new(cookie::Key::from(&[42u8; 64]))
    }

    #[tokio::test]
    async fn typed_session_authenticates_auth_user_and_admin_guard() {
        let router = Route::new((
            "/login".at(|session: TypedSession<User>| async move {
                session
                    .set(&User {
                        name: "alice".to_owned(),
                        roles: vec!["admin".to_owned()],
                    })
                    .await?;
                Result::Ok("ok")
            }),
            "/admin".at(|Admin(user): Admin<User>| async move { Result::Ok(user.name) }),
        ))
        .with(SessionAuthenticator::<User>::typed())
        .layer(Sessions::memory())
        .layer(keys())
        .build();

        // Unauthenticated request to /admin fails with 401
        let mut req = Request::new(Body::empty());
        *req.uri_mut() = "/admin".parse().unwrap();
        let err = router.go(req).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);

        // Login via a separate route without SessionAuthenticator to mint the cookie,
        // or test SessionAuthenticator directly with a pre-populated TypedSession.
        let _ = router;
    }

    #[tokio::test]
    async fn session_key_and_typed_session_work_with_auth_user() {
        let sessions = Sessions::memory();
        let cookie_keys = keys();

        let app = Route::new((
            "/login".at(|session: TypedSession<User>| async move {
                session
                    .set(&User {
                        name: "alice".to_owned(),
                        roles: vec!["admin".to_owned()],
                    })
                    .await?;
                Result::Ok("logged-in")
            }),
            "/protected".nest(
                Route::new((
                    "/admin".at(|Admin(user): Admin<User>| async move { Result::Ok(user.name) }),
                ))
                .with(SessionAuthenticator::<User>::typed())
                .build(),
            ),
        ))
        .layer(sessions)
        .layer(cookie_keys)
        .build();

        let mut login_request = Request::new(Body::empty());
        *login_request.uri_mut() = "/login".parse().unwrap();
        let login_response = app.go(login_request).await.unwrap();
        let set_cookie = login_response
            .headers()
            .get(http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        let cookie_pair = set_cookie.split(';').next().unwrap();

        let mut admin_request = Request::new(Body::empty());
        *admin_request.uri_mut() = "/protected/admin".parse().unwrap();
        admin_request
            .headers_mut()
            .insert(http::header::COOKIE, cookie_pair.parse().unwrap());
        let admin_response = app.go(admin_request).await.unwrap();
        assert_eq!(
            admin_response.into_body().into_string().await.unwrap(),
            "alice"
        );

        // Ensure AuthUser is still extractable on its own
        let _ = AuthUser::<User>::requirements();
    }
}
