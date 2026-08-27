//! Converting between the buffered bodies Lambda speaks and the streaming bodies Skyzen does.
//!
//! Everything else about an HTTP invocation — which event shape arrived, how its path and query
//! were spelled, which response shape the caller expects back — is [`lambda_http`]'s job, and this
//! module deliberately does not second-guess it.

use http_kit::{Body, BodyError};
use lambda_http::Body as LambdaBody;

/// Turn a buffered Lambda request body into a Skyzen body.
#[must_use]
pub fn into_skyzen_body(body: LambdaBody) -> Body {
    match body {
        LambdaBody::Empty => Body::empty(),
        LambdaBody::Text(text) => Body::from_bytes(text),
        LambdaBody::Binary(bytes) => Body::from_bytes(bytes),
        // `Body` is `#[non_exhaustive]`: a variant added upstream must not silently become an
        // empty request body, so it is refused here instead.
        other => unreachable!("lambda_http delivered an unsupported body variant: {other:?}"),
    }
}

/// Collect a Skyzen response body into the buffered form Lambda returns.
///
/// A body that is valid UTF-8 travels as text and anything else as base64-encoded binary, which is
/// what makes the round trip byte-faithful: `Text` is delivered verbatim, `Binary` is decoded by
/// the caller, and a compressed or otherwise non-textual response therefore arrives intact rather
/// than mangled by a lossy UTF-8 conversion.
///
/// # Errors
///
/// Returns the body's own error when the response stream fails part way through. There is nothing
/// to send at that point — Lambda has no partial responses on this path — so the invocation fails.
pub async fn into_lambda_body(body: Body) -> Result<LambdaBody, BodyError> {
    let bytes = body.into_bytes().await?;
    if bytes.is_empty() {
        return Ok(LambdaBody::Empty);
    }

    Ok(match String::from_utf8(bytes.to_vec()) {
        Ok(text) => LambdaBody::Text(text),
        Err(error) => LambdaBody::Binary(error.into_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::{into_lambda_body, into_skyzen_body};
    use http_kit::Body;
    use lambda_http::Body as LambdaBody;

    fn block_on<T>(future: impl core::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime")
            .block_on(future)
    }

    #[test]
    fn an_empty_body_stays_empty_in_both_directions() {
        let collected = block_on(into_skyzen_body(LambdaBody::Empty).into_bytes())
            .expect("an empty body collects");
        assert!(collected.is_empty());

        assert_eq!(
            block_on(into_lambda_body(Body::empty())).expect("collects"),
            LambdaBody::Empty
        );
    }

    #[test]
    fn a_text_body_round_trips_verbatim() {
        let request = into_skyzen_body(LambdaBody::Text("{\"hello\":\"world\"}".to_owned()));
        let response = block_on(into_lambda_body(request)).expect("collects");

        assert_eq!(
            response,
            LambdaBody::Text("{\"hello\":\"world\"}".to_owned())
        );
    }

    #[test]
    fn a_binary_body_round_trips_byte_for_byte() {
        let payload = vec![0x1f, 0x8b, 0x08, 0x00, 0xff];
        let request = into_skyzen_body(LambdaBody::Binary(payload.clone()));
        let response = block_on(into_lambda_body(request)).expect("collects");

        // Not valid UTF-8, so it travels as binary — which is what makes API Gateway base64 it.
        assert_eq!(response, LambdaBody::Binary(payload));
    }

    #[test]
    fn a_binary_body_that_happens_to_be_utf8_travels_as_text() {
        let request = into_skyzen_body(LambdaBody::Binary(b"plain".to_vec()));
        let response = block_on(into_lambda_body(request)).expect("collects");

        // Byte-faithful either way; text is the cheaper of the two on the wire.
        assert_eq!(response, LambdaBody::Text("plain".to_owned()));
    }
}
