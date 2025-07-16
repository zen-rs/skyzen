use http_kit::{Endpoint, Request, Response, Uri};
use skyzen_hyper::launch_local;

use zenwave::Client;
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
    endpoint: impl Endpoint + 'static,
    mut expected_response: Response,
    mut request: Request,
) -> Result<(), http_kit::Error> {
    let server = launch_local(endpoint, 0);
    let local_addr = server.local_addr();
    let mut uri = Uri::builder()
        .scheme("http")
        .authority(local_addr.to_string());
    if let Some(v) = request.uri().path_and_query() {
        uri = uri.path_and_query(v.to_owned());
    }

    request.set_uri(uri.build()?);
    tokio::task::spawn(server);

    let client = Client::new();

    let mut response = client.send(request).await?;

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
