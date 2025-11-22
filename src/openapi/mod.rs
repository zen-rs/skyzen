//! OpenAPI helpers powered by `utoipa` schemas.

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
#[cfg(all(debug_assertions, feature = "openapi"))]
pub use linkme::distributed_slice;

mod builtins;
pub use builtins::IgnoreOpenApi;
mod impls;

/// Strip the crate prefix from a module path, e.g. `my_crate::users::get` -> `users::get`.
pub fn trim_crate(path: &str) -> &str {
    path.split_once("::").map_or(path, |(_, rest)| rest)
}

/// Schema information captured for an extractor argument.
#[derive(Clone)]
pub struct ExtractorSchema {
    /// Content type associated with the extractor, if any.
    pub content_type: Option<&'static str>,
    /// JSON schema describing the extractor payload.
    pub schema: Option<SchemaRef>,
}

/// Schema information captured for a responder.
#[derive(Clone)]
pub struct ResponseSchema {
    /// HTTP status code returned by the responder (or [`StatusCode::OK`] by default).
    pub status: Option<StatusCode>,
    /// Description associated with the response.
    pub description: Option<&'static str>,
    /// JSON schema describing the response payload.
    pub schema: Option<SchemaRef>,
    /// Content type returned by the responder, if known.
    pub content_type: Option<&'static str>,
}

impl fmt::Debug for ExtractorSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtractorSchema")
            .field("content_type", &self.content_type)
            .field("has_schema", &self.schema.is_some())
            .finish()
    }
}

impl fmt::Debug for ResponseSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseSchema")
            .field("status", &self.status)
            .field("description", &self.description)
            .field("content_type", &self.content_type)
            .field("has_schema", &self.schema.is_some())
            .finish()
    }
}

/// Function pointer used to lazily build an extractor schema.
pub type ExtractorSchemaFn = fn() -> Option<ExtractorSchema>;
/// Function pointer used to lazily build responder schemas.
pub type ResponderSchemaFn = fn() -> Option<Vec<ResponseSchema>>;

/// Trait implemented by extractors that can describe their OpenAPI schema.
pub trait ExtractorOpenApiSchema {
    /// Return the schema for this extractor, if applicable.
    fn extractor_schema() -> Option<ExtractorSchema>;

    /// Register dependent schemas into the OpenAPI components map.
    fn register_schemas(_defs: &mut BTreeMap<String, SchemaRef>) {}
}

/// Trait implemented by responders that can describe their OpenAPI schemas.
pub trait ResponderOpenApiSchema {
    /// Return schemas for every response variant produced by this responder.
    fn responder_schemas() -> Option<Vec<ResponseSchema>>;

    /// Register dependent schemas into the OpenAPI components map.
    fn register_schemas(_defs: &mut BTreeMap<String, SchemaRef>) {}
}

/// Return the schema for a `ToSchema` type.
pub fn schema_of<T>() -> Option<SchemaRef>
where
    T: crate::ToSchema,
{
    Some(<T as crate::PartialSchema>::schema())
}

/// Return the extractor schema for `T` if it implements [`ExtractorOpenApiSchema`].
pub fn extractor_schema_of<T>() -> Option<ExtractorSchema>
where
    T: ExtractorOpenApiSchema,
{
    <T as ExtractorOpenApiSchema>::extractor_schema()
}

/// Return the responder schemas for `T` if it implements [`ResponderOpenApiSchema`].
pub fn responder_schemas_of<T>() -> Option<Vec<ResponseSchema>>
where
    T: ResponderOpenApiSchema,
{
    <T as ResponderOpenApiSchema>::responder_schemas()
}

/// Explicitly ignore OpenAPI generation for a type by providing an empty schema.
#[macro_export]
macro_rules! ignore_openapi {
    ($ty:ty) => {
        impl ::skyzen::openapi::ExtractorOpenApiSchema for $ty {
            fn extractor_schema() -> ::core::option::Option<::skyzen::openapi::ExtractorSchema> {
                None
            }
        }

        impl ::skyzen::openapi::ResponderOpenApiSchema for $ty {
            fn responder_schemas(
            ) -> ::core::option::Option<::std::vec::Vec<::skyzen::openapi::ResponseSchema>> {
                None
            }
        }
    };
}

#[cfg(all(debug_assertions, feature = "openapi"))]
/// Function pointer used to register schemas in the OpenAPI components section.
pub type SchemaCollector = fn(&mut BTreeMap<String, SchemaRef>);

#[cfg(all(debug_assertions, feature = "openapi"))]
/// Distributed registry containing handler specifications discovered via `#[skyzen::openapi]`.
#[distributed_slice]
pub static HANDLER_SPECS: [HandlerSpec] = [..];

#[cfg(all(debug_assertions, feature = "openapi"))]
#[derive(Debug, Clone, Copy)]
/// Metadata captured for every handler annotated with `#[skyzen::openapi]`.
pub struct HandlerSpec {
    /// Fully-qualified handler name (module + function).
    pub type_name: &'static str,
    /// Default display name derived from the module path (without the crate prefix).
    pub operation_name: &'static str,
    /// User-provided operation name override, if any.
    pub display_name: Option<&'static str>,
    /// Documentation collected from the handler's doc comments.
    pub docs: Option<&'static str>,
    /// Schema generators for each extractor argument.
    pub parameters: &'static [ExtractorSchemaFn],
    /// Names of each documented extractor argument (aligned with `parameters`).
    pub parameter_names: &'static [&'static str],
    /// Schema generators for the responder type, if any.
    pub response: Option<ResponderSchemaFn>,
    /// Schema collectors for parameters and responders, including their transitive dependencies.
    pub schemas: &'static [SchemaCollector],
}

#[cfg(all(debug_assertions, feature = "openapi"))]
fn find_handler_spec(type_name: &str) -> Option<&'static HandlerSpec> {
    HANDLER_SPECS
        .iter()
        .find(|spec| spec.type_name == type_name)
}

#[cfg(all(debug_assertions, feature = "openapi"))]
fn register_type<T>(defs: &mut BTreeMap<String, SchemaRef>)
where
    T: crate::PartialSchema + crate::ToSchema,
{
    let name = <T as crate::ToSchema>::name().into_owned();
    defs.entry(name)
        .or_insert_with(<T as crate::PartialSchema>::schema);
    let mut nested = Vec::new();
    <T as crate::ToSchema>::schemas(&mut nested);
    for (dep_name, schema) in nested {
        defs.entry(dep_name).or_insert(schema);
    }
}

/// Register a schema and its dependencies when OpenAPI is enabled.
pub fn register_schema_for<T>(defs: &mut BTreeMap<String, SchemaRef>)
where
    T: crate::PartialSchema + crate::ToSchema,
{
    #[cfg(all(debug_assertions, feature = "openapi"))]
    register_type::<T>(defs);
    let _ = defs;
}

#[cfg(all(debug_assertions, feature = "openapi"))]
/// Registers types and their dependencies into the OpenAPI components map.
pub trait RegisterSchemas {
    /// Insert the type's schema and dependent schemas into the provided map.
    fn register(defs: &mut BTreeMap<String, SchemaRef>);
}

#[cfg(all(debug_assertions, feature = "openapi"))]
impl<T> RegisterSchemas for T
where
    T: crate::PartialSchema + crate::ToSchema,
{
    fn register(defs: &mut BTreeMap<String, SchemaRef>) {
        register_type::<T>(defs);
    }
}

#[cfg(all(debug_assertions, feature = "openapi"))]
fn collect_schemas(collectors: &[SchemaCollector], defs: &mut BTreeMap<String, SchemaRef>) {
    for collector in collectors {
        collector(defs);
    }
}

/// Handler metadata attached to each endpoint.
#[derive(Clone, Copy, Debug)]
pub struct RouteHandlerDoc {
    #[cfg(all(debug_assertions, feature = "openapi"))]
    type_name: &'static str,
    #[cfg(all(debug_assertions, feature = "openapi"))]
    spec: Option<&'static HandlerSpec>,
}

impl RouteHandlerDoc {
    #[cfg(all(debug_assertions, feature = "openapi"))]
    const fn new(type_name: &'static str, spec: Option<&'static HandlerSpec>) -> Self {
        Self { type_name, spec }
    }

    #[cfg(not(all(debug_assertions, feature = "openapi")))]
    const fn new() -> Self {
        Self
    }
}

/// Describe the provided handler type, registering metadata during debug builds and doing nothing
/// in release builds.
#[must_use]
pub fn describe_handler<H: 'static>() -> RouteHandlerDoc {
    #[cfg(all(debug_assertions, feature = "openapi"))]
    {
        let type_name = std::any::type_name::<H>();
        let spec = find_handler_spec(type_name);
        RouteHandlerDoc::new(type_name, spec)
    }

    #[cfg(not(all(debug_assertions, feature = "openapi")))]
    {
        let _ = ::core::marker::PhantomData::<H>;
        RouteHandlerDoc::new()
    }
}

#[cfg(all(debug_assertions, feature = "openapi"))]
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

#[cfg(all(debug_assertions, feature = "openapi"))]
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
    #[cfg(all(debug_assertions, feature = "openapi"))]
    operations: Vec<OpenApiOperation>,
    #[cfg(all(debug_assertions, feature = "openapi"))]
    schemas: Vec<(String, SchemaRef)>,
}

impl Debug for OpenApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenApi")
            .field("operations", &{
                #[cfg(all(debug_assertions, feature = "openapi"))]
                {
                    &self.operations
                }
                #[cfg(not(all(debug_assertions, feature = "openapi")))]
                {
                    &"[]"
                }
            })
            .finish()
    }
}

impl OpenApi {
    /// Build an [`OpenApi`] instance from the collected route metadata.
    #[cfg(all(debug_assertions, feature = "openapi"))]
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
                        operation_id: trim_crate(handler_type).to_owned(),
                        docs: None,
                        parameters: Vec::new(),
                        responses: Vec::new(),
                    },
                    |spec| {
                        collect_schemas(spec.schemas, &mut schema_defs);
                        let docs = spec.docs;
                        let mut parameters = Vec::new();
                        for (idx, schema_fn) in spec.parameters.iter().enumerate() {
                            if let Some(schema) = schema_fn() {
                                let name =
                                    spec.parameter_names.get(idx).copied().unwrap_or("param");
                                parameters.push(NamedExtractorSchema {
                                    name: name.to_string(),
                                    schema,
                                });
                            }
                        }
                        let responses = spec
                            .response
                            .and_then(|schema| schema())
                            .unwrap_or_default();
                        OpenApiOperation {
                            path: entry.path.clone(),
                            method: entry.method.clone(),
                            handler_type,
                            operation_id: spec
                                .display_name
                                .unwrap_or_else(|| trim_crate(spec.operation_name))
                                .to_owned(),
                            docs,
                            parameters,
                            responses,
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
    #[cfg(not(all(debug_assertions, feature = "openapi")))]
    #[must_use]
    pub(crate) fn from_entries(_: &[()]) -> Self {
        Self
    }

    /// Inspect the registered operations. In release builds this returns an empty slice.
    #[must_use]
    pub fn operations(&self) -> &[OpenApiOperation] {
        #[cfg(all(debug_assertions, feature = "openapi"))]
        {
            &self.operations
        }

        #[cfg(not(all(debug_assertions, feature = "openapi")))]
        {
            &[]
        }
    }

    /// Indicates whether `OpenAPI` instrumentation is active.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        cfg!(all(debug_assertions, feature = "openapi"))
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

    /// Convert collected operations to a fully hydrated [`utoipa::openapi::OpenApi`] document.
    pub fn to_utoipa_spec(&self) -> UtoipaSpec {
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

    #[cfg(all(debug_assertions, feature = "openapi"))]
    fn build_components(&self) -> utoipa::openapi::schema::Components {
        self.schemas
            .iter()
            .cloned()
            .fold(ComponentsBuilder::new(), |builder, (name, schema)| {
                builder.schema(name, schema)
            })
            .build()
    }

    #[cfg(not(all(debug_assertions, feature = "openapi")))]
    fn build_components(&self) -> utoipa::openapi::schema::Components {
        ComponentsBuilder::new().build()
    }
}

/// Description of a parameter along with its schema metadata.
#[derive(Clone, Debug)]
pub struct NamedExtractorSchema {
    /// Parameter name as captured from the handler signature.
    pub name: String,
    /// Schema metadata for the extractor.
    pub schema: ExtractorSchema,
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
    /// Operation identifier used in the OpenAPI document.
    pub operation_id: String,
    /// Documentation extracted from the handler's doc comments.
    pub docs: Option<&'static str>,
    /// Schemas describing the extractor arguments.
    pub parameters: Vec<NamedExtractorSchema>,
    /// Schemas describing all potential responses.
    pub responses: Vec<ResponseSchema>,
}

impl fmt::Debug for OpenApiOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenApiOperation")
            .field("path", &self.path)
            .field("method", &self.method)
            .field("handler_type", &self.handler_type)
            .field("docs", &self.docs)
            .field("parameters", &self.parameters.len())
            .field("responses", &self.responses.len())
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
    let summary = op
        .docs
        .and_then(doc_summary)
        .or_else(|| Some(op.operation_id.clone()));
    let mut builder = OperationBuilder::new()
        .operation_id(Some(op.operation_id.clone()))
        .summary(summary.clone())
        .responses(build_responses(op));

    if let Some(body) = build_request_body(op) {
        builder = builder.request_body(Some(body));
    }

    if let Some(docs) = op.docs {
        let doc_string = docs.to_owned();
        if Some(doc_string.as_str()) != summary.as_deref() {
            builder = builder.description(Some(doc_string));
        }
    }

    builder.build()
}

fn build_responses(op: &OpenApiOperation) -> utoipa::openapi::response::Responses {
    if op.responses.is_empty() {
        let response = ResponseBuilder::new()
            .description("Successful response")
            .build();
        return ResponsesBuilder::new()
            .response(StatusCode::OK.as_str(), response)
            .build();
    }

    let mut builder = ResponsesBuilder::new();
    for response in &op.responses {
        let status = response.status.unwrap_or(StatusCode::OK);
        let mut response_builder =
            ResponseBuilder::new().description(response.description.unwrap_or("Response"));

        if let Some(schema) = &response.schema {
            let content_type = response.content_type.unwrap_or("application/json");
            response_builder =
                response_builder.content(content_type, Content::new(Some(schema.clone())));
        }

        builder = builder.response(status.as_str(), response_builder.build());
    }

    builder.build()
}

fn build_request_body(op: &OpenApiOperation) -> Option<utoipa::openapi::request_body::RequestBody> {
    let mut by_content_type: BTreeMap<&str, Vec<(String, RefOr<Schema>)>> = BTreeMap::new();

    for param in &op.parameters {
        let content_type = param.schema.content_type;
        if content_type.is_none() && param.schema.schema.is_none() {
            continue;
        }

        let schema = param
            .schema
            .schema
            .clone()
            .unwrap_or_else(|| utoipa::openapi::schema::empty().into());
        let content_type = content_type.unwrap_or("application/json");
        by_content_type
            .entry(content_type)
            .or_default()
            .push((param.name.clone(), schema));
    }

    if by_content_type.is_empty() {
        return None;
    }

    let mut builder = RequestBodyBuilder::new()
        .description(Some("Extractor arguments"))
        .required(Some(Required::True));

    for (content_type, schemas) in by_content_type {
        let schema = aggregate_parameter_schema(&schemas);
        builder = builder.content(content_type, Content::new(Some(schema)));
    }

    Some(builder.build())
}

fn aggregate_parameter_schema(parameters: &[(String, RefOr<Schema>)]) -> RefOr<Schema> {
    if parameters.len() == 1 {
        return parameters[0].1.clone();
    }

    let object = parameters.iter().fold(
        ObjectBuilder::new().schema_type(SchemaType::from(Type::Object)),
        |builder, (name, schema)| {
            builder
                .property(name.clone(), schema.clone())
                .required(name.clone())
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
