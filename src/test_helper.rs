use core::mem;

use http_kit::{Endpoint, Request, Response, error::BoxHttpError, http_error};
use http_kit::StatusCode;

http_error!(pub BodyReadError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to read response body");
macro_rules! test_handler {
    (
        $endpoint:expr,
        $response:expr
        $(,request=$request:expr)?

    ) => {
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        #[allow(unused_mut)]
        #[allow(unused_assignments)]
        let mut request=http_kit::Request::get("http://localhost:0/");
        $(
            request=$request;
        )*
        runtime.block_on(crate::test_helper::test_endpoint_inner(crate::handler::into_endpoint($endpoint),$response.try_into().unwrap(),request)).unwrap();
    };
}

pub async fn test_endpoint_inner(
    mut endpoint: impl Endpoint + 'static,
    mut expected_response: Response,
    mut request: Request,
) -> Result<(), BoxHttpError> {
    let mut response = endpoint
        .respond(&mut request)
        .await
        .map_err(|error| Box::new(error) as BoxHttpError)?;

    let expected_status = expected_response.status();
    let status = response.status();
    assert!(
        expected_status == status,
        "Test failed,expected status {expected_status}, but found {status}"
    );

    let expected_response = mem::take(expected_response.body_mut())
        .into_bytes()
        .await
        .map_err(|_| Box::new(BodyReadError::new()) as BoxHttpError)?;

    let response = mem::take(response.body_mut())
        .into_bytes()
        .await
        .map_err(|_| Box::new(BodyReadError::new()) as BoxHttpError)?;

    assert!(
        expected_response == response,
        "Test failed,expected response body {expected_response:?}, but found {response:?}"
    );

    Ok(())
}
