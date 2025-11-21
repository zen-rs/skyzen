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

    pub type SchemaRef = ();
    pub type SchemaFn = fn() -> Option<SchemaRef>;
    pub type SchemaCollector = fn(&mut std::collections::BTreeMap<String, SchemaRef>);

    pub trait RegisterSchemas {
        fn register(defs: &mut std::collections::BTreeMap<String, SchemaRef>);
    }

    pub trait ToSchema {}

    pub trait PartialSchema: ToSchema {
        fn schema() -> SchemaRef;
    }

    #[distributed_slice]
    pub static HANDLER_SPECS: [HandlerSpec] = [..];

    #[derive(Clone, Copy)]
    pub struct HandlerSpec {
        pub type_name: &'static str,
        pub docs: Option<&'static str>,
        pub parameters: &'static [SchemaFn],
        pub response: Option<SchemaFn>,
        pub schemas: &'static [SchemaCollector],
    }

    pub fn schema_of<T: PartialSchema>() -> Option<SchemaRef> {
        let _ = <T as PartialSchema>::schema();
        None
    }

    impl<T> RegisterSchemas for T {
        fn register(_: &mut std::collections::BTreeMap<String, SchemaRef>) {}
    }
}

struct RawBody;
struct ProxiedBody;
struct ProxySchema;
struct ResponseSchema;

impl openapi::ToSchema for ProxySchema {}
impl openapi::PartialSchema for ProxySchema {
    fn schema() -> openapi::SchemaRef {
        ()
    }
}

impl openapi::ToSchema for ResponseSchema {}
impl openapi::PartialSchema for ResponseSchema {
    fn schema() -> openapi::SchemaRef {
        ()
    }
}

#[openapi]
fn handler(
    #[ignore] _raw: RawBody,
    #[proxy(ProxySchema)] _body: ProxiedBody,
) -> Result<ResponseSchema, ()> {
    unimplemented!()
}

fn main() {}
