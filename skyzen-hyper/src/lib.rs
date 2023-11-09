#![deny(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

//! The hyper backend of skyzen

use hyper::{server::conn::AddrIncoming, Server};
use service::IntoMakeService;

mod service;

/// Transform the `Endpoint` of skyzen into the `Service` of hyper
pub fn use_hyper<E: skyzen::Endpoint + Send + Sync>(endpoint: E) -> service::IntoMakeService<E> {
    service::IntoMakeService::new(endpoint)
}

/// Lanuch your service on local server.
pub fn launch_local<E: skyzen::Endpoint + 'static>(
    endpoint: E,
    port: u16,
) -> hyper::Server<AddrIncoming, IntoMakeService<E>> {
    let server = Server::bind(&([127, 0, 0, 1], port).into()).serve(use_hyper(endpoint));
    let local_addr = server.local_addr();
    log::info!("Server now running on {local_addr}");
    server
}
