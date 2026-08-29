//! OpenAPI helpers powered by `utoipa` schemas.

use core::future::{ready, Future};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::{
    fmt::{self, Debug},
    sync::Arc,
};

use crate::{
    extract::Extractor,
    responder::Responder,
    routing::{IntoRouteNode, MethodFilter, RouteNode},
    Body, Endpoint, Request, Response, Route,
};
use http_kit::{header, http_error, Method, StatusCode};
use utoipa::openapi::{
    content::Content,
    info::Info,
    path::{
        HttpMethod, Operation, OperationBuilder, Parameter, ParameterBuilder, ParameterIn,
        PathItemBuilder, Paths, PathsBuilder,
    },
    request_body::RequestBodyBuilder,
    response::{ResponseBuilder, ResponsesBuilder},
    schema::{ComponentsBuilder, ObjectBuilder, Schema, SchemaType, Type},
    Deprecated, OpenApi as UtoipaSpec, RefOr, Required,
};
use utoipa_redoc::Redoc;

/// `OpenAPI` schema reference type alias.
pub type SchemaRef = RefOr<Schema>;

#[cfg(feature = "openapi")]
pub use skyzen_core::openapi::{
    ExtractorSchema, ParameterLocation, ResponseSchema, SchemaCollector,
};

#[cfg(not(feature = "openapi"))]
/// Where an extractor reads its data from (stubbed when `openapi` is disabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterLocation {
    /// Read from the request body.
    Body,
    /// Read from the URL query string.
    Query,
    /// Read from a request header.
    Header,
    /// Read from the route's captured `{name}` path segments.
    Path,
}

#[cfg(not(feature = "openapi"))]
/// Schema information captured for an extractor argument (stubbed when `openapi` is disabled).
#[derive(Clone)]
pub struct ExtractorSchema {
    /// Where the extractor sources its data.
    pub location: ParameterLocation,
    /// Content type associated with the extractor, if any.
    pub content_type: Option<&'static str>,
    /// JSON schema describing the extractor payload.
    pub schema: Option<SchemaRef>,
}

#[cfg(not(feature = "openapi"))]
/// Schema information captured for a responder (stubbed when `openapi` is disabled).
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

#[cfg(not(feature = "openapi"))]
impl fmt::Debug for ExtractorSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtractorSchema")
            .field("location", &self.location)
            .field("content_type", &self.content_type)
            .field("has_schema", &self.schema.is_some())
            .finish()
    }
}

#[cfg(not(feature = "openapi"))]
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

#[cfg(not(feature = "openapi"))]
/// Function type that collects `OpenAPI` schemas into a definitions map.
pub type SchemaCollector = fn(&mut BTreeMap<String, SchemaRef>);

// Re-exported for macro-generated registrations without requiring downstream crates to depend on
// `linkme` directly.
//
// NOTE: this and the `HANDLER_SPECS`/`HandlerSpec` items below are deliberately *not* gated on
// the `openapi` feature: `#[skyzen::openapi]`-generated code in downstream crates references them
// under `cfg(all(debug_assertions, not(target_arch = "wasm32")))` — a condition that cannot
// depend on skyzen's features, because it is evaluated against the downstream crate.
#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
pub use linkme;

mod builtins;
pub use builtins::IgnoreOpenApi;

/// Strip the crate prefix from a module path, e.g. `my_crate::users::get` -> `users::get`.
#[must_use]
pub fn trim_crate(path: &str) -> &str {
    path.split_once("::").map_or(path, |(_, rest)| rest)
}

/// Function pointer used to lazily build an extractor schema.
pub type ExtractorSchemaFn = fn() -> Option<ExtractorSchema>;
/// Function pointer used to lazily build responder schemas.
pub type ResponderSchemaFn = fn() -> Option<Vec<ResponseSchema>>;

/// Return the schema for a `ToSchema` type.
#[must_use]
pub fn schema_of<T>() -> Option<SchemaRef>
where
    T: crate::ToSchema,
{
    Some(<T as crate::PartialSchema>::schema())
}

/// Return the extractor schema for `T` if it exposes `OpenAPI` metadata.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn extractor_schema_of<T>() -> Option<ExtractorSchema>
where
    T: Extractor,
{
    #[cfg(feature = "openapi")]
    {
        <T as Extractor>::openapi()
    }

    #[cfg(not(feature = "openapi"))]
    {
        let _ = core::marker::PhantomData::<T>;
        None
    }
}

/// Return the responder schemas for `T` if it exposes `OpenAPI` metadata.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn responder_schemas_of<T>() -> Option<Vec<ResponseSchema>>
where
    T: Responder,
{
    #[cfg(feature = "openapi")]
    {
        <T as Responder>::openapi()
    }

    #[cfg(not(feature = "openapi"))]
    {
        let _ = core::marker::PhantomData::<T>;
        None
    }
}

/// Register dependent schemas for the extractor type if `OpenAPI` metadata is available.
#[allow(clippy::missing_const_for_fn)]
pub fn register_extractor_schemas_for<T>(defs: &mut BTreeMap<String, SchemaRef>)
where
    T: Extractor,
{
    #[cfg(feature = "openapi")]
    {
        <T as Extractor>::register_openapi_schemas(defs);
    }

    #[cfg(not(feature = "openapi"))]
    {
        let _ = (core::marker::PhantomData::<T>, defs);
    }
}

/// Register dependent schemas for the responder type if `OpenAPI` metadata is available.
#[allow(clippy::missing_const_for_fn)]
pub fn register_responder_schemas_for<T>(defs: &mut BTreeMap<String, SchemaRef>)
where
    T: Responder,
{
    #[cfg(feature = "openapi")]
    {
        <T as Responder>::register_openapi_schemas(defs);
    }

    #[cfg(not(feature = "openapi"))]
    {
        let _ = (core::marker::PhantomData::<T>, defs);
    }
}

#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
/// Distributed registry containing handler specifications discovered via `#[skyzen::openapi]`.
#[linkme::distributed_slice]
#[linkme(crate = ::skyzen::openapi::linkme)]
pub static HANDLER_SPECS: [HandlerSpec] = [..];

#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
#[derive(Debug, Clone, Copy)]
/// Metadata captured for every handler annotated with `#[skyzen::openapi]`.
pub struct HandlerSpec {
    /// Fully-qualified handler name (module + function).
    pub type_name: &'static str,
    /// Default display name derived from the module path (without the crate prefix).
    pub operation_name: &'static str,
    /// Documentation collected from the handler's doc comments.
    pub docs: Option<&'static str>,
    /// Deprecation flag extracted from handler attributes.
    pub deprecated: bool,
    /// Schema generators for each extractor argument.
    pub parameters: &'static [ExtractorSchemaFn],
    /// Names of each documented extractor argument (aligned with `parameters`).
    pub parameter_names: &'static [&'static str],
    /// Schema generators for the responder type, if any.
    pub response: Option<ResponderSchemaFn>,
    /// Schema collectors for parameters and responders, including their transitive dependencies.
    pub schemas: &'static [SchemaCollector],
}

#[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
fn find_handler_spec(type_name: &str) -> Option<&'static HandlerSpec> {
    HANDLER_SPECS
        .iter()
        .find(|spec| spec.type_name == type_name)
}

#[cfg(feature = "openapi")]
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

/// Asks "does `T` describe itself?" without requiring that it does.
///
/// The answer is decided by method resolution: [`SchemaProbe::maybe_schema`] is an *inherent*
/// method that only exists when `T: ToSchema`, and inherent methods win over the trait method of
/// the same name on `&SchemaProbe<T>`, so a self-describing type takes the first and everything
/// else falls through to the second.
///
/// The choice is made where the call is written, so it only discriminates when `T` is concrete
/// there. Calling it from a function generic over `T` always yields the fallback, silently — so
/// this is reachable from exactly one place, `#[skyzen::openapi]`'s expansion, where the payload
/// type is spelled out and the answer is therefore real.
///
/// It exists for one caller: [`Path<T>`](crate::extract::Path), whose payload is legitimately
/// allowed not to describe itself. A multi-segment route is extracted as `Path<(String, u32)>`,
/// tuples have no `ToSchema`, and the route pattern already names those parameters — so the
/// payload only supplies types, and its absence costs a type rather than failing the build. Every
/// *body* payload takes the opposite route and requires the bound outright: see
/// [`Json`](crate::utils::Json), whose schema is the documented contract rather than a bonus.
///
/// There is deliberately no generic `maybe_schema_of<T>()` wrapper around this. Such a function
/// can only ever return `None`, and having one meant six extractors and responders reported no
/// schema while looking as though they reported one.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct SchemaProbe<T>(PhantomData<T>);

impl<T> SchemaProbe<T> {
    /// Build a probe for `T`.
    #[doc(hidden)]
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

/// The fallback half of [`SchemaProbe`]: any type at all, describing nothing.
#[doc(hidden)]
pub trait MaybeSchemaProbe {
    /// No schema, because `T` does not implement `ToSchema`.
    fn maybe_schema(self) -> Option<SchemaRef>;
    /// Nothing to register, for the same reason.
    fn maybe_register(self, defs: &mut BTreeMap<String, SchemaRef>);
}

impl<T> MaybeSchemaProbe for &SchemaProbe<T> {
    fn maybe_schema(self) -> Option<SchemaRef> {
        None
    }

    fn maybe_register(self, _defs: &mut BTreeMap<String, SchemaRef>) {}
}

/// The specialized half of [`SchemaProbe`], reached only when `T` really does describe itself.
impl<T> SchemaProbe<T>
where
    T: crate::PartialSchema + crate::ToSchema,
{
    /// The schema `T` declares.
    #[doc(hidden)]
    #[must_use]
    pub fn maybe_schema(&self) -> Option<SchemaRef> {
        Some(<T as crate::PartialSchema>::schema())
    }

    /// Register `T` and its dependencies into the components map.
    #[doc(hidden)]
    #[cfg(feature = "openapi")]
    pub fn maybe_register(&self, defs: &mut BTreeMap<String, SchemaRef>) {
        register_type::<T>(defs);
    }

    /// Registering is a no-op without the `openapi` feature: nothing ever reads the components
    /// map, so the probe keeps its signature and does nothing.
    ///
    /// This is a separate `const` definition rather than one body holding a `#[cfg]`ed statement
    /// so the feature-off form is genuinely const, which is what `clippy::missing_const_for_fn`
    /// asks for on the wasm builds that turn `openapi` off.
    #[doc(hidden)]
    #[cfg(not(feature = "openapi"))]
    pub const fn maybe_register(&self, defs: &mut BTreeMap<String, SchemaRef>) {
        let _ = defs;
    }
}

/// Register a schema and its dependencies when `OpenAPI` is enabled.
#[allow(clippy::missing_const_for_fn)]
pub fn register_schema_for<T>(defs: &mut BTreeMap<String, SchemaRef>)
where
    T: crate::PartialSchema + crate::ToSchema,
{
    #[cfg(feature = "openapi")]
    register_type::<T>(defs);
    let _ = defs;
}

#[cfg(feature = "openapi")]
/// Registers types and their dependencies into the `OpenAPI` components map.
pub trait RegisterSchemas {
    /// Insert the type's schema and dependent schemas into the provided map.
    fn register(defs: &mut BTreeMap<String, SchemaRef>);
}

#[cfg(feature = "openapi")]
impl<T> RegisterSchemas for T
where
    T: crate::PartialSchema + crate::ToSchema,
{
    fn register(defs: &mut BTreeMap<String, SchemaRef>) {
        register_type::<T>(defs);
    }
}

#[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
fn collect_schemas(collectors: &[SchemaCollector], defs: &mut BTreeMap<String, SchemaRef>) {
    for collector in collectors {
        collector(defs);
    }
}

/// Handler metadata attached to each endpoint.
#[derive(Clone, Copy, Debug)]
pub struct RouteHandlerDoc {
    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
    type_name: &'static str,
    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
    spec: Option<&'static HandlerSpec>,
}

impl RouteHandlerDoc {
    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
    const fn new(type_name: &'static str, spec: Option<&'static HandlerSpec>) -> Self {
        Self { type_name, spec }
    }

    #[cfg(not(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32"))))]
    const fn new() -> Self {
        Self {}
    }
}

/// Describe the provided handler type, registering metadata when `OpenAPI` support is enabled.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn describe_handler<H: 'static>() -> RouteHandlerDoc {
    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
    {
        let type_name = std::any::type_name::<H>();
        let spec = find_handler_spec(type_name);
        RouteHandlerDoc::new(type_name, spec)
    }

    #[cfg(not(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32"))))]
    {
        let _ = ::core::marker::PhantomData::<H>;
        RouteHandlerDoc::new()
    }
}

#[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
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

#[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
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
    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
    operations: Vec<OpenApiOperation>,
    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
    schemas: Vec<(String, SchemaRef)>,
}

impl Debug for OpenApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenApi")
            .field("operations", &"[..]")
            .field("schemas", &"[..]")
            .finish()
    }
}

impl OpenApi {
    /// Build an [`OpenApi`] instance from the collected route metadata.
    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
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
                        deprecated: false,
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
                            operation_id: spec.operation_name.to_owned(),
                            docs,
                            deprecated: spec.deprecated,
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

    /// Build an empty `OpenAPI` definition when `OpenAPI` support is disabled.
    #[cfg(not(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32"))))]
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn from_entries(_: &[()]) -> Self {
        Self {}
    }

    /// Inspect the registered operations.
    #[must_use]
    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
    pub fn operations(&self) -> &[OpenApiOperation] {
        &self.operations
    }

    /// Inspect the registered operations.
    #[must_use]
    #[cfg(not(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32"))))]
    pub const fn operations(&self) -> &[OpenApiOperation] {
        &[]
    }

    /// Indicates whether `OpenAPI` instrumentation is active.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        cfg!(all(
            debug_assertions,
            feature = "openapi",
            not(target_arch = "wasm32")
        ))
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
    #[must_use]
    pub fn to_utoipa_spec(&self) -> UtoipaSpec {
        UtoipaSpec::builder()
            .info(Self::default_info())
            .paths(self.build_paths())
            .components(Some(self.build_components()))
            .build()
    }

    fn default_info() -> Info {
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

    #[cfg(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32")))]
    fn build_components(&self) -> utoipa::openapi::schema::Components {
        self.schemas
            .iter()
            .cloned()
            .fold(ComponentsBuilder::new(), |builder, (name, schema)| {
                builder.schema(name, schema)
            })
            .build()
    }

    #[cfg(not(all(debug_assertions, feature = "openapi", not(target_arch = "wasm32"))))]
    #[allow(clippy::unused_self)]
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
    /// Operation identifier used in the `OpenAPI` document.
    pub operation_id: String,
    /// Documentation extracted from the handler's doc comments.
    pub docs: Option<&'static str>,
    /// Whether the handler is deprecated.
    pub deprecated: bool,
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
            .field("operation_id", &self.operation_id)
            .field("docs", &self.docs)
            .field("deprecated", &self.deprecated)
            .field("parameters", &self.parameters.len())
            .field("responses", &self.responses.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
/// Endpoint that renders the `OpenAPI` document via Redoc.
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
    pub OpenApiRedocDisabledError, StatusCode::NOT_IMPLEMENTED, "OpenAPI support is disabled for this build");

impl Endpoint for OpenApiRedocEndpoint {
    type Error = OpenApiRedocDisabledError;
    // The document is rendered at build time, so the future is ready on creation rather than an
    // `async` block with nothing to await.
    fn respond(
        &mut self,
        _request: &mut Request,
    ) -> impl Future<Output = Result<Response, Self::Error>> + Send {
        ready(self.html.as_ref().map_or_else(
            || Err(OpenApiRedocDisabledError::new()),
            |html| {
                let mut response = Response::new(Body::from(html.as_bytes().to_vec()));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("text/html; charset=utf-8"),
                );
                Ok(response)
            },
        ))
    }
}

fn redoc_route(endpoint: OpenApiRedocEndpoint, mount_path: String) -> RouteNode {
    let wildcard_suffix = "/{*path}";
    let route = Route::new((
        RouteNode::new_endpoint(
            "",
            MethodFilter::Exact(Method::GET),
            endpoint.clone(),
            None,
            Vec::new(),
        ),
        RouteNode::new_endpoint(
            wildcard_suffix,
            MethodFilter::Exact(Method::GET),
            endpoint,
            None,
            Vec::new(),
        ),
    ));

    RouteNode::new_route(mount_path, route)
}

/// Default mount path for the generated Redoc API documentation page.
pub const DEFAULT_API_DOCS_MOUNT: &str = "/api-docs";

impl IntoRouteNode for OpenApiRedocEndpoint {
    fn into_route_node(self) -> RouteNode {
        redoc_route(self, DEFAULT_API_DOCS_MOUNT.to_string())
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
        .summary(summary)
        .responses(build_responses(op));

    if op.deprecated {
        builder = builder.deprecated(Some(Deprecated::True));
    }

    let parameters = build_parameters(op);
    if !parameters.is_empty() {
        builder = builder.parameters(Some(parameters));
    }

    if let Some(body) = build_request_body(op) {
        builder = builder.request_body(Some(body));
    }

    if let Some(docs) = op.docs {
        builder = builder.description(Some(docs.to_owned()));
    }

    builder.build()
}

/// A minimal `string` schema used as a default for path/query/header parameters that don't carry
/// their own typed schema.
fn string_param_schema() -> RefOr<Schema> {
    RefOr::T(Schema::Object(
        ObjectBuilder::new()
            .schema_type(SchemaType::from(Type::String))
            .build(),
    ))
}

/// Extract the names of `{name}` / `{*wildcard}` segments from a route path.
fn path_parameter_names(path: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let raw = &after[..end];
        let name = raw.strip_prefix('*').unwrap_or(raw);
        if !name.is_empty() {
            names.push(name.to_owned());
        }
        rest = &after[end + 1..];
    }
    names
}

/// Build the `OpenAPI` `parameters` list: path parameters (from the route pattern) plus query and
/// header parameters (from the handler's extractor schemas). Body extractors are handled separately
/// by [`build_request_body`].
fn build_parameters(op: &OpenApiOperation) -> Vec<Parameter> {
    let mut parameters = Vec::new();

    // The route pattern is what names the path parameters; a `Path<T>` extractor only supplies
    // their types, so the two are merged rather than both emitted.
    let names = path_parameter_names(&op.path);
    let typed = typed_path_schemas(op, &names);
    for name in names {
        let schema = typed.get(&name).cloned();
        parameters.push(
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Path)
                .required(Required::True)
                .schema(Some(schema.unwrap_or_else(string_param_schema)))
                .build(),
        );
    }

    for named in &op.parameters {
        match named.schema.location {
            ParameterLocation::Query => append_query_parameters(&mut parameters, named),
            ParameterLocation::Header => parameters.push(
                ParameterBuilder::new()
                    .name(named.name.clone())
                    .parameter_in(ParameterIn::Header)
                    .required(Required::False)
                    .schema(Some(
                        named
                            .schema
                            .schema
                            .clone()
                            .unwrap_or_else(string_param_schema),
                    ))
                    .build(),
            ),
            // Path parameters were emitted above, merged with the route pattern's names; body
            // extractors are handled by `build_request_body`.
            ParameterLocation::Path | ParameterLocation::Body => {}
        }
    }

    parameters
}

/// The schemas a `Path<T>` extractor contributes, keyed by path parameter name.
///
/// A struct or map payload names its own fields; a tuple or a bare primitive does not, so its
/// schema is matched positionally against the route pattern — which for the single-parameter case
/// is exactly what `Path<u64>` means.
fn typed_path_schemas(op: &OpenApiOperation, names: &[String]) -> BTreeMap<String, RefOr<Schema>> {
    let mut typed = BTreeMap::new();
    for named in &op.parameters {
        if named.schema.location != ParameterLocation::Path {
            continue;
        }
        let Some(schema) = &named.schema.schema else {
            continue;
        };
        match schema {
            RefOr::T(Schema::Object(object)) if !object.properties.is_empty() => {
                for (field, field_schema) in &object.properties {
                    typed.insert(field.clone(), field_schema.clone());
                }
            }
            _ => {
                if let [only] = names {
                    typed.insert(only.clone(), schema.clone());
                }
            }
        }
    }
    typed
}

/// Append query parameters for a `Query<T>` extractor. When the schema is an inline object its
/// fields become individual query parameters (the conventional `OpenAPI` representation); otherwise
/// the whole schema is exposed under the argument name.
fn append_query_parameters(out: &mut Vec<Parameter>, named: &NamedExtractorSchema) {
    if let Some(RefOr::T(Schema::Object(object))) = &named.schema.schema {
        for (name, schema) in &object.properties {
            let required = object.required.iter().any(|field| field == name);
            out.push(
                ParameterBuilder::new()
                    .name(name.clone())
                    .parameter_in(ParameterIn::Query)
                    .required(if required {
                        Required::True
                    } else {
                        Required::False
                    })
                    .schema(Some(schema.clone()))
                    .build(),
            );
        }
    } else {
        out.push(
            ParameterBuilder::new()
                .name(named.name.clone())
                .parameter_in(ParameterIn::Query)
                .required(Required::False)
                .schema(Some(
                    named
                        .schema
                        .schema
                        .clone()
                        .unwrap_or_else(string_param_schema),
                ))
                .build(),
        );
    }
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
        // Only body-sourced extractors contribute to the request body; query/header/path
        // parameters are emitted as `parameters` by `build_parameters`.
        if param.schema.location != ParameterLocation::Body {
            continue;
        }

        let Some(content_type) = param.schema.content_type else {
            continue;
        };

        let schema = param
            .schema
            .schema
            .clone()
            .unwrap_or_else(|| utoipa::openapi::schema::empty().into());
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
    let lines = docs.lines();
    let mut paragraph = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        paragraph.push(trimmed);
    }
    if paragraph.is_empty() {
        None
    } else {
        Some(paragraph.join(" "))
    }
}
