//! `wasm32` JWT and JWKS verification backed by `WebCrypto` (`crypto.subtle`).

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use js_sys::{Array, Object, Reflect, Uint8Array};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use super::jwt::JwtError;

/// Supported JWT signing algorithms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Algorithm {
    /// HMAC using SHA-256.
    HS256,
    /// HMAC using SHA-384.
    HS384,
    /// HMAC using SHA-512.
    HS512,
    /// RSASSA-PKCS1-v1_5 using SHA-256.
    RS256,
    /// RSASSA-PKCS1-v1_5 using SHA-384.
    RS384,
    /// RSASSA-PKCS1-v1_5 using SHA-512.
    RS512,
    /// RSASSA-PSS using SHA-256.
    PS256,
    /// RSASSA-PSS using SHA-384.
    PS384,
    /// RSASSA-PSS using SHA-512.
    PS512,
    /// ECDSA using P-256 and SHA-256.
    ES256,
    /// ECDSA using P-384 and SHA-384.
    ES384,
    /// `EdDSA` using Ed25519.
    EdDSA,
}

/// Decoded JWT header fields needed for key selection and algorithm verification.
#[derive(Clone, Debug, Deserialize)]
pub struct JwtHeader {
    pub alg: Algorithm,
    #[serde(default)]
    pub kid: Option<String>,
}

/// Validation parameters for `wasm32` JWT verification.
#[derive(Clone, Debug)]
pub struct WasmValidation {
    pub algorithms: Vec<Algorithm>,
    pub issuer: Option<Vec<String>>,
    pub audience: Option<Vec<String>>,
    pub validate_exp: bool,
    pub validate_nbf: bool,
    pub leeway: u64,
}

impl Default for WasmValidation {
    fn default() -> Self {
        Self {
            algorithms: vec![Algorithm::HS256],
            issuer: None,
            audience: None,
            validate_exp: true,
            validate_nbf: false,
            leeway: 60,
        }
    }
}

/// Key material stored by `JwtConfig` and `JwtKeySet` on `wasm32`.
#[derive(Clone)]
pub enum WasmKeyMaterial {
    Hmac(Vec<u8>),
    RsaSpki(Vec<u8>),
    EcSpki(Vec<u8>),
    Jwk(serde_json::Value),
}

/// Wrapper marking a single-threaded `wasm32` future as `Send`.
///
/// # Safety
/// `wasm32-unknown-unknown` has no threads, so a future never crosses a thread boundary.
pub struct SendFuture<F>(pub F);

#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<F> Send for SendFuture<F> {}

impl<F: Future> Future for SendFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `SendFuture` is a transparent wrapper around `F`.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.0) };
        inner.poll(cx)
    }
}

pub fn decode_header(token: &str) -> Result<JwtHeader, JwtError> {
    let header_b64 = token.split('.').next().ok_or(JwtError::Malformed)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|_| JwtError::Malformed)?;
    serde_json::from_slice(&bytes).map_err(|_| JwtError::Malformed)
}

pub fn parse_pem_spki(pem: &[u8], allow_pkcs1_rsa: bool) -> Result<Vec<u8>, JwtError> {
    let text = core::str::from_utf8(pem).map_err(|_| JwtError::Malformed)?;
    let mut lines = text.lines().map(str::trim);
    let begin = lines
        .find(|l| l.starts_with("-----BEGIN ") && l.ends_with("-----"))
        .ok_or(JwtError::Malformed)?;
    let is_pkcs1_rsa = begin == "-----BEGIN RSA PUBLIC KEY-----";
    if is_pkcs1_rsa && !allow_pkcs1_rsa {
        return Err(JwtError::Malformed);
    }
    let mut b64 = String::new();
    for line in lines {
        if line.starts_with("-----END ") {
            break;
        }
        b64.push_str(line);
    }
    let der = STANDARD.decode(b64).map_err(|_| JwtError::Malformed)?;
    if is_pkcs1_rsa {
        wrap_pkcs1_rsa_in_spki(&der)
    } else {
        Ok(der)
    }
}

fn wrap_pkcs1_rsa_in_spki(pkcs1: &[u8]) -> Result<Vec<u8>, JwtError> {
    // rsaEncryption OID: 1.2.840.113549.1.1.1 + NULL
    const RSA_ALG_ID: &[u8] = &[
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    let mut bit_string = Vec::with_capacity(pkcs1.len() + 5);
    bit_string.push(0x03);
    push_der_len(pkcs1.len() + 1, &mut bit_string)?;
    bit_string.push(0x00);
    bit_string.extend_from_slice(pkcs1);

    let inner_len = RSA_ALG_ID.len() + bit_string.len();
    let mut spki = Vec::with_capacity(inner_len + 4);
    spki.push(0x30);
    push_der_len(inner_len, &mut spki)?;
    spki.extend_from_slice(RSA_ALG_ID);
    spki.extend_from_slice(&bit_string);
    Ok(spki)
}

fn push_der_len(len: usize, out: &mut Vec<u8>) -> Result<(), JwtError> {
    if let Ok(short) = u8::try_from(len) {
        if short >= 0x80 {
            out.push(0x81);
        }
        out.push(short);
        Ok(())
    } else if let Ok(wide) = u16::try_from(len) {
        out.push(0x82);
        out.extend_from_slice(&wide.to_be_bytes());
        Ok(())
    } else {
        Err(JwtError::Malformed)
    }
}

fn subtle_crypto() -> Result<web_sys::SubtleCrypto, JwtError> {
    let global = js_sys::global();
    let crypto_val = Reflect::get(&global, &JsValue::from_str("crypto"))
        .map_err(|_| JwtError::InvalidSignature)?;
    let crypto: web_sys::Crypto = crypto_val
        .dyn_into()
        .map_err(|_| JwtError::InvalidSignature)?;
    Ok(crypto.subtle())
}

fn set_prop(obj: &Object, key: &str, val: &JsValue) -> Result<(), JwtError> {
    Reflect::set(obj, &JsValue::from_str(key), val).map_err(|_| JwtError::Malformed)?;
    Ok(())
}

fn import_algorithm_object(alg: Algorithm) -> Result<Object, JwtError> {
    let obj = Object::new();
    match alg {
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
            set_prop(&obj, "name", &JsValue::from_str("HMAC"))?;
            set_prop(&obj, "hash", &JsValue::from_str(hash_name(alg)))?;
        }
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
            set_prop(&obj, "name", &JsValue::from_str("RSASSA-PKCS1-v1_5"))?;
            set_prop(&obj, "hash", &JsValue::from_str(hash_name(alg)))?;
        }
        Algorithm::PS256 | Algorithm::PS384 | Algorithm::PS512 => {
            set_prop(&obj, "name", &JsValue::from_str("RSA-PSS"))?;
            set_prop(&obj, "hash", &JsValue::from_str(hash_name(alg)))?;
        }
        Algorithm::ES256 => {
            set_prop(&obj, "name", &JsValue::from_str("ECDSA"))?;
            set_prop(&obj, "namedCurve", &JsValue::from_str("P-256"))?;
        }
        Algorithm::ES384 => {
            set_prop(&obj, "name", &JsValue::from_str("ECDSA"))?;
            set_prop(&obj, "namedCurve", &JsValue::from_str("P-384"))?;
        }
        Algorithm::EdDSA => {
            set_prop(&obj, "name", &JsValue::from_str("Ed25519"))?;
        }
    }
    Ok(obj)
}

fn verify_algorithm_object(alg: Algorithm) -> Result<Object, JwtError> {
    let obj = Object::new();
    match alg {
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
            set_prop(&obj, "name", &JsValue::from_str("HMAC"))?;
        }
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
            set_prop(&obj, "name", &JsValue::from_str("RSASSA-PKCS1-v1_5"))?;
        }
        Algorithm::PS256 | Algorithm::PS384 | Algorithm::PS512 => {
            let salt_len = match alg {
                Algorithm::PS256 => 32.0,
                Algorithm::PS384 => 48.0,
                Algorithm::PS512 => 64.0,
                _ => unreachable!(),
            };
            set_prop(&obj, "name", &JsValue::from_str("RSA-PSS"))?;
            set_prop(&obj, "saltLength", &JsValue::from_f64(salt_len))?;
        }
        Algorithm::ES256 | Algorithm::ES384 => {
            set_prop(&obj, "name", &JsValue::from_str("ECDSA"))?;
            set_prop(&obj, "hash", &JsValue::from_str(hash_name(alg)))?;
        }
        Algorithm::EdDSA => {
            set_prop(&obj, "name", &JsValue::from_str("Ed25519"))?;
        }
    }
    Ok(obj)
}

const fn hash_name(alg: Algorithm) -> &'static str {
    match alg {
        Algorithm::HS256 | Algorithm::RS256 | Algorithm::PS256 | Algorithm::ES256 => "SHA-256",
        Algorithm::HS384 | Algorithm::RS384 | Algorithm::PS384 | Algorithm::ES384 => "SHA-384",
        Algorithm::HS512 | Algorithm::RS512 | Algorithm::PS512 | Algorithm::EdDSA => "SHA-512",
    }
}

pub async fn verify_and_decode<C: DeserializeOwned>(
    token: &str,
    material: &WasmKeyMaterial,
    alg: Algorithm,
    validation: &WasmValidation,
) -> Result<C, JwtError> {
    let mut parts = token.split('.');
    let header_b64 = parts.next().ok_or(JwtError::Malformed)?;
    let payload_b64 = parts.next().ok_or(JwtError::Malformed)?;
    let sig_b64 = parts.next().ok_or(JwtError::Malformed)?;
    if parts.next().is_some() {
        return Err(JwtError::Malformed);
    }

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| JwtError::Malformed)?;
    let signing_input = format!("{header_b64}.{payload_b64}");

    verify_signature(material, alg, signing_input.as_bytes(), &sig_bytes).await?;

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| JwtError::Malformed)?;
    validate_claims(&payload_bytes, validation)?;

    serde_json::from_slice(&payload_bytes).map_err(|_| JwtError::Malformed)
}

async fn verify_signature(
    material: &WasmKeyMaterial,
    alg: Algorithm,
    signing_input: &[u8],
    sig_bytes: &[u8],
) -> Result<(), JwtError> {
    let subtle = subtle_crypto()?;
    let import_alg = import_algorithm_object(alg)?;
    let usages = Array::of1(&JsValue::from_str("verify"));

    let import_promise = match material {
        WasmKeyMaterial::Hmac(secret) => {
            let key_data = Uint8Array::from(secret.as_slice());
            subtle
                .import_key_with_object("raw", &key_data, &import_alg, false, &usages)
                .map_err(|_| JwtError::InvalidSignature)?
        }
        WasmKeyMaterial::RsaSpki(der) | WasmKeyMaterial::EcSpki(der) => {
            let key_data = Uint8Array::from(der.as_slice());
            subtle
                .import_key_with_object("spki", &key_data, &import_alg, false, &usages)
                .map_err(|_| JwtError::InvalidSignature)?
        }
        WasmKeyMaterial::Jwk(jwk) => {
            let jwk_str = serde_json::to_string(jwk).map_err(|_| JwtError::Malformed)?;
            let jwk_obj: Object = js_sys::JSON::parse(&jwk_str)
                .map_err(|_| JwtError::Malformed)?
                .dyn_into()
                .map_err(|_| JwtError::Malformed)?;
            subtle
                .import_key_with_object("jwk", &jwk_obj, &import_alg, false, &usages)
                .map_err(|_| JwtError::InvalidSignature)?
        }
    };

    let key_js = SendFuture(JsFuture::from(import_promise))
        .await
        .map_err(|_| JwtError::InvalidSignature)?;
    let crypto_key: web_sys::CryptoKey =
        key_js.dyn_into().map_err(|_| JwtError::InvalidSignature)?;

    let verify_alg = verify_algorithm_object(alg)?;
    let sig_buf = sig_bytes.to_vec();
    let input_buf = signing_input.to_vec();
    let verify_promise = subtle
        .verify_with_object_and_u8_array_and_u8_array(
            &verify_alg,
            &crypto_key,
            &sig_buf,
            &input_buf,
        )
        .map_err(|_| JwtError::InvalidSignature)?;

    let verified = SendFuture(JsFuture::from(verify_promise))
        .await
        .map_err(|_| JwtError::InvalidSignature)?;
    if verified.as_bool() == Some(true) {
        Ok(())
    } else {
        Err(JwtError::InvalidSignature)
    }
}

#[derive(Deserialize)]
struct SpecClaims {
    #[serde(default)]
    exp: Option<f64>,
    #[serde(default)]
    nbf: Option<f64>,
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    aud: Option<serde_json::Value>,
}

fn validate_claims(payload_bytes: &[u8], validation: &WasmValidation) -> Result<(), JwtError> {
    let claims: SpecClaims =
        serde_json::from_slice(payload_bytes).map_err(|_| JwtError::Malformed)?;
    let now = js_sys::Date::now() / 1000.0;
    let leeway = f64::from(u32::try_from(validation.leeway).unwrap_or(u32::MAX));

    if validation.validate_exp {
        let exp = claims.exp.ok_or(JwtError::InvalidClaims)?;
        if now >= exp + leeway {
            return Err(JwtError::Expired);
        }
    }

    if validation.validate_nbf {
        if let Some(nbf) = claims.nbf {
            if now + leeway < nbf {
                return Err(JwtError::NotYetValid);
            }
        }
    }

    if let Some(expected_iss) = &validation.issuer {
        let Some(iss) = &claims.iss else {
            return Err(JwtError::InvalidClaims);
        };
        if !expected_iss.iter().any(|e| e == iss) {
            return Err(JwtError::InvalidClaims);
        }
    }

    if let Some(expected_aud) = &validation.audience {
        let Some(aud) = &claims.aud else {
            return Err(JwtError::InvalidClaims);
        };
        let matched = match aud {
            serde_json::Value::String(s) => expected_aud.iter().any(|e| e == s),
            serde_json::Value::Array(arr) => arr.iter().any(|item| {
                item.as_str()
                    .is_some_and(|s| expected_aud.iter().any(|e| e == s))
            }),
            _ => false,
        };
        if !matched {
            return Err(JwtError::InvalidClaims);
        }
    }

    Ok(())
}
