use http_kit::{Endpoint, Request, Response};
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
) -> Result<(), http_kit::Error> {
    let mut response = endpoint.respond(&mut request).await?;

    let expected_status = expected_response.status();
    let status = response.status();
    assert!(
        expected_status == status,
        "Test failed,expected status {expected_status}, but found {status}"
    );

    let expected_response = expected_response.take_body()?.into_bytes().await?;

    let response = response.take_body()?.into_bytes().await?;

    assert!(
        expected_response == response,
        "Test failed,expected response body {expected_response:?}, but found {response:?}"
    );

    Ok(())
}
