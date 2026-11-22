//! OAuth 2.0 / `OpenID` Connect (OIDC) PKCE, discovery, and token helpers.

use core::fmt::Write as _;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use http::StatusCode;
use serde::{de::Deserializer, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use skyzen_core::Secret;

#[cfg(feature = "jwt")]
use serde::de::DeserializeOwned;

#[cfg(feature = "jwt")]
use super::{
    jwks::{JwksValidation, JwtKeySet},
    jwt::JwtError,
};

/// Error returned by OAuth 2.0 / OIDC operations.
#[skyzen::error(status = StatusCode::BAD_GATEWAY)]
pub enum OidcError {
    /// Failed to fetch OIDC discovery metadata or exchange an authorization code.
    #[error("OIDC HTTP request failed")]
    Http,
    /// The OIDC provider returned an invalid JSON payload.
    #[error("Invalid OIDC provider response")]
    InvalidResponse,
    /// The discovered metadata `issuer` did not match the requested issuer URL.
    #[error("OIDC issuer mismatch")]
    IssuerMismatch,
    /// The ID token failed signature or claims verification.
    #[error("Invalid ID token", status = StatusCode::UNAUTHORIZED)]
    InvalidIdToken,
    /// The ID token `nonce` claim did not match the expected nonce.
    #[error("Invalid ID token nonce", status = StatusCode::UNAUTHORIZED)]
    InvalidNonce,
}

#[cfg(feature = "jwt")]
impl From<JwtError> for OidcError {
    fn from(_: JwtError) -> Self {
        Self::InvalidIdToken
    }
}

fn random_b64url(num_bytes: usize) -> String {
    let mut bytes = vec![0u8; num_bytes];
    getrandom::getrandom(&mut bytes).expect("getrandom is available on every supported target");
    URL_SAFE_NO_PAD.encode(&bytes)
}

/// Generate a cryptographically random CSRF `state` parameter (256 bits, base64url-encoded).
#[must_use]
pub fn generate_state() -> String {
    random_b64url(32)
}

/// Generate a cryptographically random OIDC `nonce` parameter (256 bits, base64url-encoded).
#[must_use]
pub fn generate_nonce() -> String {
    random_b64url(32)
}

/// PKCE `code_verifier` (RFC 7636) backed by [`Secret`] so it is redacted in [`Debug`].
#[derive(Clone, Debug)]
pub struct PkceVerifier(Secret);

/// PKCE `S256` `code_challenge` derived from a [`PkceVerifier`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkceChallenge(String);

impl PkceVerifier {
    /// Generate a fresh 43-character RFC 7636 `code_verifier` using CSPRNG `getrandom`.
    #[must_use]
    pub fn generate() -> Self {
        Self(Secret::new(random_b64url(32)))
    }

    /// Wrap an existing `code_verifier` string.
    #[must_use]
    pub fn from_secret(verifier: impl Into<String>) -> Self {
        Self(Secret::new(verifier))
    }

    /// Derive the `S256` [`PkceChallenge`] (`BASE64URL_NOPAD(SHA256(verifier))`).
    #[must_use]
    pub fn challenge(&self) -> PkceChallenge {
        let digest = Sha256::digest(self.0.expose().as_bytes());
        PkceChallenge(URL_SAFE_NO_PAD.encode(digest))
    }

    /// Expose the raw `code_verifier` string for the token exchange request.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl PkceChallenge {
    /// The PKCE challenge method (`"S256"`).
    #[must_use]
    pub const fn method(&self) -> &'static str {
        "S256"
    }

    /// The base64url-encoded SHA-256 challenge string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Builder for OAuth 2.0 / OIDC authorization redirect URLs.
#[derive(Clone, Debug)]
pub struct AuthorizationRequest {
    authorization_endpoint: String,
    client_id: String,
    redirect_uri: String,
    response_type: String,
    scopes: Vec<String>,
    state: Option<String>,
    nonce: Option<String>,
    pkce: Option<PkceChallenge>,
    extra_params: Vec<(String, String)>,
}

impl AuthorizationRequest {
    /// Create a new authorization code flow request builder.
    #[must_use]
    pub fn new(
        authorization_endpoint: impl Into<String>,
        client_id: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            authorization_endpoint: authorization_endpoint.into(),
            client_id: client_id.into(),
            redirect_uri: redirect_uri.into(),
            response_type: "code".to_owned(),
            scopes: Vec::new(),
            state: None,
            nonce: None,
            pkce: None,
            extra_params: Vec::new(),
        }
    }

    /// Add a single OAuth/OIDC scope (e.g. `"openid"`).
    #[must_use]
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes.push(scope.into());
        self
    }

    /// Add multiple OAuth/OIDC scopes.
    #[must_use]
    pub fn with_scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }

    /// Set the CSRF `state` parameter.
    #[must_use]
    pub fn with_state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }

    /// Set the OIDC `nonce` parameter.
    #[must_use]
    pub fn with_nonce(mut self, nonce: impl Into<String>) -> Self {
        self.nonce = Some(nonce.into());
        self
    }

    /// Attach a PKCE `S256` challenge.
    #[must_use]
    pub fn with_pkce(mut self, challenge: &PkceChallenge) -> Self {
        self.pkce = Some(challenge.clone());
        self
    }

    /// Append an extra query parameter (e.g. `prompt=consent`).
    #[must_use]
    pub fn with_extra_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_params.push((key.into(), value.into()));
        self
    }

    /// Render the full authorization URL with percent-encoded query parameters.
    #[must_use]
    pub fn build_url(&self) -> String {
        let mut params: Vec<(&str, &str)> = vec![
            ("response_type", &self.response_type),
            ("client_id", &self.client_id),
            ("redirect_uri", &self.redirect_uri),
        ];
        let joined_scopes = self.scopes.join(" ");
        if !joined_scopes.is_empty() {
            params.push(("scope", &joined_scopes));
        }
        if let Some(state) = &self.state {
            params.push(("state", state));
        }
        if let Some(nonce) = &self.nonce {
            params.push(("nonce", nonce));
        }
        if let Some(pkce) = &self.pkce {
            params.push(("code_challenge", pkce.as_str()));
            params.push(("code_challenge_method", pkce.method()));
        }
        for (k, v) in &self.extra_params {
            params.push((k, v));
        }

        let sep = if self.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let mut url = String::with_capacity(self.authorization_endpoint.len() + 128);
        url.push_str(&self.authorization_endpoint);
        url.push(sep);
        encode_form_pairs(&params, &mut url);
        url
    }
}

/// OIDC Provider Metadata (`.well-known/openid-configuration`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OidcProviderMetadata {
    /// The provider's issuer identifier URL.
    pub issuer: String,
    /// URL of the OAuth 2.0 authorization endpoint.
    pub authorization_endpoint: String,
    /// URL of the OAuth 2.0 token endpoint.
    pub token_endpoint: String,
    /// URL of the OIDC `UserInfo` endpoint, if advertised.
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    /// URL of the provider's JSON Web Key Set document.
    pub jwks_uri: String,
    /// Supported OAuth 2.0 scopes, if advertised.
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
    /// Supported `response_type` values, if advertised.
    #[serde(default)]
    pub response_types_supported: Option<Vec<String>>,
    /// Supported ID token signing algorithms, if advertised.
    #[serde(default)]
    pub id_token_signing_alg_values_supported: Option<Vec<String>>,
}

impl OidcProviderMetadata {
    /// Fetch and validate `.well-known/openid-configuration` from `issuer_url`.
    ///
    /// Verifies that the returned `issuer` matches `issuer_url` (ignoring a trailing slash).
    ///
    /// # Errors
    ///
    /// Returns [`OidcError`] if the HTTP request fails, JSON parsing fails, or the issuer does
    /// not match.
    #[cfg(feature = "oidc")]
    pub async fn discover(issuer_url: &str) -> Result<Self, OidcError> {
        use zenwave::Client as _;

        let base = issuer_url.trim_end_matches('/');
        let discovery_url = format!("{base}/.well-known/openid-configuration");
        let mut client = zenwave::client();
        let response = client
            .get(&discovery_url)
            .map_err(|_| OidcError::Http)?
            .await
            .map_err(|_| OidcError::Http)?;
        if !response.status().is_success() {
            return Err(OidcError::Http);
        }
        let body = response
            .into_body()
            .into_string()
            .await
            .map_err(|_| OidcError::Http)?;
        let metadata: Self = serde_json::from_str(&body).map_err(|_| OidcError::InvalidResponse)?;
        if metadata.issuer.trim_end_matches('/') != base {
            return Err(OidcError::IssuerMismatch);
        }
        Ok(metadata)
    }
}

/// Payload for exchanging an authorization code at an OAuth 2.0 / OIDC token endpoint.
#[derive(Clone, Debug)]
pub struct TokenExchangeRequest {
    code: String,
    redirect_uri: String,
    client_id: String,
    client_secret: Option<Secret>,
    code_verifier: Option<PkceVerifier>,
}

impl TokenExchangeRequest {
    /// Create a new `authorization_code` token exchange payload.
    #[must_use]
    pub fn authorization_code(
        code: impl Into<String>,
        redirect_uri: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            redirect_uri: redirect_uri.into(),
            client_id: client_id.into(),
            client_secret: None,
            code_verifier: None,
        }
    }

    /// Attach a confidential client secret.
    #[must_use]
    pub fn with_client_secret(mut self, secret: Secret) -> Self {
        self.client_secret = Some(secret);
        self
    }

    /// Attach the PKCE `code_verifier` corresponding to the authorization request's challenge.
    #[must_use]
    pub fn with_code_verifier(mut self, verifier: PkceVerifier) -> Self {
        self.code_verifier = Some(verifier);
        self
    }

    /// Encode this request as an `application/x-www-form-urlencoded` body.
    #[must_use]
    pub fn to_form_body(&self) -> String {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", &self.code),
            ("redirect_uri", &self.redirect_uri),
            ("client_id", &self.client_id),
        ];
        if let Some(secret) = &self.client_secret {
            pairs.push(("client_secret", secret.expose()));
        }
        if let Some(verifier) = &self.code_verifier {
            pairs.push(("code_verifier", verifier.expose()));
        }
        let mut body = String::with_capacity(128);
        encode_form_pairs(&pairs, &mut body);
        body
    }
}

fn deserialize_secret<'de, D>(deserializer: D) -> Result<Secret, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Secret::new)
}

fn deserialize_opt_secret<'de, D>(deserializer: D) -> Result<Option<Secret>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|opt| opt.map(Secret::new))
}

/// Response returned by an OAuth 2.0 / OIDC token endpoint.
///
/// `access_token` and `refresh_token` are stored as [`Secret`]s so they are redacted in [`Debug`].
#[derive(Clone, Debug, Deserialize)]
pub struct TokenResponse {
    /// Access token issued by the authorization server.
    #[serde(deserialize_with = "deserialize_secret")]
    pub access_token: Secret,
    /// Token type (typically `"Bearer"`).
    pub token_type: String,
    /// Lifetime in seconds of the access token, if provided.
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// Refresh token, if issued.
    #[serde(default, deserialize_with = "deserialize_opt_secret")]
    pub refresh_token: Option<Secret>,
    /// OIDC ID token JWT, if issued.
    #[serde(default)]
    pub id_token: Option<String>,
    /// Space-delimited granted scopes, if returned.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Exchange an authorization code at `token_endpoint` using `zenwave`.
///
/// # Errors
///
/// Returns [`OidcError`] if the HTTP request fails or the token response cannot be parsed.
#[cfg(feature = "oidc")]
pub async fn exchange_code(
    token_endpoint: &str,
    request: &TokenExchangeRequest,
) -> Result<TokenResponse, OidcError> {
    use zenwave::{header, Client as _};

    let body = request.to_form_body();
    let mut client = zenwave::client();
    let response = client
        .post(token_endpoint)
        .map_err(|_| OidcError::Http)?
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .map_err(|_| OidcError::Http)?
        .bytes_body(body.into_bytes())
        .await
        .map_err(|_| OidcError::Http)?;

    if !response.status().is_success() {
        return Err(OidcError::Http);
    }

    let text = response
        .into_body()
        .into_string()
        .await
        .map_err(|_| OidcError::Http)?;
    serde_json::from_str(&text).map_err(|_| OidcError::InvalidResponse)
}

/// Standard OIDC ID token claims.
#[cfg(feature = "jwt")]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdTokenClaims {
    /// Subject identifier.
    pub sub: String,
    /// Issuer identifier.
    pub iss: String,
    /// Audience (single string or first entry).
    pub aud: serde_json::Value,
    /// Expiration timestamp (seconds since UNIX epoch).
    pub exp: u64,
    /// Issued-at timestamp (seconds since UNIX epoch).
    #[serde(default)]
    pub iat: Option<u64>,
    /// Nonce bound to the authentication request.
    #[serde(default)]
    pub nonce: Option<String>,
}

#[cfg(feature = "jwt")]
#[derive(Deserialize)]
struct NonceCheck {
    #[serde(default)]
    nonce: Option<String>,
}

/// Verify an OIDC ID token against `keys`, checking `iss`, `aud`, `exp`, and `expected_nonce`.
///
/// # Errors
///
/// Returns [`OidcError::InvalidIdToken`] if signature/claims validation fails, or
/// [`OidcError::InvalidNonce`] if `expected_nonce` is `Some` and does not match the token's
/// `nonce` claim.
#[cfg(feature = "jwt")]
pub async fn verify_id_token<C: DeserializeOwned>(
    id_token: &str,
    keys: &JwtKeySet,
    issuer: &str,
    audience: &str,
    expected_nonce: Option<&str>,
) -> Result<C, OidcError> {
    let validation = JwksValidation::new()
        .with_issuer(issuer)
        .with_audience(audience);

    if let Some(expected) = expected_nonce {
        let claims = keys
            .decode_claims::<serde_json::Value>(id_token, &validation)
            .await?;
        let nonce_check: NonceCheck =
            serde_json::from_value(claims.clone()).map_err(|_| OidcError::InvalidNonce)?;
        if nonce_check.nonce.as_deref() != Some(expected) {
            return Err(OidcError::InvalidNonce);
        }
        serde_json::from_value(claims).map_err(|_| OidcError::InvalidResponse)
    } else {
        Ok(keys.decode_claims::<C>(id_token, &validation).await?)
    }
}

fn encode_form_pairs(pairs: &[(&str, &str)], out: &mut String) {
    for (idx, (k, v)) in pairs.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        percent_encode(k, out);
        out.push('=');
        percent_encode(v, out);
    }
}

fn percent_encode(input: &str, out: &mut String) {
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        generate_nonce, generate_state, AuthorizationRequest, OidcProviderMetadata, PkceVerifier,
        TokenExchangeRequest, TokenResponse,
    };

    #[test]
    fn pkce_s256_matches_rfc7636_vector() {
        // RFC 7636 Appendix B test vector:
        // verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        // challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        let verifier = PkceVerifier::from_secret("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            verifier.challenge().as_str(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(verifier.challenge().method(), "S256");

        let debug = format!("{verifier:?}");
        assert!(!debug.contains("dBjftJeZ4CVP"));
        assert!(debug.contains("Secret(<redacted>)"));
    }

    #[test]
    fn generates_distinct_random_state_nonce_and_pkce() {
        let s1 = generate_state();
        let s2 = generate_state();
        assert_ne!(s1, s2);
        assert!(s1.len() >= 43);

        let n1 = generate_nonce();
        let n2 = generate_nonce();
        assert_ne!(n1, n2);

        let v1 = PkceVerifier::generate();
        let v2 = PkceVerifier::generate();
        assert_ne!(v1.expose(), v2.expose());
        assert_eq!(v1.expose().len(), 43);
    }

    #[test]
    fn builds_authorization_url_and_token_form_body() {
        let verifier = PkceVerifier::generate();
        let challenge = verifier.challenge();
        let url = AuthorizationRequest::new(
            "https://accounts.example.com/authorize",
            "client-123",
            "https://app.example.com/callback",
        )
        .with_scopes(["openid", "profile", "email"])
        .with_state("st_1")
        .with_nonce("nc_1")
        .with_pkce(&challenge)
        .build_url();

        assert!(url.starts_with("https://accounts.example.com/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("scope=openid%20profile%20email"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("code_challenge={}", challenge.as_str())));

        let form = TokenExchangeRequest::authorization_code(
            "auth_code",
            "https://app.example.com/callback",
            "client-123",
        )
        .with_code_verifier(verifier)
        .to_form_body();
        assert!(form.contains("grant_type=authorization_code"));
        assert!(form.contains("code=auth_code"));
        assert!(form.contains("code_verifier="));
    }

    #[test]
    fn parses_discovery_and_redacts_token_response_debug() {
        let doc = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/oauth/token",
            "userinfo_endpoint": "https://auth.example.com/userinfo",
            "jwks_uri": "https://auth.example.com/.well-known/jwks.json"
        }"#;
        let meta: OidcProviderMetadata = serde_json::from_str(doc).unwrap();
        assert_eq!(meta.issuer, "https://auth.example.com");
        assert_eq!(
            meta.jwks_uri,
            "https://auth.example.com/.well-known/jwks.json"
        );

        let token_json = r#"{
            "access_token": "at_secret_value",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "rt_secret_value"
        }"#;
        let resp: TokenResponse = serde_json::from_str(token_json).unwrap();
        assert_eq!(resp.access_token.expose(), "at_secret_value");
        let debug = format!("{resp:?}");
        assert!(!debug.contains("at_secret_value"));
        assert!(!debug.contains("rt_secret_value"));
    }

    #[cfg(feature = "jwt")]
    #[tokio::test]
    async fn verifies_id_token_with_nonce() {
        use super::{verify_id_token, IdTokenClaims, OidcError};
        use crate::auth::jwks::JwtKeySet;
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

        let secret = b"oidc-test-secret";
        let jwks_json = format!(
            r#"{{"keys":[{{"kty":"oct","kid":"k1","use":"sig","alg":"HS256","k":"{}"}}]}}"#,
            URL_SAFE_NO_PAD.encode(secret)
        );
        let keys = JwtKeySet::from_json(&jwks_json).unwrap();
        let claims = IdTokenClaims {
            sub: "user-1".to_owned(),
            iss: "https://issuer.example".to_owned(),
            aud: serde_json::Value::String("client-1".to_owned()),
            exp: u64::MAX,
            iat: Some(1_700_000_000),
            nonce: Some("expected-nonce".to_owned()),
        };
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("k1".to_owned());
        let id_token = encode(&header, &claims, &EncodingKey::from_secret(secret)).unwrap();

        let verified: IdTokenClaims = verify_id_token(
            &id_token,
            &keys,
            "https://issuer.example",
            "client-1",
            Some("expected-nonce"),
        )
        .await
        .unwrap();
        assert_eq!(verified, claims);

        let bad_nonce = verify_id_token::<IdTokenClaims>(
            &id_token,
            &keys,
            "https://issuer.example",
            "client-1",
            Some("wrong-nonce"),
        )
        .await;
        assert!(matches!(bad_nonce, Err(OidcError::InvalidNonce)));
    }
}
