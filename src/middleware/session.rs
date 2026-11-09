//! Sessions on a portable store, read lazily and persisted once per request.
//!
//! [`Sessions::kv`] keeps the data in a `[[service]]` KV binding, [`Sessions::cookie`]
//! encrypts the whole session into the cookie itself and [`Sessions::memory`] keeps it in the
//! process. A handler takes `session: Session` (or `session: TypedSession<T>`), reads and
//! mutates it freely, and the layer writes back afterwards — only when the session is dirty —
//! so a read-only request costs one `get` and one `expire`, not a rewrite.
//!
//! Both `Sessions::kv` and `Sessions::cookie` need a [`CookieKeys`] layer somewhere above
//! them — the session cookie is signed or encrypted, never unsigned.

use core::{
    any::{type_name, TypeId},
    fmt,
    marker::PhantomData,
    ops::Deref,
    time::Duration,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use cookie::{Cookie, CookieJar as RawCookieJar};
use futures_core::future::BoxFuture;
use http_kit::{
    header,
    header::{HeaderMap, HeaderValue},
    HttpError, Request, Response, StatusCode,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{Map, Value};
use sha2::Digest;
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor, PendingResponseHeaders, Requirement,
};
use skyzen_services::kv::{Kv, KvError};

use crate::utils::cookie::{parse_request_jar, CookieKeys, MissingCookieKeys};

/// Default session cookie name.
const DEFAULT_COOKIE: &str = "sid";
/// Prefix separating session keys from anything else sharing the KV namespace.
const KEY_PREFIX: &str = "sess:";
/// The key a [`TypedSession`] serializes itself under inside the session map.
const TYPED_KEY: &str = "__typed";
/// Default idle time-to-live: one week.
const DEFAULT_TTL: Duration = Duration::from_hours(168);

/// The data one session carries: a JSON object of application keys.
pub type SessionData = Map<String, Value>;

/// The persistence side of [`Sessions`], for stores the built-ins do not cover.
///
/// A store is *resolved per request* by the layer — a `kv` store pulls the named `[[service]]`
/// binding out of the request extensions there — so a store value itself is self-contained:
/// it holds whatever handle the resolve produced and needs no request access later.
pub trait SessionStore: Send + Sync + 'static {
    /// Load the session `id`, or `None` when it has expired or was never written.
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>, SessionError>>;

    /// Persist `data` under `id` with `ttl` until idle expiry.
    fn save<'a>(
        &'a self,
        id: &'a str,
        data: &'a SessionData,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), SessionError>>;

    /// Drop `id` entirely.
    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), SessionError>>;

    /// Refresh `id`'s idle expiry without rewriting it. Stores that cannot touch without a
    /// rewrite leave this alone — it runs on clean reads only, so a miss is harmless.
    fn touch<'a>(&'a self, id: &'a str, ttl: Duration) -> BoxFuture<'a, Result<(), SessionError>> {
        let _ = (id, ttl);
        Box::pin(async { Ok(()) })
    }
}

/// The KV bucket one session lives under: `sess:<sha256(id)>`, so the raw id a client
/// presents is never a store key.
fn store_key(id: &str) -> String {
    let digest = sha2::Sha256::digest(id.as_bytes());
    let mut key = String::with_capacity(KEY_PREFIX.len() + digest.len() * 2);
    key.push_str(KEY_PREFIX);
    for byte in digest {
        use core::fmt::Write;
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// A fresh session id: 128 random bits, hex-encoded (32 chars, cookie-safe).
fn mint_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("getrandom is available on every supported target");
    let mut id = String::with_capacity(32);
    for byte in bytes {
        use core::fmt::Write;
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// Whether the store has been consulted yet.
#[derive(Debug, Clone)]
enum SessionCache {
    /// Nothing has read or written the session — the record may still exist server-side.
    Unloaded,
    /// The store (or the cookie payload) answered: the session data, or `None` for absent.
    Ready(Option<SessionData>),
}

/// What the extractor shares with the layer's write-back pass.
#[derive(Debug)]
struct SessionInner {
    /// The id the next `Set-Cookie` advertises; changes on `cycle_id`.
    id: String,
    /// The lazily loaded session data.
    data: SessionCache,
    /// The id the request came in on, kept so `cycle_id` can delete the old record.
    loaded_id: Option<String>,
    /// Which write-back the layer owes the store.
    pending: Pending,
}

/// Lock the session state, recovering a guard a panicked writer left poisoned — every critical
/// section here is a single field write, so the state cannot be torn.
fn lock(inner: &Mutex<SessionInner>) -> std::sync::MutexGuard<'_, SessionInner> {
    inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// Never mutated. A `touch` is still owed when the session exists.
    Clean,
    /// `insert`/`remove`/`cycle_id` ran; the data must be written.
    Dirty,
    /// `destroy` ran; the record and the cookie die together.
    Destroyed,
}

/// The slot the [`Session`] extractor leaves in the request extensions so the [`Sessions`]
/// layer can persist at end of request.
#[derive(Debug, Clone)]
struct SessionSlot(Arc<Mutex<SessionInner>>);

/// The store, cookie policy and TTL choices of one [`Sessions`] layer, resolved per request and
/// kept in the request extensions for the extractor.
#[derive(Clone)]
struct SessionConfig {
    store: Arc<dyn SessionStore>,
    cookie_name: &'static str,
    ttl: Duration,
    /// `Sessions::cookie` keeps the data inside the cookie (private jar), so its cookie is the
    /// session itself rather than an id into a store.
    in_cookie: bool,
}

impl fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionConfig")
            .field("cookie_name", &self.cookie_name)
            .field("ttl", &self.ttl)
            .field("in_cookie", &self.in_cookie)
            .finish_non_exhaustive()
    }
}

/// The session a handler reads and writes.
///
/// Extraction is cheap: the cookie is verified but the store is not consulted until the first
/// `get`. Mutations (`insert`, `remove`, `cycle_id`, `destroy`) only mark the session dirty —
/// the [`Sessions`] layer persists them when the handler returns.
#[derive(Debug)]
pub struct Session {
    inner: Arc<Mutex<SessionInner>>,
    config: SessionConfig,
}

impl Session {
    /// The current session id. Changes after `cycle_id`.
    #[must_use]
    pub fn id(&self) -> String {
        lock(&self.inner).id.clone()
    }

    /// The value stored under `key`, loading the session from the store the first time it is
    /// needed.
    ///
    /// # Errors
    /// [`SessionError::Store`] when the backend read fails.
    pub async fn get(&self, key: &str) -> Result<Option<Value>, SessionError> {
        self.data()
            .await
            .map(|data| data.and_then(|data| data.get(key).cloned()))
    }

    /// The whole session map, lazily loaded. `Ok(None)` means the session is new or expired.
    async fn data(&self) -> Result<Option<SessionData>, SessionError> {
        // Take what is needed under the lock, drop it, then await — a guard held across the
        // store call would stall the layer's write-back pass.
        let (id, cached) = {
            let inner = lock(&self.inner);
            // Load under the id the request came in on — `cycle_id` may already have minted
            // the outgoing id, and loading that would silently discard the old record.
            (
                inner.loaded_id.clone().unwrap_or_else(|| inner.id.clone()),
                match &inner.data {
                    SessionCache::Ready(data) => Some(data.clone()),
                    SessionCache::Unloaded => None,
                },
            )
        };
        if let Some(data) = cached {
            return Ok(data);
        }
        let loaded = self.config.store.load(&id).await?;
        lock(&self.inner).data = SessionCache::Ready(loaded.clone());
        Ok(loaded)
    }

    /// Set `key` to `value`, marking the session dirty. Loads the session first when the
    /// store has not been consulted yet, so an insert merges rather than clobbers.
    ///
    /// # Errors
    /// [`SessionError::Codec`] when `value` does not serialize, [`SessionError::Store`] when the
    /// backend read fails.
    pub async fn insert(&self, key: &str, value: impl Serialize) -> Result<(), SessionError> {
        let value = serde_json::to_value(value)
            .map_err(|error| SessionError::encode("session value", error))?;
        self.data().await?;
        {
            let mut inner = lock(&self.inner);
            if let SessionCache::Ready(data) = &mut inner.data {
                data.get_or_insert_with(SessionData::new)
                    .insert(key.to_owned(), value);
            }
            inner.pending = Pending::Dirty;
        }
        Ok(())
    }

    /// Drop `key`, marking the session dirty. Loads the session first like `insert` does.
    ///
    /// # Errors
    /// [`SessionError::Store`] when the backend read fails.
    pub async fn remove(&self, key: &str) -> Result<(), SessionError> {
        self.data().await?;
        {
            let mut inner = lock(&self.inner);
            if let SessionCache::Ready(Some(data)) = &mut inner.data {
                data.remove(key);
            }
            inner.pending = Pending::Dirty;
        }
        Ok(())
    }

    /// Mint a new id, keep the data, and drop the old record at write-back. Call on
    /// authentication changes so a stolen id stops working.
    pub fn cycle_id(&self) {
        let mut inner = lock(&self.inner);
        inner.id = mint_id();
        inner.pending = Pending::Dirty;
    }

    /// Empty the session and delete both the record and the cookie.
    pub fn destroy(&self) {
        let mut inner = lock(&self.inner);
        inner.data = SessionCache::Ready(None);
        inner.pending = Pending::Destroyed;
    }

    /// A new anonymous session — the store assigns no record until the first `insert`.
    fn fresh(config: SessionConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionInner {
                id: mint_id(),
                data: SessionCache::Ready(None),
                loaded_id: None,
                pending: Pending::Clean,
            })),
            config,
        }
    }
}

/// The encrypted cookie payload: the session map plus the server-side expiry the
/// client cannot alter, so a captured cookie cannot be replayed past `ttl`.
#[derive(Serialize, serde::Deserialize)]
struct CookiePayload {
    #[serde(rename = "e")]
    expires: f64,
    #[serde(rename = "d")]
    data: SessionData,
}

impl CookiePayload {
    /// Wrap `data` with an expiry `ttl` from now — sliding, matching the cookie's Max-Age.
    fn wrap(data: SessionData, ttl: Duration) -> Self {
        Self {
            expires: ttl.as_secs_f64().mul_add(1000.0, now_ms()),
            data,
        }
    }
}

/// The cookie's signed or encrypted payload for `name`, trying the fallback key when the
/// primary rejects it.
fn cookie_value(
    jar: &RawCookieJar,
    keys: &CookieKeys,
    in_cookie: bool,
    name: &str,
) -> Option<String> {
    let get = |key: &cookie::Key| {
        if in_cookie {
            jar.private(key).get(name)
        } else {
            jar.signed(key).get(name)
        }
    };
    get(keys.key())
        .or_else(|| keys.fallback().and_then(get))
        .map(|cookie| cookie.value().to_owned())
}

impl Extractor for Session {
    type Error = SessionError;
    fn extract(
        request: &mut Request,
    ) -> impl core::future::Future<Output = Result<Self, Self::Error>> + Send {
        std::future::ready(Self::extract_now(request))
    }

    fn requirements() -> Vec<Requirement> {
        vec![
            Requirement::of::<CookieKeys>("`.layer(CookieKeys::from_secret::<..>())`"),
            Requirement::of::<Sessions>("`.layer(Sessions::kv::<..>())`"),
        ]
    }
}

impl Session {
    /// The synchronous body of `extract` — no store IO happens here, by design.
    fn extract_now(request: &mut Request) -> Result<Self, SessionError> {
        let config = request
            .extensions()
            .get::<SessionConfig>()
            .cloned()
            .ok_or(SessionError::NoLayer)?;

        // A request that already extracted a session reuses the shared handle — a second
        // SessionSlot would orphan the first extractor's writes at write-back.
        if let Some(slot) = request.extensions().get::<SessionSlot>() {
            return Ok(Self {
                inner: slot.0.clone(),
                config,
            });
        }

        let keys = request
            .extensions()
            .get::<CookieKeys>()
            .cloned()
            .ok_or_else(MissingCookieKeys::new)?;
        let jar = parse_request_jar(request)?;

        let session = if config.in_cookie {
            let data = cookie_value(&jar, &keys, true, config.cookie_name)
                .and_then(|value| serde_json::from_str::<CookiePayload>(&value).ok())
                .filter(|payload| payload.expires > now_ms())
                .map(|payload| payload.data);
            Self {
                inner: Arc::new(Mutex::new(SessionInner {
                    id: mint_id(),
                    data: SessionCache::Ready(data),
                    loaded_id: None,
                    pending: Pending::Clean,
                })),
                config,
            }
        } else {
            let id = cookie_value(&jar, &keys, false, config.cookie_name)
                .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
            match id {
                Some(id) => Self {
                    inner: Arc::new(Mutex::new(SessionInner {
                        loaded_id: Some(id.clone()),
                        id,
                        data: SessionCache::Unloaded,
                        pending: Pending::Clean,
                    })),
                    config,
                },
                None => Self::fresh(config),
            }
        };

        // Leave the shared handle for the layer's write-back pass.
        request
            .extensions_mut()
            .insert(SessionSlot(session.inner.clone()));
        Ok(session)
    }
}

/// A session whose entire payload is one typed value.
///
/// `TypedSession<Cart>` serializes `Cart` into the session under one reserved key; reads are
/// `serde_json` round-trips against the same store and cookie policy as [`Session`].
#[derive(Debug)]
pub struct TypedSession<T> {
    session: Session,
    _marker: PhantomData<T>,
}

impl<T> TypedSession<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// The stored value, deserialized. Lazily loads the backing session first.
    ///
    /// # Errors
    /// [`SessionError::Store`] when the backend read fails, [`SessionError::Codec`] when the
    /// stored value does not deserialize as `T`.
    pub async fn get(&self) -> Result<Option<T>, SessionError> {
        self.session
            .get(TYPED_KEY)
            .await?
            .map(|value| {
                serde_json::from_value(value).map_err(|e| SessionError::encode(type_name::<T>(), e))
            })
            .transpose()
    }

    /// Replace the stored value.
    ///
    /// # Errors
    /// [`SessionError::Codec`] when `T` does not serialize, [`SessionError::Store`] when the
    /// backend read fails.
    pub async fn set(&self, value: &T) -> Result<(), SessionError> {
        self.session.insert(TYPED_KEY, value).await
    }

    /// Drop the stored value.
    ///
    /// # Errors
    /// [`SessionError::Store`] when the backend read fails.
    pub async fn clear(&self) -> Result<(), SessionError> {
        self.session.remove(TYPED_KEY).await
    }

    /// The underlying session, for `id`/`cycle_id`/`destroy`.
    #[must_use]
    pub const fn session(&self) -> &Session {
        &self.session
    }
}

impl<T> Extractor for TypedSession<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    type Error = SessionError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(Self {
            session: Session::extract(request).await?,
            _marker: PhantomData,
        })
    }

    fn requirements() -> Vec<Requirement> {
        <Session as Extractor>::requirements()
    }
}

/// How a [`SessionsLayer`] resolves the per-request store.
type StoreResolver =
    Arc<dyn Fn(&Request) -> Result<Arc<dyn SessionStore>, SessionError> + Send + Sync>;

/// The layer [`Sessions::kv`], [`Sessions::cookie`] and [`Sessions::memory`] build.
///
/// After the chain runs it reads the [`SessionSlot`], persists the session once if it went
/// dirty and emits the `Set-Cookie` — a signed id for stores, the encrypted payload for
/// `Sessions::cookie`.
pub struct SessionsLayer {
    resolve: StoreResolver,
    cookie_name: &'static str,
    ttl: Duration,
    in_cookie: bool,
}

impl fmt::Debug for SessionsLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionsLayer")
            .field("cookie_name", &self.cookie_name)
            .field("ttl", &self.ttl)
            .field("in_cookie", &self.in_cookie)
            .finish_non_exhaustive()
    }
}

impl SessionsLayer {
    /// The session cookie name (`sid` by default).
    #[must_use]
    pub const fn cookie_name(mut self, name: &'static str) -> Self {
        self.cookie_name = name;
        self
    }

    /// The idle time-to-live (one week by default).
    #[must_use]
    pub const fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Emit the session cookie and persist the session state after the chain ran, through the
    /// store `handle` already resolved for this request.
    async fn write_back(
        &self,
        store: &dyn SessionStore,
        response: &mut Response,
        keys: &CookieKeys,
        slot: &SessionSlot,
    ) -> Result<(), SessionError> {
        let (id, loaded_id, loaded, pending) = {
            let inner = slot.0.lock().expect("session lock");
            (
                inner.id.clone(),
                inner.loaded_id.clone(),
                inner.data.clone(),
                inner.pending,
            )
        };
        for cookie in self
            .jar_for(store, keys, (id, loaded_id, loaded, pending))
            .await?
            .delta()
        {
            response.headers_mut().append(
                header::SET_COOKIE,
                HeaderValue::try_from(cookie.encoded().to_string())
                    .map_err(|_| SessionError::CookieHeader)?,
            );
        }
        Ok(())
    }

    /// Build the cookies the session's pending state calls for — store writes and deletions
    /// happen here too, so this runs for both the normal `Set-Cookie` path and the error
    /// path's deferred headers.
    async fn jar_for(
        &self,
        store: &dyn SessionStore,
        keys: &CookieKeys,
        (id, loaded_id, loaded, pending): (String, Option<String>, SessionCache, Pending),
    ) -> Result<RawCookieJar, SessionError> {
        let mut jar = RawCookieJar::new();
        match pending {
            Pending::Destroyed => {
                if let Some(loaded_id) = loaded_id {
                    store.delete(&loaded_id).await?;
                }
                let mut cookie = Cookie::new(self.cookie_name, "");
                cookie.make_removal();
                cookie.set_path("/");
                jar.add(cookie);
            }
            Pending::Dirty => {
                let data = match loaded {
                    SessionCache::Ready(data) => data.unwrap_or_default(),
                    // Dirty without ever being read is `cycle_id` alone — resurrect the old
                    // record's data under the new id rather than writing an empty map.
                    SessionCache::Unloaded => match &loaded_id {
                        Some(old) => store.load(old).await?.unwrap_or_default(),
                        None => SessionData::new(),
                    },
                };
                if self.in_cookie {
                    let encoded = serde_json::to_string(&CookiePayload::wrap(data, self.ttl))
                        .map_err(|e| SessionError::encode("session", e))?;
                    let mut cookie = Cookie::new(self.cookie_name, encoded);
                    set_session_cookie_flags(&mut cookie, self.ttl, keys.secure());
                    jar.private_mut(keys.key()).add(cookie);
                } else {
                    store.save(&id, &data, self.ttl).await?;
                    if let Some(loaded_id) = loaded_id.filter(|old| *old != id) {
                        store.delete(&loaded_id).await?;
                    }
                    let mut cookie = Cookie::new(self.cookie_name, id.clone());
                    set_session_cookie_flags(&mut cookie, self.ttl, keys.secure());
                    jar.signed_mut(keys.key()).add(cookie);
                }
            }
            Pending::Clean => {
                if self.in_cookie {
                    // Nothing stored server-side; only re-emit when the cookie held data so an
                    // anonymous visitor is not issued an empty session.
                    if let SessionCache::Ready(Some(data)) = loaded {
                        let encoded = serde_json::to_string(&CookiePayload::wrap(data, self.ttl))
                            .map_err(|e| SessionError::encode("session", e))?;
                        let mut cookie = Cookie::new(self.cookie_name, encoded);
                        set_session_cookie_flags(&mut cookie, self.ttl, keys.secure());
                        jar.private_mut(keys.key()).add(cookie);
                    }
                } else if loaded_id.is_some() {
                    store.touch(&id, self.ttl).await?;
                }
            }
        }
        Ok(jar)
    }

    /// A failed chain still owes the session's side effects — a `destroy` must expire the
    /// cookie, a rotation must deliver the new id — so the cookies ride the request's
    /// `PendingResponseHeaders` for whichever boundary renders the error.
    async fn persist(
        &self,
        store: &dyn SessionStore,
        keys: &CookieKeys,
        slot: &SessionSlot,
        request: &mut Request,
    ) -> Result<(), SessionError> {
        let (id, loaded_id, loaded, pending) = {
            let inner = slot.0.lock().expect("session lock");
            (
                inner.id.clone(),
                inner.loaded_id.clone(),
                inner.data.clone(),
                inner.pending,
            )
        };
        let jar = self
            .jar_for(store, keys, (id, loaded_id, loaded, pending))
            .await?;
        let extensions = request.extensions_mut();
        let pending_headers = match extensions.get_mut::<PendingResponseHeaders>() {
            Some(pending) => pending,
            None => {
                extensions.insert(PendingResponseHeaders(HeaderMap::new()));
                extensions
                    .get_mut::<PendingResponseHeaders>()
                    .expect("pending headers just inserted")
            }
        };
        for cookie in jar.delta() {
            pending_headers.0.append(
                header::SET_COOKIE,
                HeaderValue::try_from(cookie.encoded().to_string())
                    .map_err(|_| SessionError::CookieHeader)?,
            );
        }
        Ok(())
    }
}

fn set_session_cookie_flags(cookie: &mut Cookie<'_>, ttl: Duration, secure: bool) {
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(cookie::SameSite::Lax);
    cookie.set_secure(secure);
    cookie.set_max_age(cookie::time::Duration::seconds(
        ttl.as_secs().try_into().unwrap_or(i64::MAX),
    ));
}

impl Middleware for SessionsLayer {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let store = (self.resolve)(request)?;
        request.extensions_mut().insert(SessionConfig {
            store: store.clone(),
            cookie_name: self.cookie_name,
            ttl: self.ttl,
            in_cookie: self.in_cookie,
        });
        let result = next.run(request).await;
        let Some(slot) = request.extensions().get::<SessionSlot>().cloned() else {
            return result;
        };
        let keys = request
            .extensions()
            .get::<CookieKeys>()
            .cloned()
            .ok_or_else(MissingCookieKeys::new)?;
        match result {
            Ok(mut response) => {
                self.write_back(&*store, &mut response, &keys, &slot)
                    .await?;
                Ok(response)
            }
            // The chain failed — a response will be built at the edge — but the session's
            // side effects still apply: store writes here, Set-Cookie deferred onto whatever
            // response the error becomes.
            Err(error) => {
                self.persist(&*store, &keys, &slot, request).await?;
                Err(error)
            }
        }
    }

    fn provisions(&self) -> Vec<TypeId> {
        vec![TypeId::of::<Sessions>()]
    }
}

/// Marker the `Requirement` names, so a route that takes a [`Session`] fails at build without
/// the layer.
#[derive(Debug)]
pub struct Sessions;

impl Sessions {
    /// Sessions in the `[[service]]` KV binding `S` — the generated `import_config!` type for a
    /// `kv_namespaces`/`[[service]]` entry.
    ///
    /// Cloudflare KV is eventually consistent: a `get` right after `put` may miss by a second
    /// or two. A session is read before it is written in nearly every flow, so the staleness
    /// surfaces only as a just-logged-in user looking logged-out once — pick `cookie()` when
    /// that is not acceptable.
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn kv<S>() -> SessionsLayer
    where
        S: Deref<Target = Kv> + Clone + Send + Sync + 'static,
    {
        SessionsLayer {
            resolve: Arc::new(|request| {
                let kv = request
                    .extensions()
                    .get::<S>()
                    .map(|service| (**service).clone())
                    .ok_or_else(|| SessionError::MissingService(type_name::<S>()))?;
                Ok(Arc::new(KvSessionStore { kv }) as Arc<dyn SessionStore>)
            }),
            cookie_name: DEFAULT_COOKIE,
            ttl: DEFAULT_TTL,
            in_cookie: false,
        }
    }

    /// Sessions encrypted into the cookie itself — no server state, no KV, and the privacy of
    /// [`PrivateCookieJar`]. Keep payloads small; a cookie is not a database.
    ///
    /// The trade-off of statelessness is revocation: `destroy` and `cycle_id` expire the
    /// browser's copy, but a captured cookie replays until its embedded `exp` — nothing
    /// server-side can invalidate it. Pick [`kv`][Self::kv] or [`memory`][Self::memory] when
    /// sessions must die on demand (logout-everywhere, revocation, rotation with immediate
    /// effect).
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn cookie() -> SessionsLayer {
        SessionsLayer {
            resolve: Arc::new(|_| Ok(Arc::new(CookieSessionStore) as Arc<dyn SessionStore>)),
            cookie_name: DEFAULT_COOKIE,
            ttl: DEFAULT_TTL,
            in_cookie: true,
        }
    }

    /// Sessions in this process — one `Sessions` layer, one map. Right for development and
    /// single-replica deployments; a restart or a second replica loses them.
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn memory() -> SessionsLayer {
        let store = Arc::new(MemorySessionStore::default()) as Arc<dyn SessionStore>;
        SessionsLayer {
            resolve: Arc::new(move |_| Ok(store.clone())),
            cookie_name: DEFAULT_COOKIE,
            ttl: DEFAULT_TTL,
            in_cookie: false,
        }
    }

    /// Sessions in a caller-provided [`SessionStore`].
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn custom(store: impl SessionStore) -> SessionsLayer {
        let store = Arc::new(store) as Arc<dyn SessionStore>;
        SessionsLayer {
            resolve: Arc::new(move |_| Ok(store.clone())),
            cookie_name: DEFAULT_COOKIE,
            ttl: DEFAULT_TTL,
            in_cookie: false,
        }
    }
}

/// Sessions in a manifest `[[service]]` KV binding, resolved per request in the layer.
struct KvSessionStore {
    kv: Kv,
}

impl fmt::Debug for KvSessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KvSessionStore(..)")
    }
}

impl SessionStore for KvSessionStore {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>, SessionError>> {
        Box::pin(async move {
            self.kv
                .get_json::<SessionData>(&store_key(id))
                .await
                .map_err(SessionError::store)
        })
    }

    fn save<'a>(
        &'a self,
        id: &'a str,
        data: &'a SessionData,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            let bytes = serde_json::to_vec(data).map_err(|e| SessionError::encode("session", e))?;
            self.kv
                .put_with_ttl(&store_key(id), &bytes, ttl)
                .await
                .map_err(SessionError::store)
        })
    }

    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.kv
                .delete(&store_key(id))
                .await
                .map_err(SessionError::store)
        })
    }

    fn touch<'a>(&'a self, id: &'a str, ttl: Duration) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.kv
                .expire(&store_key(id), ttl)
                .await
                .map(|_| ())
                .or_else(|error| {
                    // A backend without native expiry (Cloudflare KV) cannot slide a TTL
                    // without rewriting the record — the session simply keeps the deadline
                    // its last write set rather than failing the request.
                    match error {
                        KvError::Unsupported(_) => Ok(()),
                        other => Err(SessionError::store(other)),
                    }
                })
        })
    }
}

/// The store for `Sessions::cookie` — every operation is a no-op because the data rides in the
/// cookie itself, written by [`SessionsLayer::write_back`].
struct CookieSessionStore;

impl fmt::Debug for CookieSessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CookieSessionStore")
    }
}

impl SessionStore for CookieSessionStore {
    fn load<'a>(
        &'a self,
        _id: &'a str,
    ) -> BoxFuture<'a, Result<Option<SessionData>, SessionError>> {
        Box::pin(async { Ok(None) })
    }

    fn save<'a>(
        &'a self,
        _id: &'a str,
        _data: &'a SessionData,
        _ttl: Duration,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async { Ok(()) })
    }

    fn delete<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async { Ok(()) })
    }
}

/// An in-process store for `Sessions::memory` — a map with lazy expiry sweeps.
#[derive(Default)]
struct MemorySessionStore {
    entries: Mutex<HashMap<String, (f64, SessionData)>>,
}

impl fmt::Debug for MemorySessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySessionStore").finish_non_exhaustive()
    }
}

/// Wall-clock milliseconds — the value is serialized into cookies and compared across
/// processes, so `Instant` is unusable here.
#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

/// `Date.now` on a Worker.
#[cfg(target_arch = "wasm32")]
fn now_ms() -> f64 {
    js_sys::Date::now()
}

impl SessionStore for MemorySessionStore {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>, SessionError>> {
        Box::pin(async move {
            let now = now_ms();
            let key = store_key(id);
            let mut entries = self.entries.lock().expect("memory store");
            match entries.get(&key) {
                Some((expires, _)) if *expires <= now => {
                    entries.remove(&key);
                    drop(entries);
                    Ok(None)
                }
                Some((_, data)) => {
                    let data = data.clone();
                    drop(entries);
                    Ok(Some(data))
                }
                None => {
                    drop(entries);
                    Ok(None)
                }
            }
        })
    }

    fn save<'a>(
        &'a self,
        id: &'a str,
        data: &'a SessionData,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            let expires = ttl.as_secs_f64().mul_add(1000.0, now_ms());
            self.entries
                .lock()
                .expect("memory store")
                .insert(store_key(id), (expires, data.clone()));
            Ok(())
        })
    }

    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.entries
                .lock()
                .expect("memory store")
                .remove(&store_key(id));
            Ok(())
        })
    }

    fn touch<'a>(&'a self, id: &'a str, ttl: Duration) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            if let Some((expires, _)) = self
                .entries
                .lock()
                .expect("memory store")
                .get_mut(&store_key(id))
            {
                *expires = ttl.as_secs_f64().mul_add(1000.0, now_ms());
            }
            Ok(())
        })
    }
}

/// Everything that can go wrong between the session cookie and the store.
#[derive(Debug)]
pub enum SessionError {
    /// A [`Session`] was extracted without a `Sessions` layer.
    NoLayer,
    /// No `CookieKeys` layer provided the signing/encryption keys.
    MissingKeys(MissingCookieKeys),
    /// The `Cookie` header could not be parsed.
    ParseCookies(crate::utils::cookie::CookieParseError),
    /// The named `[[service]]` binding was not on the request.
    MissingService(&'static str),
    /// The store itself failed.
    Store(Box<dyn std::error::Error + Send + Sync>),
    /// The session payload could not be (de)serialized.
    Codec(&'static str, Box<dyn std::error::Error + Send + Sync>),
    /// The `Set-Cookie` header could not be built.
    CookieHeader,
}

impl SessionError {
    fn store(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Store(Box::new(error))
    }

    fn encode(who: &'static str, error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Codec(who, Box::new(error))
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLayer => {
                f.write_str("no `Sessions` layer configured; add `.layer(Sessions::kv::<..>())`")
            }
            Self::MissingKeys(_) => {
                f.write_str("no `CookieKeys` layer configured; sessions sign and encrypt cookies")
            }
            Self::ParseCookies(_) => f.write_str("failed to parse cookies"),
            Self::MissingService(name) => write!(f, "service `{name}` is not on the request"),
            Self::Store(error) => write!(f, "session store failed: {error}"),
            Self::Codec(who, error) => write!(f, "session codec failed for {who}: {error}"),
            Self::CookieHeader => f.write_str("failed to build the Set-Cookie header"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingKeys(inner) => Some(inner),
            Self::ParseCookies(inner) => Some(inner),
            Self::Store(inner) | Self::Codec(_, inner) => Some(&**inner),
            _ => None,
        }
    }
}

impl HttpError for SessionError {
    fn status(&self) -> StatusCode {
        if matches!(self, Self::ParseCookies(_)) {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

impl From<MissingCookieKeys> for SessionError {
    fn from(error: MissingCookieKeys) -> Self {
        Self::MissingKeys(error)
    }
}

impl From<crate::utils::cookie::CookieParseError> for SessionError {
    fn from(error: crate::utils::cookie::CookieParseError) -> Self {
        Self::ParseCookies(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{Session, Sessions, TypedSession};
    use crate::{
        routing::{CreateRouteNode, Route, Router},
        utils::{CookieKeys, Key},
        Body, Request, Response, StatusCode,
    };
    use core::{any::TypeId, ops::Deref};
    use http_kit::header::{HeaderValue, COOKIE, SET_COOKIE};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use skyzen_core::middleware::{Middleware, Next};
    use skyzen_services::kv::Kv;

    async fn set_name(session: Session) -> &'static str {
        session.insert("name", "lex").await.unwrap();
        "ok"
    }

    async fn get_name(session: Session) -> String {
        session
            .get("name")
            .await
            .unwrap()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    async fn cycle_and_read(session: Session) -> String {
        session.cycle_id();
        session.id()
    }

    async fn cycle_and_insert(session: Session) -> &'static str {
        session.cycle_id();
        session.insert("nick", "b").await.unwrap();
        "done"
    }

    async fn destroy_it(session: Session) -> &'static str {
        session.destroy();
        "gone"
    }

    /// A manifest-shaped `[[service]]` newtype — the type `import_config!` generates for a KV
    /// binding: clones itself into the request extensions like the generated middleware does.
    #[derive(Clone)]
    struct KvService(Kv);

    impl Deref for KvService {
        type Target = Kv;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Middleware for KvService {
        async fn handle(
            &self,
            request: &mut Request,
            next: Next<'_>,
        ) -> Result<Response, skyzen_core::Error> {
            request.extensions_mut().insert(self.clone());
            next.run(request).await
        }

        fn provisions(&self) -> Vec<TypeId> {
            vec![TypeId::of::<Self>()]
        }
    }

    fn router(sessions: super::SessionsLayer) -> Router {
        Route::new((
            "/set".at(set_name),
            "/get".at(get_name),
            "/cycle".at(cycle_and_read),
            "/cycle-insert".at(cycle_and_insert),
            "/destroy".at(destroy_it),
        ))
        .layer(sessions)
        .layer(CookieKeys::new(Key::generate()).secure_cookies(false))
        .build()
    }

    async fn go(router: &Router, path: &str, cookies: Option<String>) -> Response {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().unwrap();
        if let Some(cookies) = cookies {
            request
                .headers_mut()
                .insert(COOKIE, HeaderValue::from_str(&cookies).unwrap());
        }
        router.go(request).await.unwrap()
    }

    fn session_cookie(response: &Response) -> Option<String> {
        response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("sid="))
            .map(|value| value.split(';').next().unwrap().to_owned())
    }

    #[tokio::test]
    async fn a_memory_session_round_trips_through_the_signed_cookie() {
        let router = router(Sessions::memory());

        let response = go(&router, "/set", None).await;
        let cookie = session_cookie(&response).expect("a sid cookie is set");

        let response = go(&router, "/get", Some(cookie)).await;
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "lex");
    }

    #[tokio::test]
    async fn a_cookie_session_keeps_the_payload_in_the_cookie_itself() {
        let router = router(Sessions::cookie());

        let response = go(&router, "/set", None).await;
        let cookie = session_cookie(&response).expect("a sid cookie is set");
        // The value is encrypted, so the payload never appears in the cookie.
        assert!(!cookie.contains("lex"));

        let response = go(&router, "/get", Some(cookie)).await;
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "lex");
    }

    #[tokio::test]
    async fn cycle_id_rotates_the_id_and_keeps_the_data() {
        let router = router(Sessions::memory());

        let response = go(&router, "/set", None).await;
        let old_cookie = session_cookie(&response).unwrap();

        let response = go(&router, "/cycle", Some(old_cookie.clone())).await;
        let new_cookie = session_cookie(&response).unwrap();
        assert_ne!(old_cookie, new_cookie);

        let response = go(&router, "/get", Some(new_cookie)).await;
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "lex");

        // The abandoned id resolves to nothing.
        let response = go(&router, "/get", Some(old_cookie)).await;
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "");
    }

    #[tokio::test]
    async fn cycle_id_then_insert_keeps_the_old_record() {
        let router = router(Sessions::memory());

        let response = go(&router, "/set", None).await;
        let old_cookie = session_cookie(&response).unwrap();

        // Rotate, then write without ever reading — the write must merge into the record
        // the request arrived on, not clobber it under the fresh id.
        let response = go(&router, "/cycle-insert", Some(old_cookie)).await;
        let new_cookie = session_cookie(&response).unwrap();

        let response = go(&router, "/get", Some(new_cookie)).await;
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "lex");
    }

    #[tokio::test]
    async fn destroy_expires_the_cookie_and_drops_the_record() {
        let router = router(Sessions::memory());

        let response = go(&router, "/set", None).await;
        let cookie = session_cookie(&response).unwrap();

        let response = go(&router, "/destroy", Some(cookie.clone())).await;
        let expired = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("sid="))
            .expect("an expiring cookie")
            .to_owned();
        assert!(expired.contains("Max-Age=0"));

        let response = go(&router, "/get", Some(cookie)).await;
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "");
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Cart {
        items: Vec<String>,
    }

    async fn set_cart(session: TypedSession<Cart>) -> &'static str {
        session
            .set(&Cart {
                items: vec!["tea".into()],
            })
            .await
            .unwrap();
        "ok"
    }

    async fn get_cart(session: TypedSession<Cart>) -> String {
        session
            .get()
            .await
            .unwrap()
            .map(|cart| cart.items.join(","))
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_typed_session_serializes_the_whole_value() {
        let router = Route::new(("/set".at(set_cart), "/get".at(get_cart)))
            .layer(Sessions::memory())
            .layer(CookieKeys::new(Key::generate()).secure_cookies(false))
            .build();

        let response = go(&router, "/set", None).await;
        let cookie = session_cookie(&response).unwrap();

        let response = go(&router, "/get", Some(cookie)).await;
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "tea");
    }

    #[tokio::test]
    async fn a_kv_session_uses_the_named_binding() {
        // The service binds outermost: `Sessions::kv` resolves it at the head of its own
        // handle, before any inner middleware or the endpoint runs.
        let kv = KvService(Kv::new(skyzen_test::mock::InMemoryKv::new()));
        let router = Route::new(("/set".at(set_name), "/get".at(get_name)))
            .layer(kv)
            .layer(Sessions::kv::<KvService>())
            .layer(CookieKeys::new(Key::generate()).secure_cookies(false))
            .build();

        let response = go(&router, "/set", None).await;
        let cookie = session_cookie(&response).unwrap();

        let response = go(&router, "/get", Some(cookie)).await;
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "lex");
    }

    #[test]
    fn extracting_a_session_without_the_layer_fails_at_build() {
        let error = Route::new(("/get".at(get_name),))
            .layer(CookieKeys::new(Key::generate()))
            .try_build()
            .unwrap_err();
        assert!(matches!(
            error,
            crate::routing::RouteBuildError::MissingProvision { .. }
        ));
    }

    #[tokio::test]
    async fn extracting_twice_shares_one_session() {
        async fn handler(first: Session, second: Session) -> String {
            first.insert("k", 7u32).await.unwrap();
            second
                .get("k")
                .await
                .unwrap()
                .and_then(|v: Value| v.as_u64().map(|n| n.to_string()))
                .unwrap_or_else(|| "lost".to_owned())
        }
        let router = Route::new(("/x".at(handler),))
            .layer(Sessions::memory())
            .layer(CookieKeys::new(Key::generate()).secure_cookies(false))
            .build();
        let response = go(&router, "/x", None).await;
        // The second extractor's handle sees the first's writes — no orphaned slot.
        assert_eq!(response.into_body().into_string().await.unwrap(), "7");
    }

    #[tokio::test]
    async fn a_session_insert_serializes_arbitrary_values() {
        async fn set_value(session: Session) -> &'static str {
            session.insert("count", 42u32).await.unwrap();
            "ok"
        }
        async fn get_value(session: Session) -> String {
            session
                .get("count")
                .await
                .unwrap()
                .and_then(|v: Value| v.as_u64().map(|n| n.to_string()))
                .unwrap_or_default()
        }
        let router = Route::new(("/set".at(set_value), "/get".at(get_value)))
            .layer(Sessions::memory())
            .layer(CookieKeys::new(Key::generate()).secure_cookies(false))
            .build();
        let response = go(&router, "/set", None).await;
        let cookie = session_cookie(&response).unwrap();
        let response = go(&router, "/get", Some(cookie)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.into_body().into_string().await.unwrap(), "42");
    }
}
