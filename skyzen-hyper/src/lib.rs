#![deny(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

//! The hyper backend of skyzen

mod service;

/// Transform the `Endpoint` of skyzen into the `Service` of hyper
pub fn use_hyper<E: skyzen::Endpoint + Send + Sync>(endpoint: E) -> service::IntoMakeService<E> {
    service::IntoMakeService::new(endpoint)
}

#[cfg(test)]
mod test {
    use super::use_hyper;
    #[tokio::test]
    async fn test() {
        femme::start();
        use hyper::Server;
        use skyzen::{CreateRouteNode, Route};

        let route: Route = ["/".at(|| async move { "Hello,world!" })].into();

        Server::bind(&([127, 0, 0, 1], 8080).into()).serve(use_hyper(route.build()));
    }
}
