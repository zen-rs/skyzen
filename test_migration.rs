use http_kit::{Request, Response, Body, Endpoint};

struct HelloEndpoint;

impl Endpoint for HelloEndpoint {
    async fn respond(&mut self, _request: &mut Request) -> http_kit::Result<Response> {
        let response = Response::new(Body::from("Hello, World!"));
        Ok(response)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut endpoint = HelloEndpoint;
    let mut request = Request::new(Body::empty());
    
    let response = endpoint.respond(&mut request).await?;
    println!("Status: {}", response.status());
    println!("Response created successfully!");
    
    Ok(())
}