//! JSON Web Key Set (JWKS, RFC 7517) parsing, verification, and key rotation.
//!
//! Provides [`JwtKeySet`] for static/in-memory JWKS verification, [`JwksAuthenticator`] for
//! [`AuthMiddleware`](crate::middleware::auth::AuthMiddleware), and (with the `oidc` feature)
//! [`RemoteJwks`] for cached remote JWKS fetching with `kid`-miss cooldown refresh.

use std::{collections::HashMap, future::Future, marker::PhantomData, sync::Arc};

use http::StatusCode;
#[cfg(not(target_arch = "wasm32"))]
use jsonwebtoken::{
    decode, decode_header,
    jwk::{AlgorithmParameters, Jwk, JwkSet, KeyAlgorithm, PublicKeyUse},
    DecodingKey, TokenData, Validation,
};
use serde::de::DeserializeOwned;

use super::jwt::{Algorithm, JwtError, KeyFamily};
#[cfg(target_arch = "wasm32")]
use super::wasm_jwt::{
    decode_header as wasm_decode_header, verify_and_decode, SendFuture, WasmKeyMaterial,
    WasmValidation,
};
use crate::{
    extract::auth::{parse_bearer, BearerTokenError},
    middleware::auth::Authenticator,
    Request,
};

#[cfg(feature = "oidc")]
use core::time::Duration;
#[cfg(feature = "oidc")]
use std::sync::Mutex;

/// Error returned when parsing or refreshing a JWKS fails.
#[skyzen::error(status = StatusCode::BAD_REQUEST)]
pub enum JwksError {
    /// The JWKS JSON document could not be parsed.
    #[error("Invalid JWKS document")]
    InvalidDocument,
    /// A JWK entry could not be converted into a decoding key.
    #[error("Invalid JWK entry")]
    InvalidKey,
    /// Fetching a remote JWKS document failed.
    #[error("Failed to fetch remote JWKS", status = StatusCode::SERVICE_UNAVAILABLE)]
    Fetch,
}

#[derive(Clone)]
struct JwkEntry {
    #[cfg(not(target_arch = "wasm32"))]
    decoding_key: DecodingKey,
    #[cfg(target_arch = "wasm32")]
    material: WasmKeyMaterial,
    family: KeyFamily,
    algorithm: Option<Algorithm>,
}

/// Validation settings applied when verifying tokens against a [`JwtKeySet`].
#[derive(Clone, Debug)]
pub struct JwksValidation {
    issuer: Option<Vec<String>>,
    audience: Option<Vec<String>>,
    allowed_algorithms: Option<Vec<Algorithm>>,
    validate_exp: bool,
    validate_nbf: bool,
    leeway: u64,
}

impl Default for JwksValidation {
    fn default() -> Self {
        Self {
            issuer: None,
            audience: None,
            allowed_algorithms: None,
            validate_exp: true,
            validate_nbf: false,
            leeway: 60,
        }
    }
}

impl JwksValidation {
    /// Create default validation settings (`validate_exp = true`, `leeway = 60`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the expected issuer claim (`iss`).
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(vec![issuer.into()]);
        self
    }

    /// Set the expected audience claim (`aud`).
    #[must_use]
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(vec![audience.into()]);
        self
    }

    /// Restrict verification to an explicit allowlist of signing algorithms.
    #[must_use]
    pub fn with_algorithms(mut self, algorithms: Vec<Algorithm>) -> Self {
        self.allowed_algorithms = Some(algorithms);
        self
    }

    /// Disable expiration (`exp`) validation.
    #[must_use]
    pub const fn without_expiration_validation(mut self) -> Self {
        self.validate_exp = false;
        self
    }

    /// Enable or disable `nbf` (not before) claim validation.
    #[must_use]
    pub const fn with_nbf_validation(mut self, validate: bool) -> Self {
        self.validate_nbf = validate;
        self
    }

    /// Set the leeway (in seconds) for `exp` and `nbf` claims.
    #[must_use]
    pub const fn with_leeway(mut self, leeway: u64) -> Self {
        self.leeway = leeway;
        self
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn build_jsonwebtoken_validation(&self, algorithm: Algorithm) -> Validation {
        let mut validation = Validation::new(algorithm);
        validation.validate_exp = self.validate_exp;
        validation.validate_nbf = self.validate_nbf;
        validation.leeway = self.leeway;
        if let Some(iss) = &self.issuer {
            validation.set_issuer(iss);
        }
        if let Some(aud) = &self.audience {
            validation.set_audience(aud);
        }
        validation
    }

    #[cfg(target_arch = "wasm32")]
    fn build_wasm_validation(&self, algorithm: Algorithm) -> WasmValidation {
        WasmValidation {
            algorithms: vec![algorithm],
            issuer: self.issuer.clone(),
            audience: self.audience.clone(),
            validate_exp: self.validate_exp,
            validate_nbf: self.validate_nbf,
            leeway: self.leeway,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
const fn map_key_algorithm(alg: KeyAlgorithm) -> Option<Algorithm> {
    match alg {
        KeyAlgorithm::HS256 => Some(Algorithm::HS256),
        KeyAlgorithm::HS384 => Some(Algorithm::HS384),
        KeyAlgorithm::HS512 => Some(Algorithm::HS512),
        KeyAlgorithm::RS256 => Some(Algorithm::RS256),
        KeyAlgorithm::RS384 => Some(Algorithm::RS384),
        KeyAlgorithm::RS512 => Some(Algorithm::RS512),
        KeyAlgorithm::PS256 => Some(Algorithm::PS256),
        KeyAlgorithm::PS384 => Some(Algorithm::PS384),
        KeyAlgorithm::PS512 => Some(Algorithm::PS512),
        KeyAlgorithm::ES256 => Some(Algorithm::ES256),
        KeyAlgorithm::ES384 => Some(Algorithm::ES384),
        KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
        _ => None,
    }
}

#[cfg(not(target_arch = "wasm32"))]
const fn key_family(jwk: &Jwk) -> KeyFamily {
    match jwk.algorithm {
        AlgorithmParameters::RSA(_) => KeyFamily::Rsa,
        AlgorithmParameters::EllipticCurve(_) => KeyFamily::Ec,
        AlgorithmParameters::OctetKey(_) => KeyFamily::Hmac,
        AlgorithmParameters::OctetKeyPair(_) => KeyFamily::Ed,
    }
}

/// A parsed JSON Web Key Set (JWKS) ready for JWT signature verification.
#[derive(Clone)]
pub struct JwtKeySet {
    by_kid: HashMap<String, JwkEntry>,
    unkeyed: Vec<JwkEntry>,
}

impl std::fmt::Debug for JwtKeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtKeySet")
            .field("keyed_count", &self.by_kid.len())
            .field("unkeyed_count", &self.unkeyed.len())
            .finish()
    }
}

impl JwtKeySet {
    /// Parse an RFC 7517 JWKS JSON document.
    ///
    /// # Errors
    ///
    /// Returns [`JwksError::InvalidDocument`] if JSON parsing fails or [`JwksError::InvalidKey`]
    /// if a signature key cannot be decoded.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_json(json: &str) -> Result<Self, JwksError> {
        let jwk_set: JwkSet = serde_json::from_str(json).map_err(|_| JwksError::InvalidDocument)?;
        Self::from_jwk_set(&jwk_set)
    }

    /// Parse an RFC 7517 JWKS JSON document on `wasm32`.
    ///
    /// # Errors
    ///
    /// Returns [`JwksError::InvalidDocument`] if JSON parsing fails or [`JwksError::InvalidKey`]
    /// if a signature key cannot be decoded.
    #[cfg(target_arch = "wasm32")]
    pub fn from_json(json: &str) -> Result<Self, JwksError> {
        #[derive(serde::Deserialize)]
        struct WasmJwkSet {
            keys: Vec<serde_json::Value>,
        }

        let set: WasmJwkSet = serde_json::from_str(json).map_err(|_| JwksError::InvalidDocument)?;
        let mut by_kid = HashMap::new();
        let mut unkeyed = Vec::new();

        for raw in set.keys {
            if raw.get("use").and_then(serde_json::Value::as_str) == Some("enc") {
                continue;
            }
            let kty = raw
                .get("kty")
                .and_then(serde_json::Value::as_str)
                .ok_or(JwksError::InvalidKey)?;
            let family = match kty {
                "RSA" => KeyFamily::Rsa,
                "EC" => KeyFamily::Ec,
                "oct" => KeyFamily::Hmac,
                "OKP" => KeyFamily::Ed,
                _ => return Err(JwksError::InvalidKey),
            };
            let algorithm = raw
                .get("alg")
                .and_then(serde_json::Value::as_str)
                .map(|alg| {
                    let mapped: Algorithm =
                        serde_json::from_value(serde_json::Value::String(alg.to_owned()))
                            .map_err(|_| JwksError::InvalidKey)?;
                    if !family.permits(mapped) {
                        return Err(JwksError::InvalidKey);
                    }
                    Ok(mapped)
                })
                .transpose()?;

            let kid = raw
                .get("kid")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let entry = JwkEntry {
                material: WasmKeyMaterial::Jwk(raw),
                family,
                algorithm,
            };
            if let Some(kid) = kid {
                by_kid.insert(kid, entry);
            } else {
                unkeyed.push(entry);
            }
        }

        Ok(Self { by_kid, unkeyed })
    }

    /// Convert a [`jsonwebtoken::jwk::JwkSet`] into a [`JwtKeySet`].
    ///
    /// Keys marked with `use: "enc"` (encryption-only) are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`JwksError::InvalidKey`] if a JWK has invalid key material or an `alg`
    /// incompatible with its `kty`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_jwk_set(jwk_set: &JwkSet) -> Result<Self, JwksError> {
        let mut by_kid = HashMap::new();
        let mut unkeyed = Vec::new();

        for jwk in &jwk_set.keys {
            if matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)) {
                continue;
            }

            let family = key_family(jwk);
            let algorithm = match jwk.common.key_algorithm {
                Some(alg) => {
                    let mapped = map_key_algorithm(alg).ok_or(JwksError::InvalidKey)?;
                    if !family.permits(mapped) {
                        return Err(JwksError::InvalidKey);
                    }
                    Some(mapped)
                }
                None => None,
            };

            let decoding_key = DecodingKey::from_jwk(jwk).map_err(|_| JwksError::InvalidKey)?;
            let entry = JwkEntry {
                decoding_key,
                family,
                algorithm,
            };

            if let Some(kid) = &jwk.common.key_id {
                by_kid.insert(kid.clone(), entry);
            } else {
                unkeyed.push(entry);
            }
        }

        Ok(Self { by_kid, unkeyed })
    }

    /// Number of usable verification keys in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_kid.len() + self.unkeyed.len()
    }

    /// Returns `true` if the key set contains no usable verification keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_kid.is_empty() && self.unkeyed.is_empty()
    }

    /// Returns `true` if a key with the given `kid` is present.
    #[must_use]
    pub fn contains_kid(&self, kid: &str) -> bool {
        self.by_kid.contains_key(kid)
    }

    /// Verify and decode `token` using the matching key from this set and `validation`.
    ///
    /// Enforces [`KeyFamily`] algorithm-confusion protection, any JWK-declared `alg`, and
    /// `validation`'s algorithm allowlist.
    ///
    /// # Errors
    ///
    /// Returns [`JwtError`] if the token header is malformed, no matching key is found, the
    /// algorithm is disallowed, or signature/claims verification fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn decode<C: DeserializeOwned>(
        &self,
        token: &str,
        validation: &JwksValidation,
    ) -> Result<TokenData<C>, JwtError> {
        let header = decode_header(token)?;

        if let Some(allowed) = &validation.allowed_algorithms {
            if !allowed.contains(&header.alg) {
                return Err(JwtError::InvalidAlgorithm);
            }
        }

        if let Some(kid) = header.kid.as_deref() {
            let entry = self.by_kid.get(kid).ok_or(JwtError::InvalidSignature)?;
            return decode_with_entry(token, header.alg, entry, validation);
        }

        // No `kid` in header: try unkeyed keys first, or the single keyed entry if there is
        // unambiguously one key in the set.
        if self.unkeyed.is_empty() && self.by_kid.len() == 1 {
            if let Some(entry) = self.by_kid.values().next() {
                return decode_with_entry(token, header.alg, entry, validation);
            }
        }

        let mut last_err = JwtError::InvalidSignature;
        for entry in &self.unkeyed {
            match decode_with_entry(token, header.alg, entry, validation) {
                Ok(data) => return Ok(data),
                Err(err) => last_err = err,
            }
        }
        Err(last_err)
    }

    /// Verify and decode `token` into claims `C` asynchronously on both native and `wasm32`.
    ///
    /// # Errors
    ///
    /// Returns [`JwtError`] if verification or claims validation fails.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(clippy::unused_async, clippy::unused_async_trait_impl)
    )]
    pub async fn decode_claims<C: DeserializeOwned>(
        &self,
        token: &str,
        validation: &JwksValidation,
    ) -> Result<C, JwtError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.decode::<C>(token, validation).map(|data| data.claims)
        }
        #[cfg(target_arch = "wasm32")]
        {
            let header = wasm_decode_header(token)?;
            if let Some(allowed) = &validation.allowed_algorithms {
                if !allowed.contains(&header.alg) {
                    return Err(JwtError::InvalidAlgorithm);
                }
            }
            if let Some(kid) = header.kid.as_deref() {
                let entry = self.by_kid.get(kid).ok_or(JwtError::InvalidSignature)?;
                return decode_with_entry_wasm(token, header.alg, entry, validation).await;
            }
            if self.unkeyed.is_empty() && self.by_kid.len() == 1 {
                if let Some(entry) = self.by_kid.values().next() {
                    return decode_with_entry_wasm(token, header.alg, entry, validation).await;
                }
            }
            let mut last_err = JwtError::InvalidSignature;
            for entry in &self.unkeyed {
                match decode_with_entry_wasm(token, header.alg, entry, validation).await {
                    Ok(claims) => return Ok(claims),
                    Err(err) => last_err = err,
                }
            }
            Err(last_err)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn decode_with_entry<C: DeserializeOwned>(
    token: &str,
    header_alg: Algorithm,
    entry: &JwkEntry,
    validation: &JwksValidation,
) -> Result<TokenData<C>, JwtError> {
    if !entry.family.permits(header_alg) {
        return Err(JwtError::InvalidAlgorithm);
    }
    if let Some(key_alg) = entry.algorithm {
        if key_alg != header_alg {
            return Err(JwtError::InvalidAlgorithm);
        }
    }
    let jt_validation = validation.build_jsonwebtoken_validation(header_alg);
    decode::<C>(token, &entry.decoding_key, &jt_validation).map_err(JwtError::from)
}

#[cfg(target_arch = "wasm32")]
async fn decode_with_entry_wasm<C: DeserializeOwned>(
    token: &str,
    header_alg: Algorithm,
    entry: &JwkEntry,
    validation: &JwksValidation,
) -> Result<C, JwtError> {
    if !entry.family.permits(header_alg) {
        return Err(JwtError::InvalidAlgorithm);
    }
    if let Some(key_alg) = entry.algorithm {
        if key_alg != header_alg {
            return Err(JwtError::InvalidAlgorithm);
        }
    }
    let wasm_validation = validation.build_wasm_validation(header_alg);
    verify_and_decode(token, &entry.material, header_alg, &wasm_validation).await
}

#[cfg(feature = "oidc")]
type FetchFn = Arc<
    dyn Fn(&str) -> futures_core::future::BoxFuture<'static, Result<String, JwksError>>
        + Send
        + Sync,
>;

#[cfg(feature = "oidc")]
type JwksWaiter = async_channel::Sender<Result<Arc<JwtKeySet>, JwksError>>;

#[cfg(feature = "oidc")]
#[derive(Default)]
struct RemoteJwksState {
    keys: Option<Arc<JwtKeySet>>,
    last_fetched_ms: Option<f64>,
    last_miss_refresh_ms: Option<f64>,
    waiters: Option<Vec<JwksWaiter>>,
}

#[cfg(feature = "oidc")]
#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[cfg(feature = "oidc")]
#[cfg(target_arch = "wasm32")]
fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// Cached remote JWKS fetcher with TTL expiration and `kid`-miss cooldown protection.
///
/// Works on both native (`zenwave` compio backend) and `wasm32` (`fetch` API).
/// Concurrent callers hitting an expired cache or unknown `kid` coalesce onto a single in-flight
/// HTTP request, and unknown `kid` refreshes are rate-limited by `kid_miss_cooldown`.
#[cfg(feature = "oidc")]
#[derive(Clone)]
pub struct RemoteJwks {
    jwks_uri: String,
    refresh_ttl: Duration,
    kid_miss_cooldown: Duration,
    state: Arc<Mutex<RemoteJwksState>>,
    fetcher: FetchFn,
}

#[cfg(feature = "oidc")]
impl std::fmt::Debug for RemoteJwks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteJwks")
            .field("jwks_uri", &self.jwks_uri)
            .field("refresh_ttl", &self.refresh_ttl)
            .field("kid_miss_cooldown", &self.kid_miss_cooldown)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "oidc")]
impl RemoteJwks {
    /// Create a new remote JWKS source for `jwks_uri` with a 5-minute TTL and 30-second
    /// `kid`-miss cooldown.
    #[must_use]
    pub fn new(jwks_uri: impl Into<String>) -> Self {
        let fetcher: FetchFn = Arc::new(|uri: &str| {
            let uri = uri.to_owned();
            Box::pin(async move {
                use zenwave::Client as _;
                let mut client = zenwave::client();
                let response = client
                    .get(&uri)
                    .map_err(|_| JwksError::Fetch)?
                    .await
                    .map_err(|_| JwksError::Fetch)?;
                if !response.status().is_success() {
                    return Err(JwksError::Fetch);
                }
                response
                    .into_body()
                    .into_string()
                    .await
                    .map(|s| s.to_string())
                    .map_err(|_| JwksError::Fetch)
            })
        });
        Self {
            jwks_uri: jwks_uri.into(),
            refresh_ttl: Duration::from_secs(300),
            kid_miss_cooldown: Duration::from_secs(30),
            state: Arc::new(Mutex::new(RemoteJwksState::default())),
            fetcher,
        }
    }

    #[cfg(test)]
    fn with_fetcher(jwks_uri: impl Into<String>, fetcher: FetchFn) -> Self {
        Self {
            jwks_uri: jwks_uri.into(),
            refresh_ttl: Duration::from_secs(300),
            kid_miss_cooldown: Duration::from_secs(30),
            state: Arc::new(Mutex::new(RemoteJwksState::default())),
            fetcher,
        }
    }

    /// Set how long a fetched JWKS remains fresh before a background/on-demand refresh.
    #[must_use]
    pub const fn with_refresh_ttl(mut self, ttl: Duration) -> Self {
        self.refresh_ttl = ttl;
        self
    }

    /// Set the minimum interval between refreshes triggered by an unknown `kid`.
    #[must_use]
    pub const fn with_kid_miss_cooldown(mut self, cooldown: Duration) -> Self {
        self.kid_miss_cooldown = cooldown;
        self
    }

    /// Return the current key set, fetching or refreshing if expired or if `token`'s `kid` is
    /// missing (subject to `kid_miss_cooldown`).
    ///
    /// # Errors
    ///
    /// Returns [`JwksError`] if the remote JWKS cannot be fetched or parsed.
    pub async fn resolve_for_token(&self, token: &str) -> Result<Arc<JwtKeySet>, JwksError> {
        enum Action {
            Cached(Arc<JwtKeySet>),
            Wait(async_channel::Receiver<Result<Arc<JwtKeySet>, JwksError>>),
            Fetch,
        }

        #[cfg(not(target_arch = "wasm32"))]
        let kid = decode_header(token).ok().and_then(|h| h.kid);
        #[cfg(target_arch = "wasm32")]
        let kid = wasm_decode_header(token).ok().and_then(|h| h.kid);
        let now = now_ms();
        let ttl_ms = self.refresh_ttl.as_secs_f64() * 1000.0;
        let cooldown_ms = self.kid_miss_cooldown.as_secs_f64() * 1000.0;

        let action = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let is_fresh = state.last_fetched_ms.is_some_and(|ts| now - ts < ttl_ms);
            if is_fresh {
                if let Some(keys) = &state.keys {
                    let kid_present = kid.as_deref().is_none_or(|id| keys.contains_kid(id));
                    if kid_present {
                        Action::Cached(Arc::clone(keys))
                    } else if state
                        .last_miss_refresh_ms
                        .is_some_and(|ts| now - ts < cooldown_ms)
                    {
                        // Cooldown active: return current keys without stampeding upstream.
                        Action::Cached(Arc::clone(keys))
                    } else if let Some(waiters) = &mut state.waiters {
                        let (tx, rx) = async_channel::bounded(1);
                        waiters.push(tx);
                        Action::Wait(rx)
                    } else {
                        state.last_miss_refresh_ms = Some(now);
                        state.waiters = Some(Vec::new());
                        Action::Fetch
                    }
                } else if let Some(waiters) = &mut state.waiters {
                    let (tx, rx) = async_channel::bounded(1);
                    waiters.push(tx);
                    Action::Wait(rx)
                } else {
                    state.waiters = Some(Vec::new());
                    Action::Fetch
                }
            } else if let Some(waiters) = &mut state.waiters {
                let (tx, rx) = async_channel::bounded(1);
                waiters.push(tx);
                Action::Wait(rx)
            } else {
                if kid.is_some() && state.keys.is_some() {
                    state.last_miss_refresh_ms = Some(now);
                }
                state.waiters = Some(Vec::new());
                Action::Fetch
            }
        };

        match action {
            Action::Cached(keys) => Ok(keys),
            Action::Wait(rx) => rx.recv().await.unwrap_or(Err(JwksError::Fetch)),
            Action::Fetch => self.execute_fetch().await,
        }
    }

    /// Force an immediate fetch of the remote JWKS and update the cache.
    ///
    /// # Errors
    ///
    /// Returns [`JwksError`] if the remote JWKS cannot be fetched or parsed.
    pub async fn refresh(&self) -> Result<Arc<JwtKeySet>, JwksError> {
        let wait_rx = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(waiters) = &mut state.waiters {
                let (tx, rx) = async_channel::bounded(1);
                waiters.push(tx);
                Some(rx)
            } else {
                state.waiters = Some(Vec::new());
                None
            }
        };

        if let Some(rx) = wait_rx {
            rx.recv().await.unwrap_or(Err(JwksError::Fetch))
        } else {
            self.execute_fetch().await
        }
    }

    async fn execute_fetch(&self) -> Result<Arc<JwtKeySet>, JwksError> {
        let result = match (self.fetcher)(&self.jwks_uri).await {
            Ok(body) => JwtKeySet::from_json(&body).map(Arc::new),
            Err(err) => Err(err),
        };

        let waiters = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Ok(keys) = &result {
                state.keys = Some(Arc::clone(keys));
                state.last_fetched_ms = Some(now_ms());
            }
            state.waiters.take().unwrap_or_default()
        };

        for tx in waiters {
            let outcome = match &result {
                Ok(keys) => Ok(Arc::clone(keys)),
                Err(JwksError::InvalidDocument) => Err(JwksError::InvalidDocument),
                Err(JwksError::InvalidKey) => Err(JwksError::InvalidKey),
                Err(JwksError::Fetch) => Err(JwksError::Fetch),
            };
            let _ = tx.try_send(outcome);
        }

        result
    }
}

#[derive(Clone, Debug)]
enum JwksSourceInner {
    Static(Arc<JwtKeySet>),
    #[cfg(feature = "oidc")]
    Remote(RemoteJwks),
}

/// Authenticator that verifies Bearer JWTs against a static or remote JWKS.
#[derive(Clone)]
pub struct JwksAuthenticator<C> {
    source: JwksSourceInner,
    validation: JwksValidation,
    _claims: PhantomData<fn() -> C>,
}

impl<C> std::fmt::Debug for JwksAuthenticator<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwksAuthenticator")
            .field("source", &self.source)
            .field("validation", &self.validation)
            .finish()
    }
}

impl<C> JwksAuthenticator<C> {
    /// Create a new JWKS authenticator backed by an in-memory [`JwtKeySet`].
    #[must_use]
    pub fn new(key_set: JwtKeySet) -> Self {
        Self {
            source: JwksSourceInner::Static(Arc::new(key_set)),
            validation: JwksValidation::default(),
            _claims: PhantomData,
        }
    }

    /// Parse an RFC 7517 JWKS JSON document and build an authenticator from it.
    ///
    /// # Errors
    ///
    /// Returns [`JwksError`] if the JSON document or any JWK is invalid.
    pub fn from_jwks_json(json: &str) -> Result<Self, JwksError> {
        Ok(Self::new(JwtKeySet::from_json(json)?))
    }

    /// Create a new JWKS authenticator backed by a cached [`RemoteJwks`] source.
    #[cfg(feature = "oidc")]
    #[must_use]
    pub fn remote(remote: RemoteJwks) -> Self {
        Self {
            source: JwksSourceInner::Remote(remote),
            validation: JwksValidation::default(),
            _claims: PhantomData,
        }
    }

    /// Set the expected issuer claim (`iss`).
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.validation = self.validation.with_issuer(issuer);
        self
    }

    /// Set the expected audience claim (`aud`).
    #[must_use]
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.validation = self.validation.with_audience(audience);
        self
    }

    /// Restrict verification to the given signing algorithms.
    #[must_use]
    pub fn with_algorithms(mut self, algorithms: Vec<Algorithm>) -> Self {
        self.validation = self.validation.with_algorithms(algorithms);
        self
    }

    /// Disable expiration (`exp`) validation.
    #[must_use]
    pub const fn without_expiration_validation(mut self) -> Self {
        self.validation.validate_exp = false;
        self
    }

    /// Enable or disable `nbf` (not before) claim validation.
    #[must_use]
    pub const fn with_nbf_validation(mut self, validate: bool) -> Self {
        self.validation.validate_nbf = validate;
        self
    }

    /// Set the leeway (in seconds) for `exp` and `nbf` claims.
    #[must_use]
    pub const fn with_leeway(mut self, leeway: u64) -> Self {
        self.validation.leeway = leeway;
        self
    }
}

impl<C> Authenticator for JwksAuthenticator<C>
where
    C: DeserializeOwned + Clone + Send + Sync + 'static,
{
    type User = C;
    type Error = JwtError;

    fn authenticate(
        &self,
        req: &Request,
    ) -> impl Future<Output = Result<Self::User, Self::Error>> + Send {
        let token = parse_bearer(req).map(str::to_owned);
        let source = self.source.clone();
        let validation = self.validation.clone();
        let fut = async move {
            let token = token.map_err(JwtError::from)?;
            let keys = match source {
                JwksSourceInner::Static(keys) => keys,
                #[cfg(feature = "oidc")]
                JwksSourceInner::Remote(remote) => remote
                    .resolve_for_token(&token)
                    .await
                    .map_err(|_| JwtError::InvalidSignature)?,
            };
            keys.decode_claims::<C>(&token, &validation).await
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            fut
        }
        #[cfg(target_arch = "wasm32")]
        {
            SendFuture(fut)
        }
    }
}

impl From<BearerTokenError> for JwksError {
    fn from(_: BearerTokenError) -> Self {
        Self::InvalidDocument
    }
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use http::header::AUTHORIZATION;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde::{Deserialize, Serialize};

    use super::{JwksAuthenticator, JwksValidation, JwtError, JwtKeySet};
    use crate::middleware::auth::Authenticator;

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct Claims {
        sub: String,
        exp: u64,
    }

    fn oct_jwks(kid: &str, secret: &[u8]) -> String {
        let k = URL_SAFE_NO_PAD.encode(secret);
        format!(r#"{{"keys":[{{"kty":"oct","kid":"{kid}","use":"sig","alg":"HS256","k":"{k}"}}]}}"#)
    }

    #[tokio::test]
    async fn verifies_token_by_kid_from_static_jwks() {
        let secret = b"super-secret-jwks-key";
        let jwks = oct_jwks("key-1", secret);
        let auth = JwksAuthenticator::<Claims>::from_jwks_json(&jwks).unwrap();

        let claims = Claims {
            sub: "alice".to_owned(),
            exp: u64::MAX,
        };
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("key-1".to_owned());
        let token = encode(&header, &claims, &EncodingKey::from_secret(secret)).unwrap();

        let req = http::Request::builder()
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(http_kit::Body::empty())
            .unwrap();

        assert_eq!(auth.authenticate(&req).await.unwrap(), claims);
    }

    #[tokio::test]
    async fn rejects_unknown_kid_and_algorithm_mismatch() {
        let secret = b"super-secret-jwks-key";
        let jwks = oct_jwks("key-1", secret);
        let key_set = JwtKeySet::from_json(&jwks).unwrap();

        let claims = Claims {
            sub: "alice".to_owned(),
            exp: u64::MAX,
        };

        // Unknown kid
        let mut unknown_header = Header::new(Algorithm::HS256);
        unknown_header.kid = Some("key-2".to_owned());
        let unknown_token =
            encode(&unknown_header, &claims, &EncodingKey::from_secret(secret)).unwrap();
        assert!(matches!(
            key_set.decode::<Claims>(&unknown_token, &JwksValidation::default()),
            Err(JwtError::InvalidSignature)
        ));

        // Algorithm mismatch against JWK's `alg: "HS256"`
        let mut wrong_alg = Header::new(Algorithm::HS384);
        wrong_alg.kid = Some("key-1".to_owned());
        let wrong_token = encode(&wrong_alg, &claims, &EncodingKey::from_secret(secret)).unwrap();
        assert!(matches!(
            key_set.decode::<Claims>(&wrong_token, &JwksValidation::default()),
            Err(JwtError::InvalidAlgorithm)
        ));
    }

    #[cfg(feature = "oidc")]
    #[tokio::test]
    async fn remote_jwks_respects_kid_miss_cooldown() {
        use super::{FetchFn, RemoteJwks};
        use core::time::Duration;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let fetcher: FetchFn = Arc::new(move |_| {
            let calls_clone = Arc::clone(&calls_clone);
            Box::pin(async move {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(oct_jwks("key-1", b"secret"))
            })
        });

        let remote = RemoteJwks::with_fetcher("https://example.com/.well-known/jwks.json", fetcher)
            .with_refresh_ttl(Duration::from_secs(300))
            .with_kid_miss_cooldown(Duration::from_secs(60));

        let claims = Claims {
            sub: "alice".to_owned(),
            exp: u64::MAX,
        };
        let mut h1 = Header::new(Algorithm::HS256);
        h1.kid = Some("key-1".to_owned());
        let t1 = encode(&h1, &claims, &EncodingKey::from_secret(b"secret")).unwrap();

        let keys = remote.resolve_for_token(&t1).await.unwrap();
        assert!(keys.contains_kid("key-1"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Cached hit
        let _ = remote.resolve_for_token(&t1).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Unknown kid triggers at most one refresh, then cooldown blocks a second one
        let mut h2 = Header::new(Algorithm::HS256);
        h2.kid = Some("key-2".to_owned());
        let t2 = encode(&h2, &claims, &EncodingKey::from_secret(b"secret")).unwrap();

        let _ = remote.resolve_for_token(&t2).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let _ = remote.resolve_for_token(&t2).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
