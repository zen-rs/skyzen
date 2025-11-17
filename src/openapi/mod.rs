//! OpenAPI helpers powered by `utoipa` schemas.

use std::fmt;

use http_kit::Method;
use utoipa::openapi::{schema::Schema, RefOr};

#[cfg(debug_assertions)]
use linkme::distributed_slice;

mod builtins;

/// Assert that `T` can produce an OpenAPI schema.
#[doc(hidden)]
pub const fn assert_schema<T>()
where
    T: OpenApiSchema,
{
    let _ = ::core::marker::PhantomData::<T>;
}

/// Trait implemented by extractors and responders that can describe themselves via OpenAPI schema.
pub trait OpenApiSchema: Send + Sync + 'static {
    /// Produce a [`Schema`] for the implementing type.
    fn schema() -> RefOr<Schema>;
}

/// Helper function referenced by the procedural macro to obtain a [`Schema`].
pub fn schema_of<T>() -> RefOr<Schema>
where
    T: OpenApiSchema,
{
    T::schema()
}

#[cfg(debug_assertions)]
/// Function pointer used to lazily build a [`Schema`].
pub type SchemaFn = fn() -> RefOr<Schema>;

#[cfg(debug_assertions)]
/// Distributed registry containing handler specifications discovered via `#[skyzen::openapi]`.
#[distributed_slice]
pub static HANDLER_SPECS: [HandlerSpec] = [..];

#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy)]
/// Metadata captured for every handler annotated with `#[skyzen::openapi]`.
pub struct HandlerSpec {
    /// Fully-qualified handler name (module + function).
    pub type_name: &'static str,
    /// Documentation collected from the handler's doc comments.
    pub docs: Option<&'static str>,
    /// Schema generators for each extractor argument.
    pub parameters: &'static [SchemaFn],
    /// Schema generator for the responder type.
    pub response: SchemaFn,
}

#[cfg(debug_assertions)]
fn find_handler_spec(type_name: &str) -> Option<&'static HandlerSpec> {
    HANDLER_SPECS
        .iter()
        .find(|spec| spec.type_name == type_name)
}

/// Handler metadata attached to each endpoint.
#[derive(Clone, Copy, Debug)]
pub struct RouteHandlerDoc {
    #[cfg(debug_assertions)]
    type_name: &'static str,
    #[cfg(debug_assertions)]
    spec: Option<&'static HandlerSpec>,
}

impl RouteHandlerDoc {
    #[cfg(debug_assertions)]
    const fn new(type_name: &'static str, spec: Option<&'static HandlerSpec>) -> Self {
        Self { type_name, spec }
    }

    #[cfg(not(debug_assertions))]
    const fn new() -> Self {
        Self
    }
}

/// Describe the provided handler type, registering metadata during debug builds and doing nothing
/// in release builds.
#[must_use]
pub fn describe_handler<H: 'static>() -> RouteHandlerDoc {
    #[cfg(debug_assertions)]
    {
        let type_name = std::any::type_name::<H>();
        let spec = find_handler_spec(type_name);
        RouteHandlerDoc::new(type_name, spec)
    }

    #[cfg(not(debug_assertions))]
    {
        let _ = ::core::marker::PhantomData::<H>;
        RouteHandlerDoc::new()
    }
}

#[cfg(debug_assertions)]
#[derive(Debug, Clone)]
/// Route metadata stored when `OpenAPI` instrumentation is enabled.
pub struct RouteOpenApiEntry {
    /// HTTP path served by the handler.
    pub path: String,
    /// HTTP method associated with the handler.
    pub method: Method,
    /// Handler documentation collected from the distributed registry.
    pub handler: RouteHandlerDoc,
}

#[cfg(debug_assertions)]
impl RouteOpenApiEntry {
    #[must_use]
    /// Construct a new entry describing a route + handler pair.
    pub const fn new(path: String, method: Method, handler: RouteHandlerDoc) -> Self {
        Self {
            path,
            method,
            handler,
        }
    }
}

/// Minimal `OpenAPI` representation for Skyzen routers.
#[derive(Debug, Clone, Default)]
pub struct OpenApi {
    #[cfg(debug_assertions)]
    operations: Vec<OpenApiOperation>,
}

impl OpenApi {
    /// Build an [`OpenApi`] instance from the collected route metadata.
    #[cfg(debug_assertions)]
    #[must_use]
    pub(crate) fn from_entries(entries: &[RouteOpenApiEntry]) -> Self {
        let operations = entries
            .iter()
            .map(|entry| {
                let handler_type = entry.handler.type_name;
                entry.handler.spec.map_or_else(
                    || OpenApiOperation {
                        path: entry.path.clone(),
                        method: entry.method.clone(),
                        handler_type,
                        docs: None,
                        parameters: Vec::new(),
                        response: RefOr::T(Schema::default()),
                    },
                    |spec| {
                        let docs = spec.docs;
                        let parameters = spec.parameters.iter().map(|schema| schema()).collect();
                        let response = (spec.response)();
                        OpenApiOperation {
                            path: entry.path.clone(),
                            method: entry.method.clone(),
                            handler_type,
                            docs,
                            parameters,
                            response,
                        }
                    },
                )
            })
            .collect();
        Self { operations }
    }

    /// Build an empty OpenAPI definition when OpenAPI support is disabled.
    #[cfg(not(debug_assertions))]
    #[must_use]
    pub(crate) fn from_entries(_: &[()]) -> Self {
        Self
    }

    /// Inspect the registered operations. In release builds this returns an empty slice.
    #[must_use]
    pub fn operations(&self) -> &[OpenApiOperation] {
        #[cfg(debug_assertions)]
        {
            &self.operations
        }

        #[cfg(not(debug_assertions))]
        {
            &[]
        }
    }

    /// Indicates whether `OpenAPI` instrumentation is active.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        cfg!(debug_assertions)
    }
}

/// Description of a single handler operation.
#[derive(Clone)]
pub struct OpenApiOperation {
    /// Path served by the handler.
    pub path: String,
    /// HTTP method for the handler.
    pub method: Method,
    /// Handler type name.
    pub handler_type: &'static str,
    /// Documentation extracted from the handler's doc comments.
    pub docs: Option<&'static str>,
    /// Schemas describing the extractor arguments.
    pub parameters: Vec<RefOr<Schema>>,
    /// Schema describing the responder.
    pub response: RefOr<Schema>,
}

impl fmt::Debug for OpenApiOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenApiOperation")
            .field("path", &self.path)
            .field("method", &self.method)
            .field("handler_type", &self.handler_type)
            .field("docs", &self.docs)
            .field("parameters", &self.parameters.len())
            .finish()
    }
}
