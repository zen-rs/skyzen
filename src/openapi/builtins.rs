use std::net::SocketAddr;

use http_kit::{Request, Response};
#[cfg(feature = "json")]
use serde::Serialize;
use utoipa::openapi::schema::{ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::RefOr;

use crate::{
    extract::{
        client_ip::{ClientIp, PeerAddr},
        Query,
    },
    openapi::OpenApiSchema,
    routing::Params,
    utils::State,
    Body,
};

#[cfg(feature = "json")]
use crate::{responder::PrettyJson, utils::Json};

#[cfg(feature = "form")]
use crate::utils::Form;

fn string_schema(description: &'static str) -> RefOr<Schema> {
    RefOr::T(Schema::Object(
        ObjectBuilder::new()
            .schema_type(SchemaType::from(Type::String))
            .description(Some(description))
            .build(),
    ))
}

fn object_schema(title: &'static str, description: &'static str) -> RefOr<Schema> {
    RefOr::T(Schema::Object(
        ObjectBuilder::new()
            .schema_type(SchemaType::from(Type::Object))
            .title(Some(title))
            .description(Some(description))
            .build(),
    ))
}

macro_rules! simple_schema {
    ($ty:ty, $schema:expr) => {
        impl OpenApiSchema for $ty {
            fn schema() -> RefOr<Schema> {
                $schema
            }
        }
    };
}

simple_schema!(Body, string_schema("Raw HTTP body"));
simple_schema!(
    Request,
    object_schema(
        "Request",
        "Opaque HTTP request. Prefer typed extractors for structured data."
    )
);
simple_schema!(
    Response,
    object_schema(
        "Response",
        "Opaque HTTP response. Prefer typed responders for structured data."
    )
);
simple_schema!(http_kit::Error, string_schema("Framework error variant"));
simple_schema!(
    Params,
    object_schema("Params", "Route parameters extracted from the current path")
);
simple_schema!(SocketAddr, string_schema("Socket address (IP + port)"));
simple_schema!(
    ClientIp,
    string_schema("Client IP address (may be forwarded)")
);
simple_schema!(
    PeerAddr,
    string_schema("Peer socket address reported by the transport")
);

impl<T> OpenApiSchema for Query<T>
where
    T: OpenApiSchema,
{
    fn schema() -> RefOr<Schema> {
        T::schema()
    }
}

#[cfg(feature = "form")]
impl<T> OpenApiSchema for Form<T>
where
    T: OpenApiSchema,
{
    fn schema() -> RefOr<Schema> {
        T::schema()
    }
}

#[cfg(feature = "json")]
impl<T> OpenApiSchema for Json<T>
where
    T: OpenApiSchema,
{
    fn schema() -> RefOr<Schema> {
        T::schema()
    }
}

#[cfg(feature = "json")]
impl<T> OpenApiSchema for PrettyJson<T>
where
    T: OpenApiSchema + Serialize,
{
    fn schema() -> RefOr<Schema> {
        T::schema()
    }
}

impl<T> OpenApiSchema for State<T>
where
    T: OpenApiSchema + Clone + Send + Sync + 'static,
{
    fn schema() -> RefOr<Schema> {
        T::schema()
    }
}
