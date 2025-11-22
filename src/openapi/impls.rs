use std::{borrow::Cow, collections::BTreeMap};

use http_kit::{
    error::BoxHttpError,
    header::{HeaderMap, HeaderName, HeaderValue},
    utils::{ByteStr, Bytes},
    Body, Method, Uri,
};
use utoipa::openapi::schema::{ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::RefOr;

use crate::{
    openapi::{
        ExtractorOpenApiSchema, ExtractorSchema, ResponderOpenApiSchema, ResponseSchema, SchemaRef,
    },
    Error, Response, StatusCode,
};

#[cfg(feature = "websocket")]
use crate::websocket::{WebSocketUpgrade, WebSocketUpgradeResponder};

fn plain_string_schema() -> SchemaRef {
    RefOr::T(Schema::Object(
        ObjectBuilder::new()
            .schema_type(SchemaType::from(Type::String))
            .build(),
    ))
}

fn single_response(
    content_type: Option<&'static str>,
    schema: Option<SchemaRef>,
) -> Option<Vec<ResponseSchema>> {
    Some(vec![ResponseSchema {
        status: None,
        description: None,
        schema,
        content_type,
    }])
}

impl<T> ExtractorOpenApiSchema for Option<T>
where
    T: ExtractorOpenApiSchema,
{
    fn extractor_schema() -> Option<ExtractorSchema> {
        T::extractor_schema()
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        T::register_schemas(defs);
    }
}

impl<T> ExtractorOpenApiSchema for Result<T, BoxHttpError>
where
    T: ExtractorOpenApiSchema,
{
    fn extractor_schema() -> Option<ExtractorSchema> {
        T::extractor_schema()
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        T::register_schemas(defs);
    }
}

impl ExtractorOpenApiSchema for Bytes {
    fn extractor_schema() -> Option<ExtractorSchema> {
        Some(ExtractorSchema {
            content_type: Some("application/octet-stream"),
            schema: None,
        })
    }
}

impl ExtractorOpenApiSchema for ByteStr {
    fn extractor_schema() -> Option<ExtractorSchema> {
        Some(ExtractorSchema {
            content_type: Some("text/plain; charset=utf-8"),
            schema: None,
        })
    }
}

impl ExtractorOpenApiSchema for Body {
    fn extractor_schema() -> Option<ExtractorSchema> {
        Some(ExtractorSchema {
            content_type: Some("application/octet-stream"),
            schema: None,
        })
    }
}

impl ExtractorOpenApiSchema for Uri {
    fn extractor_schema() -> Option<ExtractorSchema> {
        None
    }
}

impl ExtractorOpenApiSchema for Method {
    fn extractor_schema() -> Option<ExtractorSchema> {
        None
    }
}

#[cfg(feature = "websocket")]
impl ExtractorOpenApiSchema for WebSocketUpgrade {
    fn extractor_schema() -> Option<ExtractorSchema> {
        None
    }
}

impl ResponderOpenApiSchema for Bytes {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(Some("application/octet-stream"), None)
    }
}

impl ResponderOpenApiSchema for Vec<u8> {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(Some("application/octet-stream"), None)
    }
}

impl ResponderOpenApiSchema for Body {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(Some("application/octet-stream"), None)
    }
}

impl ResponderOpenApiSchema for &'static [u8] {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(Some("application/octet-stream"), None)
    }
}

impl ResponderOpenApiSchema for Cow<'static, [u8]> {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(Some("application/octet-stream"), None)
    }
}

impl ResponderOpenApiSchema for ByteStr {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(
            Some("text/plain; charset=utf-8"),
            Some(plain_string_schema()),
        )
    }
}

impl ResponderOpenApiSchema for String {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(
            Some("text/plain; charset=utf-8"),
            Some(plain_string_schema()),
        )
    }
}

impl ResponderOpenApiSchema for &'static str {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(
            Some("text/plain; charset=utf-8"),
            Some(plain_string_schema()),
        )
    }
}

impl ResponderOpenApiSchema for Cow<'static, str> {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        single_response(
            Some("text/plain; charset=utf-8"),
            Some(plain_string_schema()),
        )
    }
}

impl ResponderOpenApiSchema for Response {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}

impl ResponderOpenApiSchema for HeaderMap {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}

impl ResponderOpenApiSchema for (HeaderName, HeaderValue) {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}

#[cfg(feature = "websocket")]
impl ResponderOpenApiSchema for WebSocketUpgradeResponder {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: Some(StatusCode::SWITCHING_PROTOCOLS),
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}

macro_rules! impl_tuple_responder_openapi {
    () => {
        impl ResponderOpenApiSchema for () {
            fn responder_schemas() -> Option<Vec<ResponseSchema>> {
                None
            }
        }
    };
    ($($ty:ident),+) => {
        impl<$($ty,)*> ResponderOpenApiSchema for ($($ty,)*)
        where
            $($ty: ResponderOpenApiSchema,)*
        {
            fn responder_schemas() -> Option<Vec<ResponseSchema>> {
                let mut schemas = Vec::new();
                $(if let Some(mut inner) = <$ty as ResponderOpenApiSchema>::responder_schemas() {
                    schemas.append(&mut inner);
                })*
                if schemas.is_empty() {
                    None
                } else {
                    Some(schemas)
                }
            }

            fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
                $(
                    <$ty as ResponderOpenApiSchema>::register_schemas(defs);
                )*
            }
        }
    };
}

macro_rules! openapi_tuples {
    ($macro:ident) => {
        $macro!();
        $macro!(T0);
        $macro!(T0, T1);
        $macro!(T0, T1, T2);
        $macro!(T0, T1, T2, T3);
        $macro!(T0, T1, T2, T3, T4);
        $macro!(T0, T1, T2, T3, T4, T5);
        $macro!(T0, T1, T2, T3, T4, T5, T6);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14);
    };
}

openapi_tuples!(impl_tuple_responder_openapi);

impl<T, E> ResponderOpenApiSchema for Result<T, E>
where
    T: ResponderOpenApiSchema,
    E: ResponderOpenApiSchema,
{
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        let mut schemas = Vec::new();
        if let Some(mut ok) = T::responder_schemas() {
            schemas.append(&mut ok);
        }
        if let Some(mut err) = E::responder_schemas() {
            schemas.append(&mut err);
        }

        if schemas.is_empty() {
            None
        } else {
            Some(schemas)
        }
    }

    fn register_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        T::register_schemas(defs);
        E::register_schemas(defs);
    }
}

impl ResponderOpenApiSchema for Error {
    fn responder_schemas() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: Some(StatusCode::SERVICE_UNAVAILABLE),
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}
