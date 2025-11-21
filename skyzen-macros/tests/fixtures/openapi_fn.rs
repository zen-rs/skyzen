use linkme_impl::distributed_slice;
use skyzen_macros::openapi;

extern crate self as skyzen;

// Minimal stub of the parts referenced by the macro expansion.
pub mod openapi {
    pub use linkme::distributed_slice;

    pub type SchemaRef = ();
    pub type SchemaFn = fn() -> Option<SchemaRef>;

    #[distributed_slice]
    pub static HANDLER_SPECS: [HandlerSpec] = [..];

    #[derive(Clone, Copy)]
    pub struct HandlerSpec {
        pub type_name: &'static str,
        pub docs: Option<&'static str>,
        pub parameters: &'static [SchemaFn],
        pub response: Option<SchemaFn>,
    }

    pub fn schema_of<T>() -> Option<SchemaRef> {
        let _ = core::marker::PhantomData::<T>;
        None
    }
}

#[openapi]
fn handler(_: i32) {}

fn main() {}
