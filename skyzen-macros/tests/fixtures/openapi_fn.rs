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

// Minimal stub of the parts referenced by the macro expansion.
pub use crate::utoipa::ToSchema;
pub mod openapi {
    pub use linkme::distributed_slice;

    pub type SchemaRef = ();
    pub type SchemaFn = fn() -> Option<SchemaRef>;
    pub type SchemaCollector = fn(&mut Vec<(String, SchemaRef)>);

    pub trait PartialSchema: crate::ToSchema {
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

    impl<T> PartialSchema for T
    where
        T: crate::ToSchema,
    {
        fn schema() -> SchemaRef {
            ()
        }
    }

    pub fn schema_of<T: PartialSchema>() -> Option<SchemaRef> {
        let _ = <T as PartialSchema>::schema();
        None
    }
}

#[openapi]
fn handler(_: i32) {}

fn main() {}
