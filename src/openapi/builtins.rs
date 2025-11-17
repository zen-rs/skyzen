use std::net::SocketAddr;

use http_kit::{Request, Response};
use schemars::{
    gen::SchemaGenerator,
    schema::{InstanceType, Metadata, Schema, SchemaObject, SingleOrVec},
    JsonSchema,
};

use crate::{
    extract::{
        client_ip::{ClientIp, PeerAddr},
        Query,
    },
    routing::Params,
    utils::State,
    Body,
};

#[cfg(feature = "json")]
use crate::{responder::PrettyJson, utils::Json};
#[cfg(feature = "json")]
use serde::Serialize;

#[cfg(feature = "form")]
use crate::utils::Form;

use super::{schema_from_json_schema, OpenApiSchema};

fn string_schema(description: &'static str) -> Schema {
    let mut schema = SchemaObject::default();
    schema.instance_type = Some(SingleOrVec::Single(Box::new(InstanceType::String)));
    schema.metadata = Some(Box::new(Metadata {
        description: Some(description.to_owned()),
        ..Default::default()
    }));
    schema.into()
}

fn object_schema(title: &'static str, description: &'static str) -> Schema {
    let mut schema = SchemaObject::default();
    schema.instance_type = Some(SingleOrVec::Single(Box::new(InstanceType::Object)));
    schema.metadata = Some(Box::new(Metadata {
        title: Some(title.to_owned()),
        description: Some(description.to_owned()),
        ..Default::default()
    }));
    schema.into()
}

impl OpenApiSchema for Body {
    fn schema(_: &mut SchemaGenerator) -> Schema {
        string_schema("Raw HTTP body")
    }
}

impl OpenApiSchema for Request {
    fn schema(_: &mut SchemaGenerator) -> Schema {
        object_schema(
            "Request",
            "Opaque HTTP request. Prefer typed extractors for structured data.",
        )
    }
}

impl OpenApiSchema for Response {
    fn schema(_: &mut SchemaGenerator) -> Schema {
        object_schema(
            "Response",
            "Opaque HTTP response. Prefer typed responders for structured data.",
        )
    }
}

impl OpenApiSchema for http_kit::Error {
    fn schema(_: &mut SchemaGenerator) -> Schema {
        string_schema("Framework error variant")
    }
}

impl OpenApiSchema for Params {
    fn schema(_: &mut SchemaGenerator) -> Schema {
        object_schema("Params", "Route parameters extracted from the current path")
    }
}

impl OpenApiSchema for SocketAddr {
    fn schema(_: &mut SchemaGenerator) -> Schema {
        string_schema("Socket address (IP + port)")
    }
}

impl OpenApiSchema for ClientIp {
    fn schema(_: &mut SchemaGenerator) -> Schema {
        string_schema("Client IP address (may be forwarded)")
    }
}

impl OpenApiSchema for PeerAddr {
    fn schema(_: &mut SchemaGenerator) -> Schema {
        string_schema("Peer socket address reported by the transport")
    }
}

impl<T> OpenApiSchema for Query<T>
where
    T: JsonSchema + Send + Sync + 'static,
{
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        schema_from_json_schema::<T>(generator)
    }
}

#[cfg(feature = "form")]
impl<T> OpenApiSchema for Form<T>
where
    T: JsonSchema + Send + Sync + 'static,
{
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        schema_from_json_schema::<T>(generator)
    }
}

#[cfg(feature = "json")]
impl<T> OpenApiSchema for Json<T>
where
    T: JsonSchema + Send + Sync + 'static,
{
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        schema_from_json_schema::<T>(generator)
    }
}

#[cfg(feature = "json")]
impl<T> OpenApiSchema for PrettyJson<T>
where
    T: JsonSchema + Serialize + Send + Sync + 'static,
{
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        schema_from_json_schema::<T>(generator)
    }
}

impl<T> OpenApiSchema for State<T>
where
    T: JsonSchema + Clone + Send + Sync + 'static,
{
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        schema_from_json_schema::<T>(generator)
    }
}

impl<T> OpenApiSchema for http_kit::Result<T>
where
    T: OpenApiSchema,
{
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        T::schema(generator)
    }
}
