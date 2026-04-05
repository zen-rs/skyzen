#[cfg(any(feature = "json", feature = "form"))]
use serde::Serialize;
use utoipa::openapi::schema::{ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::RefOr;

use crate::{
    extract::client_ip::{ClientIp, PeerAddr},
    openapi::SchemaRef,
    routing::Params,
    utils::State,
};

#[cfg(feature = "form")]
use crate::extract::Query;

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

#[cfg(feature = "form")]
impl<T> utoipa::PartialSchema for Query<T>
where
    T: utoipa::ToSchema,
{
    fn schema() -> SchemaRef {
        <T as utoipa::PartialSchema>::schema()
    }
}

#[cfg(feature = "form")]
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

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use serde_json::Value;

    use super::*;

    #[derive(Debug, Serialize, utoipa::ToSchema)]
    struct Payload {
        name: String,
    }

    fn schema_json<T>() -> Value
    where
        T: utoipa::PartialSchema,
    {
        serde_json::to_value(T::schema()).unwrap()
    }

    #[test]
    fn built_in_route_and_address_schemas_expose_stable_metadata() {
        assert_eq!(<Params as utoipa::ToSchema>::name().as_ref(), "Params");
        assert_eq!(schema_json::<Params>()["title"], "Params");
        assert_eq!(
            schema_json::<Params>()["description"],
            "Route parameters extracted from the current path"
        );

        assert_eq!(schema_json::<ClientIp>()["type"], "string");
        assert_eq!(
            schema_json::<ClientIp>()["description"],
            "Client IP address (may be forwarded)"
        );
        assert_eq!(schema_json::<PeerAddr>()["type"], "string");
    }

    #[cfg(feature = "form")]
    #[test]
    fn form_and_query_wrappers_forward_inner_schema() {
        let inner = schema_json::<Payload>();
        assert_eq!(schema_json::<Form<Payload>>(), inner);
        assert_eq!(schema_json::<Query<Payload>>(), inner);
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_wrappers_forward_inner_schema() {
        let inner = schema_json::<Payload>();
        assert_eq!(schema_json::<Json<Payload>>(), inner);
        assert_eq!(schema_json::<PrettyJson<Payload>>(), inner);
    }

    #[test]
    fn state_and_ignore_openapi_use_empty_schema() {
        let empty_schema: SchemaRef = utoipa::openapi::schema::empty().into();
        let empty_schema = serde_json::to_value(empty_schema).unwrap();
        assert_eq!(schema_json::<State<String>>(), empty_schema);
        assert_eq!(schema_json::<IgnoreOpenApi<String>>(), empty_schema);
    }
}
