#[cfg(any(feature = "json", feature = "form"))]
use serde::Serialize;
use utoipa::openapi::schema::{ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::RefOr;

use crate::{
    extract::{
        client_ip::{ClientIp, PeerAddr},
        Query,
    },
    openapi::SchemaRef,
    routing::Params,
    utils::State,
};

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

impl<T> utoipa::PartialSchema for State<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn schema() -> SchemaRef {
        utoipa::openapi::schema::empty().into()
    }
}

impl<T> utoipa::ToSchema for State<T> where T: Clone + Send + Sync + 'static {}

/// Wrapper that explicitly opts out of `OpenAPI` schema generation for contained extractors or
/// responders.
#[derive(Debug, Clone, Copy)]
pub struct IgnoreOpenApi<T>(pub T);

impl<T: Send + Sync + 'static> utoipa::PartialSchema for IgnoreOpenApi<T> {
    fn schema() -> SchemaRef {
        utoipa::openapi::schema::empty().into()
    }
}

impl<T: Send + Sync + 'static> utoipa::ToSchema for IgnoreOpenApi<T> {}
