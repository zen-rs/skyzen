//! OpenAPI helpers powered by `utoipa` schemas.

#[cfg(debug_assertions)]
use std::collections::BTreeMap;
use std::{
    fmt::{self, Debug},
    sync::Arc,
};

use crate::{
    routing::{IntoRouteNode, RouteNode},
    Body, Endpoint, Request, Response, Route,
};
use http_kit::{header, http_error, Method, StatusCode};
use utoipa::openapi::{
    content::Content,
    info::Info,
    path::{HttpMethod, Operation, OperationBuilder, PathItemBuilder, Paths, PathsBuilder},
    request_body::RequestBodyBuilder,
    response::{ResponseBuilder, ResponsesBuilder},
    schema::{ComponentsBuilder, ObjectBuilder, Schema, SchemaType, Type},
    OpenApi as UtoipaSpec, RefOr, Required,
};
use utoipa_redoc::Redoc;
/// OpenAPI schema reference type alias.
pub type SchemaRef = RefOr<Schema>;

// Re-exported for macro-generated registrations without requiring downstream crates to depend on
// `linkme` directly.
#[cfg(debug_assertions)]
pub use linkme::distributed_slice;

mod builtins;
pub use builtins::IgnoreOpenApi;

/// Return the schema for a `ToSchema` type.
pub fn schema_of<T>() -> Option<SchemaRef>
where
    T: crate::ToSchema,
{
    Some(<T as crate::PartialSchema>::schema())
}

/// Explicitly ignore OpenAPI generation for a type by providing an empty schema.
#[macro_export]
macro_rules! ignore_openapi {
    ($ty:ty) => {
        impl ::utoipa::PartialSchema for $ty {
            fn schema() -> ::utoipa::openapi::RefOr<::utoipa::openapi::schema::Schema> {
                ::utoipa::openapi::schema::empty().into()
            }
        }
        impl ::utoipa::ToSchema for $ty {}
    };
}

#[doc(hidden)]
#[cfg(debug_assertions)]
/// Function pointer used to lazily build a [`Schema`].
pub type SchemaFn = fn() -> Option<SchemaRef>;

#[cfg(debug_assertions)]
/// Function pointer used to register schemas in the OpenAPI components section.
pub type SchemaCollector = fn(&mut Vec<(String, SchemaRef)>);

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
    /// Schema generator for the responder type, if any.
    pub response: Option<SchemaFn>,
    /// Schema collectors for parameters and responders, including their transitive dependencies.
    pub schemas: &'static [SchemaCollector],
}

#[cfg(debug_assertions)]
fn find_handler_spec(type_name: &str) -> Option<&'static HandlerSpec> {
    HANDLER_SPECS
        .iter()
        .find(|spec| spec.type_name == type_name)
}

#[cfg(debug_assertions)]
fn collect_schemas(collectors: &[SchemaCollector], defs: &mut BTreeMap<String, SchemaRef>) {
    let mut buffer = Vec::new();
    for collector in collectors {
        collector(&mut buffer);
    }
    for (name, schema) in buffer {
        defs.entry(name).or_insert(schema);
    }
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
#[derive(Clone, Default)]
pub struct OpenApi {
    #[cfg(debug_assertions)]
    operations: Vec<OpenApiOperation>,
    #[cfg(debug_assertions)]
    schemas: Vec<(String, SchemaRef)>,
}

impl Debug for OpenApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenApi")
            .field("operations", &{
                #[cfg(debug_assertions)]
                {
                    &self.operations
                }
                #[cfg(not(debug_assertions))]
                {
                    &"[]"
                }
            })
            .finish()
    }
}

impl OpenApi {
    /// Build an [`OpenApi`] instance from the collected route metadata.
    #[cfg(debug_assertions)]
    #[must_use]
    pub(crate) fn from_entries(entries: &[RouteOpenApiEntry]) -> Self {
        let mut schema_defs = BTreeMap::new();
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
                        response: None,
                    },
                    |spec| {
                        collect_schemas(spec.schemas, &mut schema_defs);
                        let docs = spec.docs;
                        let parameters = spec
                            .parameters
                            .iter()
                            .filter_map(|schema| schema())
                            .collect();
                        let response = spec.response.and_then(|schema| schema());
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
        let schemas = schema_defs.into_iter().collect();
        Self {
            operations,
            schemas,
        }
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

    #[must_use]
    /// Convert the collected spec to a [`Redoc`](utoipa_redoc::Redoc) endpoint.
    pub fn redoc(&self) -> OpenApiRedocEndpoint {
        if !self.is_enabled() {
            return OpenApiRedocEndpoint::disabled();
        }

        let html = Redoc::new(self.to_utoipa_spec()).to_html();
        OpenApiRedocEndpoint::enabled(html)
    }

    /// Build a [`RouteNode`] that serves the generated `OpenAPI` document at the provided mount path.
    #[must_use]
    pub fn redoc_route(&self, mount_path: impl Into<String>) -> RouteNode {
        let endpoint = self.redoc();
        redoc_route(endpoint, mount_path.into())
    }

    fn to_utoipa_spec(&self) -> UtoipaSpec {
        UtoipaSpec::builder()
            .info(self.default_info())
            .paths(self.build_paths())
            .components(Some(self.build_components()))
            .build()
    }

    fn default_info(&self) -> Info {
        Info::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
    }

    fn build_paths(&self) -> Paths {
        self.operations()
            .iter()
            .fold(PathsBuilder::new(), |builder, op| {
                if let Some(http_method) = method_to_http_method(&op.method) {
                    let operation = build_operation(op);
                    let path_item = PathItemBuilder::new()
                        .operation(http_method, operation)
                        .build();
                    builder.path(op.path.clone(), path_item)
                } else {
                    builder
                }
            })
            .build()
    }

    #[cfg(debug_assertions)]
    fn build_components(&self) -> utoipa::openapi::schema::Components {
        self.schemas
            .iter()
            .cloned()
            .fold(ComponentsBuilder::new(), |builder, (name, schema)| {
                builder.schema(name, schema)
            })
            .build()
    }

    #[cfg(not(debug_assertions))]
    fn build_components(&self) -> utoipa::openapi::schema::Components {
        ComponentsBuilder::new().build()
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
    pub parameters: Vec<SchemaRef>,
    /// Schema describing the responder, if documented.
    pub response: Option<SchemaRef>,
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

#[derive(Clone, Debug)]
/// Endpoint that renders the OpenAPI document via Redoc.
pub struct OpenApiRedocEndpoint {
    html: Option<Arc<String>>,
}

impl OpenApiRedocEndpoint {
    fn enabled(html: String) -> Self {
        Self {
            html: Some(Arc::new(html)),
        }
    }

    const fn disabled() -> Self {
        Self { html: None }
    }
}

http_error!(
    /// Error returned when OpenAPI support is disabled.
    pub OpenApiRedocDisabledError, StatusCode::NOT_IMPLEMENTED, "OpenAPI instrumentation disabled at compile time");

impl Endpoint for OpenApiRedocEndpoint {
    type Error = OpenApiRedocDisabledError;
    async fn respond(&mut self, _request: &mut Request) -> Result<Response, Self::Error> {
        match &self.html {
            Some(html) => {
                let mut response = Response::new(Body::from(html.as_bytes().to_vec()));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("text/html; charset=utf-8"),
                );
                Ok(response)
            }
            None => Err(OpenApiRedocDisabledError::new()),
        }
    }
}

fn redoc_route(endpoint: OpenApiRedocEndpoint, mount_path: String) -> RouteNode {
    let wildcard_suffix = "/{*path}";
    let route = Route::new((
        RouteNode::new_endpoint("", Method::GET, endpoint.clone(), None),
        RouteNode::new_endpoint(wildcard_suffix, Method::GET, endpoint, None),
    ));

    RouteNode::new_route(mount_path, route)
}

impl IntoRouteNode for OpenApiRedocEndpoint {
    fn into_route_node(self) -> RouteNode {
        redoc_route(self, "/api-doc".to_string())
    }
}

fn method_to_http_method(method: &Method) -> Option<HttpMethod> {
    match method.as_str() {
        "GET" => Some(HttpMethod::Get),
        "POST" => Some(HttpMethod::Post),
        "PUT" => Some(HttpMethod::Put),
        "DELETE" => Some(HttpMethod::Delete),
        "PATCH" => Some(HttpMethod::Patch),
        "OPTIONS" => Some(HttpMethod::Options),
        "HEAD" => Some(HttpMethod::Head),
        "TRACE" => Some(HttpMethod::Trace),
        _ => None,
    }
}

fn build_operation(op: &OpenApiOperation) -> Operation {
    let mut builder = OperationBuilder::new()
        .operation_id(Some(op.handler_type.to_owned()))
        .responses(build_responses(op));

    if let Some(body) = build_request_body(op) {
        builder = builder.request_body(Some(body));
    }

    if let Some(docs) = op.docs {
        if let Some(summary) = doc_summary(docs) {
            builder = builder.summary(Some(summary));
        }
        builder = builder.description(Some(docs.to_owned()));
    }

    builder.build()
}

fn build_responses(op: &OpenApiOperation) -> utoipa::openapi::response::Responses {
    let mut builder = ResponseBuilder::new().description("Successful response");

    if let Some(schema) = &op.response {
        builder = builder.content("application/json", Content::new(Some(schema.clone())));
    }

    ResponsesBuilder::new()
        .response(StatusCode::OK.as_str(), builder.build())
        .build()
}

fn build_request_body(op: &OpenApiOperation) -> Option<utoipa::openapi::request_body::RequestBody> {
    if op.parameters.is_empty() {
        return None;
    }

    let schema = aggregate_parameter_schema(&op.parameters);
    Some(
        RequestBodyBuilder::new()
            .description(Some("Extractor arguments"))
            .required(Some(Required::True))
            .content("application/json", Content::new(Some(schema)))
            .build(),
    )
}

fn aggregate_parameter_schema(parameters: &[RefOr<Schema>]) -> RefOr<Schema> {
    if parameters.len() == 1 {
        return parameters[0].clone();
    }

    let object = parameters.iter().enumerate().fold(
        ObjectBuilder::new().schema_type(SchemaType::from(Type::Object)),
        |builder, (idx, schema)| {
            let name = format!("param{idx}");
            builder
                .property(name.clone(), schema.clone())
                .required(name)
        },
    );

    RefOr::T(Schema::from(object.build()))
}

fn doc_summary(docs: &str) -> Option<String> {
    docs.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(std::string::ToString::to_string)
}
