//! The in-band envelope for queue transports that carry text and nothing else.
//!
//! A [`MessageQueue`](super::MessageQueue) body is `Vec<u8>`, but some transports can only carry
//! text and offer no property channel to tag an encoding with — Azure Storage queues hold a
//! message as character data inside an XML document. The tag therefore has to travel *in* the body,
//! and this module owns that format so every party to it agrees:
//!
//! - Text that XML 1.0 can carry, and that does not begin with one of the prefixes below, travels
//!   **verbatim**: JSON produced by `send_json` arrives as plain JSON, readable by any other
//!   consumer of the queue.
//! - Anything else — binary, or text with characters XML 1.0 cannot represent — travels as
//!   [`BASE64_PREFIX`] followed by standard base64.
//! - Text that would itself begin with [`BASE64_PREFIX`] or [`UTF8_PREFIX`] travels as
//!   [`UTF8_PREFIX`] followed by the text, so it cannot be mistaken for one of the other two forms.
//!
//! That last rule is what makes the mapping injective, and injectivity is the whole promise:
//! [`decode`] returns exactly the bytes [`encode`] was given.
//!
//! # Why it lives here
//!
//! Two independent parties speak this format and neither may depend on the other: `skyzen-azure`
//! writes it from [`MessageQueue::send`](super::MessageQueue::send), and the framework's Azure
//! Functions integration reads it back off a message the *host* delivered. A platform crate never
//! depends on the framework crate, so the shared definition belongs below both — here.

use base64::Engine as _;

/// The envelope prefix marking a base64-encoded body.
pub const BASE64_PREFIX: &str = "skyzen-b64:";

/// The envelope prefix marking a body that is text but had to be escaped.
///
/// Only a payload that would otherwise be mistaken for an envelope carries it.
pub const UTF8_PREFIX: &str = "skyzen-utf8:";

/// The base64 alphabet the envelope uses.
const ENGINE: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// A body tagged [`BASE64_PREFIX`] whose payload is not base64.
///
/// The only way [`decode`] can fail: every other shape is bytes by construction.
#[derive(Debug)]
pub struct DecodeError(base64::DecodeError);

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "a message body prefixed {BASE64_PREFIX:?} is not valid base64: {}",
            self.0
        )
    }
}

impl core::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Whether `c` can be carried by an XML 1.0 document.
///
/// The W3C XML 1.0 character range: `#x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD] |
/// [#x10000-#x10FFFF]`. A Rust `char` is never a surrogate, so that range needs no check. This is
/// the same rule the SQS backend applies to its own bodies.
const fn is_xml_text_char(c: char) -> bool {
    matches!(c,
        '\u{9}' | '\u{A}' | '\u{D}'
        | '\u{20}'..='\u{D7FF}'
        | '\u{E000}'..='\u{FFFD}'
        | '\u{10000}'..='\u{10FFFF}')
}

/// Encode a payload into the envelope.
///
/// Applies no size limit: a transport's own cap is the transport's to enforce, on the encoded text
/// this returns.
#[must_use]
pub fn encode(message: &[u8]) -> String {
    match core::str::from_utf8(message) {
        Ok(text) if text.chars().all(is_xml_text_char) => {
            if text.starts_with(BASE64_PREFIX) || text.starts_with(UTF8_PREFIX) {
                // Escaping a body that already looks like an envelope is what keeps the encoding
                // injective: without it, `decode` could not tell this text from an encoded blob.
                format!("{UTF8_PREFIX}{text}")
            } else {
                text.to_owned()
            }
        }
        _ => format!("{BASE64_PREFIX}{}", ENGINE.encode(message)),
    }
}

/// Reverse [`encode`].
///
/// # Errors
///
/// [`DecodeError`] when the body is tagged [`BASE64_PREFIX`] but does not decode as base64 — which
/// means something other than [`encode`] wrote it.
pub fn decode(text: &str) -> Result<Vec<u8>, DecodeError> {
    if let Some(encoded) = text.strip_prefix(BASE64_PREFIX) {
        return ENGINE.decode(encoded).map_err(DecodeError);
    }

    Ok(text
        .strip_prefix(UTF8_PREFIX)
        .unwrap_or(text)
        .as_bytes()
        .to_vec())
}

#[cfg(test)]
mod tests {
    use super::{decode, encode, BASE64_PREFIX, UTF8_PREFIX};

    /// The round trip the wire format promises: what a producer encodes, a consumer decodes.
    fn round_trip(payload: &[u8]) -> Vec<u8> {
        decode(&encode(payload)).expect("payload should decode")
    }

    #[test]
    fn json_passes_through_unchanged() {
        let payload = br#"{"kind":"email"}"#;
        assert_eq!(encode(payload), r#"{"kind":"email"}"#);
        assert_eq!(round_trip(payload), payload.to_vec());
    }

    #[test]
    fn unicode_text_passes_through_unchanged() {
        let payload = "hello 世界".as_bytes();
        assert_eq!(encode(payload), "hello 世界");
        assert_eq!(round_trip(payload), payload.to_vec());
    }

    #[test]
    fn binary_is_base64_encoded_behind_the_prefix() {
        let payload = [0xFF, 0xFE, 0x00, 0x01];
        assert!(encode(&payload).starts_with(BASE64_PREFIX));
        assert_eq!(round_trip(&payload), payload.to_vec());
    }

    #[test]
    fn text_xml_cannot_carry_is_base64_encoded() {
        // A lone form feed is valid UTF-8 and invalid XML 1.0, so it cannot travel verbatim.
        let payload = b"before\x0Cafter";
        assert!(encode(payload).starts_with(BASE64_PREFIX));
        assert_eq!(round_trip(payload), payload.to_vec());
    }

    #[test]
    fn text_that_looks_like_an_envelope_is_escaped_so_the_encoding_stays_injective() {
        for payload in [
            format!("{BASE64_PREFIX}aGVsbG8="),
            format!("{UTF8_PREFIX}hello"),
        ] {
            assert_eq!(
                encode(payload.as_bytes()),
                format!("{UTF8_PREFIX}{payload}")
            );
            assert_eq!(round_trip(payload.as_bytes()), payload.as_bytes().to_vec());
        }
    }

    #[test]
    fn a_body_that_merely_looks_base64_is_not_decoded() {
        let payload = b"aGVsbG8=";
        assert_eq!(round_trip(payload), payload.to_vec());
    }

    #[test]
    fn a_body_prefixed_base64_that_is_not_base64_is_refused() {
        let error = decode(&format!("{BASE64_PREFIX}not base64!"))
            .expect_err("a malformed envelope should be refused");
        assert!(error.to_string().contains("base64"), "{error}");
    }
}
