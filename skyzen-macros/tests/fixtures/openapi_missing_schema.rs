use linkme_impl::distributed_slice;
use skyzen_macros::openapi;

extern crate self as skyzen;

pub mod openapi {
    pub use linkme::distributed_slice;

    pub type SchemaRef = ();
    pub type SchemaFn = fn() -> Option<SchemaRef>;

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
    }

    pub fn schema_of<T: PartialSchema>() -> Option<SchemaRef> {
        let _ = <T as PartialSchema>::schema();
        None
    }
}

struct MissingSchema;
struct ResponseSchema;

impl openapi::ToSchema for ResponseSchema {}
impl openapi::PartialSchema for ResponseSchema {
    fn schema() -> openapi::SchemaRef {
        ()
    }
}

#[openapi]
fn handler(_param: MissingSchema) -> ResponseSchema {
    ResponseSchema
}

fn main() {}
