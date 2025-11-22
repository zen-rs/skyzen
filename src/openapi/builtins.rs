use std::collections::BTreeMap;

#[cfg(any(feature = "json", feature = "form"))]
use serde::Serialize;
use utoipa::openapi::schema::{ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::RefOr;

use crate::ignore_openapi;
use crate::{
    extract::{
        client_ip::{ClientIp, PeerAddr},
        Query,
    },
    openapi::{
        register_schema_for, schema_of, ExtractorOpenApiSchema, ExtractorSchema,
        ResponderOpenApiSchema, ResponseSchema, SchemaRef,
    },
    routing::Params,
    utils::{cookie::CookieJar, State},
};

#[cfg(feature = "multipart")]
use crate::utils::Multipart;

#[cfg(feature = "json")]
use crate::{responder::PrettyJson, utils::Json};

#[cfg(feature = "form")]
use crate::utils::Form;

fn string_schema(description: &'static str) -> SchemaRef {
    RefOr::T(Schema::Object(
        ObjectBuilder::new()
            .schema_type(SchemaType::from(Type::String))
            .description(Some(description))
            .build(),
    ))
}

fn object_schema(title: &'static str, description: &'static str) -> SchemaRef {
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
        impl ::utoipa::PartialSchema for $ty {
            fn schema() -> SchemaRef {
                $schema
            }
        }

        impl ::utoipa::ToSchema for $ty {
            fn name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(stringify!($ty))
            }
        }
    };
}

simple_schema!(
    Params,
    object_schema("Params", "Route parameters extracted from the current path")
);
simple_schema!(
    ClientIp,
    string_schema("Client IP address (may be forwarded)")
);
simple_schema!(
    PeerAddr,
    string_schema("Peer socket address reported by the transport")
);

impl ExtractorOpenApiSchema for Params {
    fn extractor_schema() -> Option<ExtractorSchema> {
        schema_of::<Self>().map(|schema| ExtractorSchema {
            content_type: None,
            schema: Some(schema),
        })
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<Self>(defs);
    }
}

impl ExtractorOpenApiSchema for ClientIp {
    fn extractor_schema() -> Option<ExtractorSchema> {
        schema_of::<Self>().map(|schema| ExtractorSchema {
            content_type: None,
            schema: Some(schema),
        })
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<Self>(defs);
    }
}

impl ExtractorOpenApiSchema for PeerAddr {
    fn extractor_schema() -> Option<ExtractorSchema> {
        schema_of::<Self>().map(|schema| ExtractorSchema {
            content_type: None,
            schema: Some(schema),
        })
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<Self>(defs);
    }
}

ignore_openapi!(CookieJar);

impl<T> utoipa::PartialSchema for Query<T>
where
    T: utoipa::ToSchema,
{
    fn schema() -> SchemaRef {
        <T as utoipa::PartialSchema>::schema()
    }
}

impl<T> utoipa::ToSchema for Query<T>
where
    T: utoipa::ToSchema,
{
    fn schemas(schemas: &mut Vec<(String, SchemaRef)>) {
        T::schemas(schemas);
    }
}

impl<T> ExtractorOpenApiSchema for Query<T>
where
    T: utoipa::PartialSchema + utoipa::ToSchema + Send + Sync + 'static,
{
    fn extractor_schema() -> Option<ExtractorSchema> {
        schema_of::<T>().map(|schema| ExtractorSchema {
            content_type: Some("application/x-www-form-urlencoded"),
            schema: Some(schema),
        })
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<T>(defs);
    }
}

#[cfg(feature = "multipart")]
impl ExtractorOpenApiSchema for Multipart {
    fn extractor_schema() -> Option<ExtractorSchema> {
        Some(ExtractorSchema {
            content_type: Some("multipart/form-data"),
            schema: None,
        })
    }
}

#[cfg(feature = "form")]
impl<T> utoipa::PartialSchema for Form<T>
where
    T: utoipa::ToSchema + 'static + Send + Sync,
{
    fn schema() -> SchemaRef {
        <T as utoipa::PartialSchema>::schema()
    }
}

#[cfg(feature = "form")]
impl<T> utoipa::ToSchema for Form<T>
where
    T: utoipa::ToSchema + 'static + Send + Sync,
{
    fn schemas(schemas: &mut Vec<(String, SchemaRef)>) {
        T::schemas(schemas);
    }
}

#[cfg(feature = "form")]
impl<T> ExtractorOpenApiSchema for Form<T>
where
    T: utoipa::PartialSchema + utoipa::ToSchema + Send + Sync + 'static,
{
    fn extractor_schema() -> Option<ExtractorSchema> {
        schema_of::<T>().map(|schema| ExtractorSchema {
            content_type: Some("application/x-www-form-urlencoded"),
            schema: Some(schema),
        })
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<T>(defs);
    }
}

#[cfg(feature = "form")]
impl<T> ResponderOpenApiSchema for Form<T>
where
    T: utoipa::PartialSchema + utoipa::ToSchema + Serialize + Send + Sync + 'static,
{
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: schema_of::<T>(),
            content_type: Some("application/x-www-form-urlencoded"),
        }])
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<T>(defs);
    }
}

#[cfg(feature = "json")]
impl<T> utoipa::PartialSchema for Json<T>
where
    T: utoipa::ToSchema + 'static + Send + Sync,
{
    fn schema() -> SchemaRef {
        <T as utoipa::PartialSchema>::schema()
    }
}

#[cfg(feature = "json")]
impl<T> utoipa::ToSchema for Json<T>
where
    T: utoipa::ToSchema + 'static + Send + Sync,
{
    fn schemas(schemas: &mut Vec<(String, SchemaRef)>) {
        T::schemas(schemas);
    }
}

#[cfg(feature = "json")]
impl<T> ExtractorOpenApiSchema for Json<T>
where
    T: utoipa::PartialSchema + utoipa::ToSchema + Send + Sync + 'static,
{
    fn extractor_schema() -> Option<ExtractorSchema> {
        schema_of::<T>().map(|schema| ExtractorSchema {
            content_type: Some("application/json"),
            schema: Some(schema),
        })
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<T>(defs);
    }
}

#[cfg(feature = "json")]
impl<T> ResponderOpenApiSchema for Json<T>
where
    T: utoipa::PartialSchema + utoipa::ToSchema + Serialize + Send + Sync + 'static,
{
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: schema_of::<T>(),
            content_type: Some("application/json"),
        }])
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<T>(defs);
    }
}

#[cfg(feature = "json")]
impl<T> utoipa::PartialSchema for PrettyJson<T>
where
    T: utoipa::ToSchema + Serialize + 'static + Send + Sync,
{
    fn schema() -> SchemaRef {
        <T as utoipa::PartialSchema>::schema()
    }
}

#[cfg(feature = "json")]
impl<T> utoipa::ToSchema for PrettyJson<T>
where
    T: utoipa::ToSchema + Serialize + 'static + Send + Sync,
{
    fn schemas(schemas: &mut Vec<(String, SchemaRef)>) {
        T::schemas(schemas);
    }
}

#[cfg(feature = "json")]
impl<T> ResponderOpenApiSchema for PrettyJson<T>
where
    T: utoipa::PartialSchema + utoipa::ToSchema + Serialize + Send + Sync + 'static,
{
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: schema_of::<T>(),
            content_type: Some("application/json"),
        }])
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        register_schema_for::<T>(defs);
    }
}

impl<T> utoipa::PartialSchema for State<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn schema() -> SchemaRef {
        utoipa::openapi::schema::empty().into()
    }
}

impl<T> utoipa::ToSchema for State<T> where T: Clone + Send + Sync + 'static {}

impl<T> ExtractorOpenApiSchema for State<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn extractor_schema() -> Option<ExtractorSchema> {
        None
    }
}

/// Wrapper that explicitly opts out of OpenAPI schema generation for contained extractors or
/// responders.
#[derive(Debug, Clone, Copy)]
pub struct IgnoreOpenApi<T>(pub T);

impl<T: Send + Sync + 'static> utoipa::PartialSchema for IgnoreOpenApi<T> {
    fn schema() -> SchemaRef {
        utoipa::openapi::schema::empty().into()
    }
}

impl<T: Send + Sync + 'static> utoipa::ToSchema for IgnoreOpenApi<T> {}

impl<T: Send + Sync + 'static> ExtractorOpenApiSchema for IgnoreOpenApi<T> {
    fn extractor_schema() -> Option<ExtractorSchema> {
        None
    }
}

impl<T: Send + Sync + 'static> ResponderOpenApiSchema for IgnoreOpenApi<T> {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        None
    }
}
