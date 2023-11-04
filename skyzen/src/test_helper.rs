use http_kit::Endpoint;
use hyper::Server;
use skyzen_hyper::use_hyper;

pub async fn test_serve(endpoint: impl Endpoint + 'static) {
    femme::start();
    Server::bind(&([127, 0, 0, 1], 8080).into())
        .serve(use_hyper(endpoint))
        .await
        .unwrap();
}
