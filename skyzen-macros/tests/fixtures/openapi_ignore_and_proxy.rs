#![allow(unexpected_cfgs)]

use linkme_impl::distributed_slice;
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
    pub use linkme::distributed_slice;
    use crate::ProxySchema;

    pub type SchemaRef = ();
    pub type ExtractorSchemaFn = fn() -> Option<ExtractorSchema>;
    pub type ResponderSchemaFn = fn() -> Option<Vec<ResponseSchemaMeta>>;
    pub type SchemaCollector = fn(&mut std::collections::BTreeMap<String, SchemaRef>);

    #[derive(Clone, Copy)]
    pub struct ExtractorSchema {
        pub content_type: Option<&'static str>,
        pub schema: Option<SchemaRef>,
    }

    #[derive(Clone, Copy)]
    pub struct ResponseSchemaMeta {
        pub status: Option<()>,
        pub description: Option<&'static str>,
        pub schema: Option<SchemaRef>,
        pub content_type: Option<&'static str>,
    }

    pub trait ToSchema {}

    pub trait PartialSchema: ToSchema {
        fn schema() -> SchemaRef;
    }

    pub trait ExtractorOpenApiSchema {
        fn extractor_schema() -> Option<ExtractorSchema>;
        fn register_schemas(_: &mut std::collections::BTreeMap<String, SchemaRef>) {}
    }

    pub trait ResponderOpenApiSchema {
        fn responder_schemas() -> Option<Vec<ResponseSchemaMeta>>;
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

    impl PartialSchema for ProxySchema {
        fn schema() -> SchemaRef {
            ()
        }
    }

    impl PartialSchema for crate::ResponseSchema {
        fn schema() -> SchemaRef {
            ()
        }
    }

    impl ToSchema for ProxySchema {}
    impl ToSchema for crate::ResponseSchema {}

    impl ExtractorOpenApiSchema for ProxySchema {
        fn extractor_schema() -> Option<ExtractorSchema> {
            Some(ExtractorSchema {
                content_type: None,
                schema: Some(()),
            })
        }
    }

    impl ResponderOpenApiSchema for crate::ResponseSchema {
        fn responder_schemas() -> Option<Vec<ResponseSchemaMeta>> {
            Some(vec![ResponseSchemaMeta {
                status: None,
                description: None,
                schema: None,
                content_type: None,
            }])
        }
    }

    impl ResponderOpenApiSchema for Result<crate::ResponseSchema, ()> {
        fn responder_schemas() -> Option<Vec<ResponseSchemaMeta>> {
            Some(Vec::new())
        }
    }

    pub fn extractor_schema_of<T: ExtractorOpenApiSchema>() -> Option<ExtractorSchema> {
        T::extractor_schema()
    }

    pub fn responder_schemas_of<T: ResponderOpenApiSchema>() -> Option<Vec<ResponseSchemaMeta>> {
        T::responder_schemas()
    }

    pub const fn trim_crate(path: &'static str) -> &'static str {
        path
    }
}

struct RawBody;
struct ProxiedBody;
struct ProxySchema;
struct ResponseSchema;

#[openapi]
fn handler(
    #[ignore] _raw: RawBody,
    #[proxy(ProxySchema)] _body: ProxiedBody,
) -> Result<ResponseSchema, ()> {
    unimplemented!()
}

fn main() {}
