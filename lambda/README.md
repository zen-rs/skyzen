# skyzen-lambda

[![crates.io](https://img.shields.io/crates/v/skyzen-lambda.svg)](https://crates.io/crates/skyzen-lambda)
[![docs.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen-lambda)
[![License](https://img.shields.io/crates/l/skyzen-lambda.svg)](../LICENSE)

AWS Lambda runtime adapter for the [Skyzen](https://github.com/zen-rs/skyzen) HTTP framework.

## Overview

`skyzen-lambda` drives Skyzen applications on AWS Lambda. The same application binary that serves HTTP natively and compiles to a Cloudflare Worker runs as a Lambda function: `skyzen`'s runtime detects `AWS_LAMBDA_RUNTIME_API` in the environment and hands over to `skyzen_lambda::run` instead of binding a local TCP listener. Application code remains platform-agnostic and requires no annotations for Lambda.

## Event Sources

AWS Lambda multiplexes event sources onto a single entry point. `skyzen-lambda` inspects each payload:

- **HTTP**: Function URLs, API Gateway (REST and HTTP APIs), Application Load Balancer (ALB), and VPC Lattice. Requests are normalized into `http::Request` by `lambda_http`, handled by the Skyzen router, and converted to the expected response shape.
- **SQS**: Payloads with `eventSource: "aws:sqs"` are decoded into a portable `QueueBatch` and dispatched through the application's `#[skyzen::queue]` handler. Responses use [partial batch responses](https://docs.aws.amazon.com/lambda/latest/dg/services-sqs-errorhandling.html) so only failed messages are redelivered.

Any unsupported event payload is rejected with a descriptive error.

## Runtime Architecture

`lambda_runtime` requires Tokio. `skyzen-lambda` initializes and owns a dedicated Tokio runtime, constructing the application within it to ensure AWS SDK clients and async handles resolve properly.

`WorkerContext` (`waitUntil`) is deliberately unavailable on Lambda, as Lambda freezes the execution environment immediately after a response is returned.

## Usage

Applications enable the `lambda` feature on the root `skyzen` crate:

```toml
[dependencies]
skyzen = { version = "0.1", features = ["lambda"] }
```

When building or embedding an endpoint manually:

```rust
use skyzen_lambda::run;
use skyzen::routing::{CreateRouteNode, Route, Router};

fn app() -> Router {
    Route::new((
        "/hello".at(|| async { "Hello from AWS Lambda!" }),
    )).build()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run(|| async { Ok(app()) }).await
}
```

## Key Functions

| Item | Description |
|------|-------------|
| `run` | Runs the Lambda event loop with a factory producing the application endpoint |
| `run_with_queue` | Runs the Lambda event loop handling both HTTP requests and SQS queue batches |

## License

MIT or Apache-2.0, at your option.
