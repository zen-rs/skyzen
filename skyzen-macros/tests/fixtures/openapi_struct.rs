#![allow(unexpected_cfgs)]

use linkme::distributed_slice;
use skyzen_macros::openapi;

extern crate self as skyzen;

mod utoipa {
    use std::borrow::Cow;
    pub trait ToSchema {
        fn name() -> Cow<'static, str> {
            Cow::Borrowed("Stub")
        }
        fn schema() -> crate::openapi::SchemaRef {
            ()
        }
        fn schemas(_: &mut Vec<(String, crate::openapi::SchemaRef)>) {}
    }

    impl<T> ToSchema for T {}
}

pub use crate::utoipa::ToSchema;
pub mod openapi {
    use linkme::distributed_slice;

    pub type SchemaRef = ();
    pub type ExtractorSchemaFn = fn() -> Option<ExtractorSchema>;
    pub type ResponderSchemaFn = fn() -> Option<Vec<ResponseSchema>>;
    pub type SchemaCollector = fn(&mut std::collections::BTreeMap<String, SchemaRef>);

    #[derive(Clone, Copy)]
    pub struct ExtractorSchema {
        pub content_type: Option<&'static str>,
        pub schema: Option<SchemaRef>,
    }

    #[derive(Clone, Copy)]
    pub struct ResponseSchema {
        pub status: Option<()>,
        pub description: Option<&'static str>,
        pub schema: Option<SchemaRef>,
        pub content_type: Option<&'static str>,
    }

    pub trait PartialSchema: crate::ToSchema {
        fn schema() -> SchemaRef;
    }

    pub trait ExtractorOpenApiSchema {
        fn extractor_schema() -> Option<ExtractorSchema>;
        fn register_schemas(_: &mut std::collections::BTreeMap<String, SchemaRef>) {}
    }

    pub trait ResponderOpenApiSchema {
        fn responder_schemas() -> Option<Vec<ResponseSchema>>;
        fn register_schemas(_: &mut std::collections::BTreeMap<String, SchemaRef>) {}
    }

    #[distributed_slice]
    pub static HANDLER_SPECS: [HandlerSpec] = [..];

    #[derive(Clone, Copy)]
    pub struct HandlerSpec {
        pub type_name: &'static str,
        pub operation_name: &'static str,
        pub docs: Option<&'static str>,
        pub deprecated: bool,
        pub parameters: &'static [ExtractorSchemaFn],
        pub parameter_names: &'static [&'static str],
        pub response: Option<ResponderSchemaFn>,
        pub schemas: &'static [SchemaCollector],
    }

    impl<T> PartialSchema for T
    where
        T: crate::ToSchema,
    {
        fn schema() -> SchemaRef {
            ()
        }
    }

    impl<T> ExtractorOpenApiSchema for T
    where
        T: PartialSchema,
    {
        fn extractor_schema() -> Option<ExtractorSchema> {
            Some(ExtractorSchema {
                content_type: None,
                schema: Some(()),
            })
        }
    }

    impl<T> ResponderOpenApiSchema for T
    where
        T: PartialSchema,
    {
        fn responder_schemas() -> Option<Vec<ResponseSchema>> {
            Some(vec![ResponseSchema {
                status: None,
                description: None,
                schema: None,
                content_type: None,
            }])
        }
    }

    pub fn extractor_schema_of<T: ExtractorOpenApiSchema>() -> Option<ExtractorSchema> {
        T::extractor_schema()
    }

    pub fn responder_schemas_of<T: ResponderOpenApiSchema>() -> Option<Vec<ResponseSchema>> {
        T::responder_schemas()
    }

    pub const fn trim_crate(path: &'static str) -> &'static str {
        path
    }
}

#[openapi]
struct NotAllowed {
    value: i32,
}

fn main() {}
//~ ERROR may only be applied to functions
