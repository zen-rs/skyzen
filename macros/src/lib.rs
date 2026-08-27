//! Procedural macros for the Skyzen framework.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use skyzen_manifest::{
    DatabaseEntry, DatabaseType, Manifest, NativeDatabaseBackend, NativeServiceBackend,
    ServiceEntry, ServiceType, SkyzenManifest,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
};
use syn::{
    Attribute, Data, DeriveInput, Error, Expr, ExprLit, Fields, FnArg, Item, ItemEnum, ItemFn,
    ItemStruct, Lit, LitInt, LitStr, Meta, MetaNameValue, PatType, ReturnType, Token, Type,
    Variant,
    parse::{Parse, ParseStream},
    parse_macro_input, parse_quote,
    punctuated::Punctuated,
    spanned::Spanned,
};

/// Attribute macro that boots a Skyzen Endpoint on native or wasm runtimes.
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    let options = match MainOptions::from_args(&args) {
        Ok(options) => options,
        Err(error) => return error.to_compile_error().into(),
    };

    let mut function = parse_macro_input!(item as ItemFn);
    let is_async = function.sig.asyncness.is_some();

    let original_ident = function.sig.ident.clone();
    let entry_ident = if original_ident == "main" {
        let unique = format_ident!("__skyzen_entry_main");
        function.sig.ident = unique.clone();
        unique
    } else {
        original_ident
    };

    let entry_call = if is_async {
        quote! { #entry_ident().await }
    } else {
        quote! { #entry_ident() }
    };
    let portable_injection_wrap_steps = match portable_injection_wrap_steps() {
        Ok(steps) => steps,
        Err(error) => return error.to_compile_error().into(),
    };
    let factory_body = quote! {
        async move {
            let endpoint = #entry_call;
            #(#portable_injection_wrap_steps)*
            endpoint
        }
    };
    let native_factory = factory_body.clone();
    // On wasm the factory receives the WinterCG env explicitly (as `__skyzen_wasm_env`), so
    // endpoint construction never depends on an ambient thread-local that concurrent
    // invocations could race.
    let wasm_factory = quote! {
        move |__skyzen_wasm_env: ::skyzen::runtime::wasm::Env| {
            let _ = &__skyzen_wasm_env;
            #factory_body
        }
    };

    let init_logging = if options.default_logger {
        quote! { ::skyzen::runtime::native::init_logging(); }
    } else {
        quote! {}
    };

    // The runtime always takes a shutdown hook; without `on_shutdown` it is a no-op, so there is
    // one launch path rather than two.
    let shutdown_hook = options
        .on_shutdown
        .as_ref()
        .map_or_else(|| quote! { || async {} }, |path| quote! { || #path() });

    let output = quote! {
        ::skyzen::import_config!();

        #function

        #[cfg(not(target_arch = "wasm32"))]
        fn main() {
            #init_logging
            let __skyzen_listen_addr =
                ::skyzen::runtime::native::apply_cli_overrides(::std::env::args());
            ::skyzen::runtime::native::launch(
                __skyzen_listen_addr,
                || #native_factory,
                #shutdown_hook,
            );
        }

        #[cfg(target_arch = "wasm32")]
        use ::skyzen::wasm_bindgen as wasm_bindgen;
        #[cfg(target_arch = "wasm32")]
        use ::skyzen::wasm_bindgen_futures as wasm_bindgen_futures;
        /// Skyzen WebAssembly request entrypoint.
        #[cfg(target_arch = "wasm32")]
        #[::skyzen::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = ::skyzen::wasm_bindgen)]
        pub async fn fetch(
            request: ::skyzen::runtime::wasm::Request,
            env: ::skyzen::runtime::wasm::Env,
            ctx: ::skyzen::runtime::wasm::ExecutionContext,
        ) -> Result<::skyzen::runtime::wasm::Response, ::skyzen::wasm_bindgen::JsValue> {
            ::skyzen::runtime::wasm::launch(#wasm_factory, request, env, ctx).await
        }
    };

    output.into()
}

/// Attribute macro that runs an async test with Skyzen's native test runtime and injected mocks.
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    if !args.is_empty() {
        return Error::new_spanned(
            quote! { #args },
            "#[skyzen::test] does not take arguments; remove them",
        )
        .to_compile_error()
        .into();
    }

    let function = parse_macro_input!(item as ItemFn);
    match expand_test(function) {
        Ok(tokens) => tokens,
        Err(error) => error.to_compile_error().into(),
    }
}

/// Import declarations from `Skyzen.toml` and generate strongly typed extractors.
///
/// This macro has no runtime side effects. It only generates types, initialization methods,
/// middleware implementations, and extractors.
///
/// # Named bindings
///
/// Every `[[service]]` and `[[database]]` entry generates a newtype around the portable wrapper,
/// named after the entry: `[[service]] name = "cache" type = "kv"` generates `pub struct
/// Cache(Kv)` with `Deref<Target = Kv>`, its own `CacheNotConfigured` error, and its own
/// `Extractor` and `Middleware`. Multiple instances of one type are therefore ordinary:
///
/// ```ignore
/// async fn handler(cache: Cache, sessions: Sessions) -> Result<&'static str> {
///     cache.put("greeting", b"hello").await?;      // Deref reaches every `Kv` method
///     sessions.put_if_absent("sid", b"{}").await?;
///     Ok("ok")
/// }
/// ```
///
/// # How a bare `Kv`, `Storage`, `Queue` or `Db` resolves
///
/// Services are injected into request extensions keyed by type, so a bare wrapper can only name
/// one instance. `#[skyzen::main]` therefore injects it **only when the manifest declares exactly
/// one service of that type** (or, for `Db`, the database marked `default = true`). With two KV
/// namespaces declared, only `Cache` and `Sessions` are injected and a handler asking for a bare
/// `Kv` gets its `KvNotConfigured` error (HTTP 500) rather than one namespace chosen arbitrarily.
/// Name the binding you mean.
#[proc_macro]
pub fn import_config(input: TokenStream) -> TokenStream {
    if !input.is_empty() {
        return Error::new(
            proc_macro2::Span::call_site(),
            "import_config!() does not take arguments",
        )
        .to_compile_error()
        .into();
    }

    match expand_import_config() {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Annotate handlers that should appear in generated `OpenAPI` documentation.
#[proc_macro_attribute]
pub fn openapi(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    if !args.is_empty() {
        return Error::new_spanned(
            quote! { #args },
            "#[skyzen::openapi] does not take arguments; remove them",
        )
        .to_compile_error()
        .into();
    }

    let item = parse_macro_input!(item as Item);
    match item {
        Item::Fn(function) => match expand_openapi_fn(function) {
            Ok(tokens) => tokens,
            Err(error) => error.to_compile_error().into(),
        },
        other => Error::new_spanned(other, "#[skyzen::openapi] may only be applied to functions")
            .to_compile_error()
            .into(),
    }
}

/// Export a Cloudflare queue consumer entrypoint on wasm targets.
#[proc_macro_attribute]
pub fn queue(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    if !args.is_empty() {
        return Error::new_spanned(
            quote! { #args },
            "#[skyzen::queue] does not take arguments; remove them",
        )
        .to_compile_error()
        .into();
    }

    let function = parse_macro_input!(item as ItemFn);
    expand_cloudflare_event(function, CloudflareEventKind::Queue)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Export a Cloudflare scheduled entrypoint on wasm targets.
#[proc_macro_attribute]
pub fn scheduled(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    if !args.is_empty() {
        return Error::new_spanned(
            quote! { #args },
            "#[skyzen::scheduled] does not take arguments; remove them",
        )
        .to_compile_error()
        .into();
    }

    let function = parse_macro_input!(item as ItemFn);
    expand_cloudflare_event(function, CloudflareEventKind::Scheduled)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Export a Cloudflare Email Worker entrypoint on wasm targets.
#[proc_macro_attribute]
pub fn email(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    if !args.is_empty() {
        return Error::new_spanned(
            quote! { #args },
            "#[skyzen::email] does not take arguments; remove them",
        )
        .to_compile_error()
        .into();
    }

    let function = parse_macro_input!(item as ItemFn);
    expand_cloudflare_event(function, CloudflareEventKind::Email)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Export a Cloudflare Tail Worker entrypoint on wasm targets.
#[proc_macro_attribute]
pub fn tail(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    if !args.is_empty() {
        return Error::new_spanned(
            quote! { #args },
            "#[skyzen::tail] does not take arguments; remove them",
        )
        .to_compile_error()
        .into();
    }

    let function = parse_macro_input!(item as ItemFn);
    expand_cloudflare_event(function, CloudflareEventKind::Tail)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Export a Cloudflare Durable Object class for a `DurableObject` impl block.
#[proc_macro_attribute]
pub fn durable_object(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    if !args.is_empty() {
        return Error::new_spanned(
            quote! { #args },
            "#[skyzen::durable_object] does not take arguments; remove them",
        )
        .to_compile_error()
        .into();
    }

    let item_struct = parse_macro_input!(item as ItemStruct);
    expand_durable_object(item_struct).into()
}

/// Error helper that implements `Display`, `Error`, and `HttpError`.
///
/// On a **struct**, `message = "..."` is required and supplies the `Display` text; `status = ...`
/// sets the HTTP status (default `500`).
///
/// On an **enum**, every variant carries its own `#[error("...", status = ...)]`, and an
/// enum-level `status = ...` supplies the default those variants override. An enum-level
/// `message` is rejected, because a per-variant message is always required and an enum-level one
/// could never be used.
///
/// A single field may be marked `#[source]` to become the error's cause, or `#[from]` to be both
/// the cause and the input of a generated `From` conversion. `#[from]` requires the variant (or
/// struct) to have exactly one field.
#[proc_macro_attribute]
pub fn error(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as ErrorArgs);
    let item = parse_macro_input!(item as Item);
    match expand_error(args, item) {
        Ok(tokens) => tokens,
        Err(error) => error.to_compile_error().into(),
    }
}

/// Derive helper that maps enum variants to HTTP status codes.
#[proc_macro_derive(HttpError, attributes(status))]
pub fn derive_http_error(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    match expand_http_error(input) {
        Ok(tokens) => tokens,
        Err(error) => error.to_compile_error().into(),
    }
}

#[allow(clippy::too_many_lines)]
fn expand_openapi_fn(mut function: ItemFn) -> syn::Result<TokenStream> {
    let fn_ident = &function.sig.ident;

    let deprecated = function
        .attrs
        .iter()
        .any(|attr| attr.path().is_ident("deprecated"));

    let doc = doc_string(&function.attrs);
    let doc_tokens = doc.as_deref().map_or_else(
        || quote! { None },
        |docs| {
            let lit = Lit::Str(syn::LitStr::new(docs, fn_ident.span()));
            quote! { Some(#lit) }
        },
    );

    let mut parameter_schemas = Vec::new();
    for input in &mut function.sig.inputs {
        match input {
            FnArg::Receiver(receiver) => {
                return Err(Error::new_spanned(
                    receiver,
                    "handlers annotated with #[skyzen::openapi] cannot take self arguments",
                ));
            }
            FnArg::Typed(pat_type) => {
                parameter_schemas.push(parse_parameter_schema(pat_type)?);
            }
        }
    }

    let raw_response_ty = match &function.sig.output {
        ReturnType::Type(_, ty) => (*ty).clone(),
        ReturnType::Default => parse_quote!(()),
    };
    let response_ty = raw_response_ty;

    let mut assertions = Vec::new();
    let mut parameter_schema_fns = Vec::new();
    let mut parameter_name_lists = Vec::new();
    let mut schema_collector_idents = Vec::new();
    let mut schema_collector_defs = Vec::new();

    for (included_idx, meta) in parameter_schemas.iter().enumerate() {
        let ty = &meta.ty;
        let schema_ident = format_ident!(
            "__SKYZEN_OPENAPI_PARAM_SCHEMA_{}_{}",
            fn_ident.to_string().to_uppercase(),
            included_idx
        );
        let collector_ident = format_ident!(
            "__SKYZEN_OPENAPI_SCHEMAS_{}_{}",
            fn_ident.to_string().to_uppercase(),
            included_idx
        );

        if let Some((payload_ty, content_type)) = documented_extractor_payload(ty)? {
            let content_type_lit = LitStr::new(content_type, fn_ident.span());
            assertions.push(quote! { let _ = ::skyzen::openapi::schema_of::<#payload_ty>; });
            parameter_schema_fns.push(quote! { #schema_ident });
            schema_collector_idents.push(collector_ident.clone());
            schema_collector_defs.push(quote! {
                fn #schema_ident() -> Option<::skyzen::openapi::ExtractorSchema> {
                    let mut schema = ::skyzen::openapi::extractor_schema_of::<#ty>().unwrap_or(
                        ::skyzen::openapi::ExtractorSchema {
                            location: ::skyzen::openapi::ParameterLocation::Body,
                            content_type: Some(#content_type_lit),
                            schema: None,
                        },
                    );
                    schema.schema = ::skyzen::openapi::schema_of::<#payload_ty>();
                    Some(schema)
                }

                fn #collector_ident(
                    schemas: &mut ::std::collections::BTreeMap<String, ::skyzen::openapi::SchemaRef>
                ) {
                    ::skyzen::openapi::register_extractor_schemas_for::<#ty>(schemas);
                    ::skyzen::openapi::register_schema_for::<#payload_ty>(schemas);
                }
            });
        } else if let Some(payload_ty) = probed_extractor_payload(ty)? {
            // `Path<T>` names no parameters of its own — the route pattern does — so its payload
            // only supplies types, and a payload that does not describe itself (a tuple, say)
            // simply leaves those parameters at their untyped default rather than failing to
            // compile. Hence the probe rather than the `schema_of` assertion above.
            assertions.push(quote! { let _ = ::skyzen::openapi::extractor_schema_of::<#ty>; });
            parameter_schema_fns.push(quote! { #schema_ident });
            schema_collector_idents.push(collector_ident.clone());
            schema_collector_defs.push(quote! {
                fn #schema_ident() -> Option<::skyzen::openapi::ExtractorSchema> {
                    let mut schema = ::skyzen::openapi::extractor_schema_of::<#ty>()?;
                    schema.schema = {
                        use ::skyzen::openapi::MaybeSchemaProbe as _;
                        ::skyzen::openapi::SchemaProbe::<#payload_ty>::new().maybe_schema()
                    };
                    Some(schema)
                }

                fn #collector_ident(
                    schemas: &mut ::std::collections::BTreeMap<String, ::skyzen::openapi::SchemaRef>
                ) {
                    ::skyzen::openapi::register_extractor_schemas_for::<#ty>(schemas);
                    {
                        use ::skyzen::openapi::MaybeSchemaProbe as _;
                        ::skyzen::openapi::SchemaProbe::<#payload_ty>::new().maybe_register(schemas);
                    }
                }
            });
        } else {
            assertions.push(quote! { let _ = ::skyzen::openapi::extractor_schema_of::<#ty>; });
            parameter_schema_fns.push(quote! { ::skyzen::openapi::extractor_schema_of::<#ty> });
            schema_collector_idents.push(collector_ident.clone());
            schema_collector_defs.push(quote! {
                fn #collector_ident(
                    schemas: &mut ::std::collections::BTreeMap<String, ::skyzen::openapi::SchemaRef>
                ) {
                    ::skyzen::openapi::register_extractor_schemas_for::<#ty>(schemas);
                }
            });
        }

        let name = meta.name.as_ref().map_or_else(
            || {
                let lit = syn::LitStr::new(&format!("param{included_idx}"), fn_ident.span());
                quote! { #lit }
            },
            |ident| quote! { stringify!(#ident) },
        );
        parameter_name_lists.push(name);
    }

    let schema_array = if parameter_schema_fns.is_empty() {
        quote! { &[] }
    } else {
        quote! { &[#(#parameter_schema_fns),*] }
    };

    let response_schema_ident = format_ident!(
        "__SKYZEN_OPENAPI_RESPONSE_SCHEMA_{}",
        fn_ident.to_string().to_uppercase()
    );
    let response_collector_ident = format_ident!(
        "__SKYZEN_OPENAPI_SCHEMAS_{}_RESP",
        fn_ident.to_string().to_uppercase()
    );
    schema_collector_idents.push(response_collector_ident.clone());

    let response_schema_fn =
        if let Some((payload_ty, content_type)) = documented_response_payload(&response_ty)? {
            let content_type_lit = LitStr::new(content_type, fn_ident.span());
            assertions.push(quote! { let _ = ::skyzen::openapi::schema_of::<#payload_ty>; });
            schema_collector_defs.push(quote! {
            fn #response_schema_ident() -> Option<Vec<::skyzen::openapi::ResponseSchema>> {
                let mut responses = ::skyzen::openapi::responder_schemas_of::<#response_ty>()
                    .unwrap_or_default();
                let mut documented = false;

                for response in &mut responses {
                    if response.status.is_none() {
                        response.schema = ::skyzen::openapi::schema_of::<#payload_ty>();
                        response.content_type = response.content_type.or(Some(#content_type_lit));
                        documented = true;
                    }
                }

                if !documented {
                    responses.push(::skyzen::openapi::ResponseSchema {
                        status: None,
                        description: None,
                        schema: ::skyzen::openapi::schema_of::<#payload_ty>(),
                        content_type: Some(#content_type_lit),
                    });
                }

                Some(responses)
            }

            fn #response_collector_ident(
                schemas: &mut ::std::collections::BTreeMap<String, ::skyzen::openapi::SchemaRef>
            ) {
                ::skyzen::openapi::register_responder_schemas_for::<#response_ty>(schemas);
                ::skyzen::openapi::register_schema_for::<#payload_ty>(schemas);
            }
        });
            quote! { Some(#response_schema_ident) }
        } else {
            let response_assert =
                quote! { let _ = ::skyzen::openapi::responder_schemas_of::<#response_ty>; };
            assertions.push(response_assert);
            schema_collector_defs.push(quote! {
                fn #response_collector_ident(
                    schemas: &mut ::std::collections::BTreeMap<String, ::skyzen::openapi::SchemaRef>
                ) {
                    ::skyzen::openapi::register_responder_schemas_for::<#response_ty>(schemas);
                }
            });
            quote! { Some(::skyzen::openapi::responder_schemas_of::<#response_ty>) }
        };

    let schema_collectors = if schema_collector_idents.is_empty() {
        quote! { &[] }
    } else {
        quote! { &[#(#schema_collector_idents),*] }
    };

    let parameter_names_array = if parameter_name_lists.is_empty() {
        quote! { &[] }
    } else {
        quote! { &[#(#parameter_name_lists),*] }
    };

    let type_name_literal = quote! { concat!(module_path!(), "::", stringify!(#fn_ident)) };
    let operation_name_literal = quote! { #type_name_literal };
    let spec_ident = format_ident!(
        "__SKYZEN_OPENAPI_SPEC_{}",
        fn_ident.to_string().to_uppercase()
    );

    // All generated items are gated on `debug_assertions` + native targets only. The condition
    // must not mention any cargo feature: these `cfg`s are evaluated against the *user's* crate
    // features, and downstream crates have no feature named `openapi`. The referenced
    // `::skyzen::openapi` symbols exist whenever skyzen itself is compiled for a native debug
    // build, independent of skyzen's `openapi` feature.
    Ok(quote! {
        #function

        #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
        const _: fn() = || {
            #(#assertions)*
        };

        #(
            #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
            #schema_collector_defs
        )*

        #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
        #[::skyzen::openapi::linkme::distributed_slice(::skyzen::openapi::HANDLER_SPECS)]
        #[linkme(crate = ::skyzen::openapi::linkme)]
        static #spec_ident: ::skyzen::openapi::HandlerSpec = ::skyzen::openapi::HandlerSpec {
            type_name: #type_name_literal,
            operation_name: #operation_name_literal,
            docs: #doc_tokens,
            deprecated: #deprecated,
            parameters: #schema_array,
            parameter_names: #parameter_names_array,
            response: #response_schema_fn,
            schemas: #schema_collectors,
        };
    }
    .into())
}

struct ParameterMeta {
    ty: Type,
    name: Option<syn::Ident>,
}

#[derive(Debug, Clone)]
enum TestParamKind {
    TestContext,
    Service(TestService),
    Db,
    NamedDatabase {
        type_ident: proc_macro2::Ident,
        database_index: usize,
    },
}

/// A portable service a `#[skyzen::test]` function can ask for by parameter type.
///
/// Each variant knows the three things the expansion needs — the binding its mock lives in, how to
/// build that mock, and the `TestContext` builder that forwards it into requests — so adding a
/// service is one variant plus one match arm each, not a bool and three parallel `if` blocks.
///
/// `Ord` fixes the order the mocks are constructed in, keeping the expansion deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TestService {
    Kv,
    Storage,
    Queue,
    DurableKv,
    DurableDb,
    Alarm,
}

impl TestService {
    /// Every service, for the case where a test asks for a `TestContext` and could reach any.
    const ALL: [Self; 6] = [
        Self::Kv,
        Self::Storage,
        Self::Queue,
        Self::DurableKv,
        Self::DurableDb,
        Self::Alarm,
    ];

    /// The snake-case name shared by this service's binding and its `TestContext` builder.
    const fn slug(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Storage => "storage",
            Self::Queue => "queue",
            Self::DurableKv => "durable_kv",
            Self::DurableDb => "durable_db",
            Self::Alarm => "alarm",
        }
    }

    /// The local the constructed mock is bound to inside the generated test body.
    fn binding_ident(self) -> proc_macro2::Ident {
        format_ident!("__skyzen_test_{}", self.slug())
    }

    /// The [`TestContext`](skyzen_test::TestContext) builder that forwards this service.
    fn context_builder(self) -> proc_macro2::Ident {
        format_ident!("with_{}", self.slug())
    }

    /// The expression that builds this service's wrapper around its in-memory mock.
    fn construction(self) -> proc_macro2::TokenStream {
        match self {
            Self::Kv => quote! {
                ::skyzen_services::Kv::new(::skyzen_test::mock::InMemoryKv::new())
            },
            Self::Storage => quote! {
                ::skyzen_services::Storage::new(::skyzen_test::mock::InMemoryStorage::new())
            },
            Self::Queue => quote! {
                ::skyzen_services::Queue::new(::skyzen_test::mock::InMemoryQueue::new())
            },
            Self::DurableKv => quote! {
                ::skyzen_services::durable::DurableKv::new(
                    ::skyzen_test::mock::InMemoryDurableKv::new(),
                )
            },
            Self::DurableDb => quote! {
                ::skyzen_services::durable::DurableDb::new(
                    ::skyzen_test::mock::InMemoryDurableDb::new(),
                )
            },
            Self::Alarm => quote! {
                ::skyzen_services::durable::Alarm::new(::skyzen_test::mock::InMemoryAlarm::new())
            },
        }
    }
}

#[derive(Default)]
struct TestRequirements {
    test_context: bool,
    services: BTreeSet<TestService>,
    databases: TestDatabaseRequirements,
}

#[derive(Default)]
struct TestDatabaseRequirements {
    default_db: bool,
    named_db_indices: Vec<usize>,
}

struct TestParamBindings {
    requirements: TestRequirements,
    statements: Vec<proc_macro2::TokenStream>,
}

fn expand_test(mut function: ItemFn) -> syn::Result<TokenStream> {
    if function.sig.asyncness.is_none() {
        return Err(Error::new_spanned(
            function.sig.fn_token,
            "#[skyzen::test] requires an async function",
        ));
    }

    if !function.sig.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &function.sig.generics,
            "#[skyzen::test] does not support generic test functions",
        ));
    }

    let outer_attrs = function.attrs.clone();
    function.attrs.clear();

    let original_ident = function.sig.ident.clone();
    let inner_ident = format_ident!("__skyzen_test_body_{}", original_ident);
    function.sig.ident = inner_ident.clone();

    let inputs = std::mem::take(&mut function.sig.inputs);
    let output = function.sig.output.clone();
    let vis = function.vis.clone();

    let databases = load_manifest()?
        .map(|manifest| manifest.database)
        .unwrap_or_default();
    let database_types = test_database_types(&databases)?;
    let bindings = collect_test_param_bindings(inputs, &database_types)?;
    let setup_statements = test_setup_statements(&bindings.requirements, &databases)?;

    let mut inner_statements = setup_statements;
    inner_statements.extend(bindings.statements);
    let original_body = function.block.stmts.clone();
    function.block.stmts = inner_statements
        .into_iter()
        .map(syn::parse2)
        .collect::<syn::Result<Vec<_>>>()?;
    function.block.stmts.extend(original_body);

    let manifest_tracking = manifest_tracking_tokens();

    Ok(quote! {
        #manifest_tracking

        #function

        #(#outer_attrs)*
        #[test]
        #vis fn #original_ident() #output {
            ::skyzen::runtime::testing::block_on(async move { #inner_ident().await })
        }
    }
    .into())
}

fn test_database_types(
    databases: &[DatabaseEntry],
) -> syn::Result<Vec<(usize, proc_macro2::Ident)>> {
    databases
        .iter()
        .enumerate()
        .map(|(index, database)| {
            database_ident_from_name(&database.name).map(|ident| (index, ident))
        })
        .collect()
}

fn collect_test_param_bindings(
    inputs: Punctuated<FnArg, Token![,]>,
    database_types: &[(usize, proc_macro2::Ident)],
) -> syn::Result<TestParamBindings> {
    let mut requirements = TestRequirements::default();
    let mut statements = Vec::new();

    for input in inputs {
        let FnArg::Typed(pat_type) = input else {
            return Err(Error::new_spanned(
                input,
                "#[skyzen::test] does not support methods; use a free function",
            ));
        };

        let pat = pat_type.pat;
        let ty = pat_type.ty;
        let kind = classify_test_param(ty.as_ref(), database_types)?;
        push_test_param_binding(
            &mut requirements,
            &mut statements,
            kind,
            pat.as_ref(),
            ty.as_ref(),
        );
    }

    // A `TestContext` is what the handler under test is exercised through, and the handler may
    // extract any portable service, so asking for one provisions all of them. Each mock is a
    // couple of empty `Arc<RwLock<_>>`s, so the ones a test never touches cost nothing.
    if requirements.test_context {
        requirements.services.extend(TestService::ALL);
    }

    Ok(TestParamBindings {
        requirements,
        statements,
    })
}

fn push_test_param_binding(
    requirements: &mut TestRequirements,
    statements: &mut Vec<proc_macro2::TokenStream>,
    kind: TestParamKind,
    pat: &syn::Pat,
    ty: &Type,
) {
    match kind {
        TestParamKind::TestContext => {
            requirements.test_context = true;
            statements.push(quote! {
                let #pat: #ty = __skyzen_test_context.clone();
            });
        }
        TestParamKind::Service(service) => {
            requirements.services.insert(service);
            let binding = service.binding_ident();
            statements.push(quote! {
                let #pat: #ty = #binding.clone();
            });
        }
        TestParamKind::Db => {
            requirements.databases.default_db = true;
            statements.push(quote! {
                let #pat: #ty = __skyzen_test_default_db.clone();
            });
        }
        TestParamKind::NamedDatabase {
            type_ident,
            database_index,
        } => {
            if !requirements
                .databases
                .named_db_indices
                .contains(&database_index)
            {
                requirements.databases.named_db_indices.push(database_index);
            }
            let db_ident = format_ident!("__skyzen_test_named_db_{database_index}");
            statements.push(quote! {
                let #pat: #ty = #type_ident::new(#db_ident.clone());
            });
        }
    }
}

fn test_setup_statements(
    requirements: &TestRequirements,
    databases: &[DatabaseEntry],
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    let mut statements = Vec::new();
    push_test_service_setup(requirements, &mut statements);
    push_test_database_setup(requirements, databases, &mut statements)?;
    push_test_context_setup(requirements, &mut statements);
    Ok(statements)
}

fn push_test_service_setup(
    requirements: &TestRequirements,
    statements: &mut Vec<proc_macro2::TokenStream>,
) {
    for service in &requirements.services {
        let binding = service.binding_ident();
        let construction = service.construction();
        statements.push(quote! {
            let #binding = #construction;
        });
    }
}

fn push_test_database_setup(
    requirements: &TestRequirements,
    databases: &[DatabaseEntry],
    statements: &mut Vec<proc_macro2::TokenStream>,
) -> syn::Result<()> {
    if !requirements.databases.default_db && requirements.databases.named_db_indices.is_empty() {
        return Ok(());
    }

    let default_database = default_database_index(databases)?;
    let mut prepared_indices = requirements.databases.named_db_indices.clone();
    if let Some(default_index) = default_database {
        if requirements.databases.default_db && !prepared_indices.contains(&default_index) {
            prepared_indices.push(default_index);
        }
    } else if requirements.databases.default_db {
        prepared_indices.push(usize::MAX);
    }

    prepared_indices.sort_unstable();
    let synthesized_default_db = prepared_indices.contains(&usize::MAX);

    for index in prepared_indices {
        push_test_database_init(index, default_database, requirements, databases, statements);
    }

    if requirements.databases.default_db && default_database.is_none() && !synthesized_default_db {
        statements.push(in_memory_default_db_init());
    }

    Ok(())
}

fn push_test_database_init(
    index: usize,
    default_database: Option<usize>,
    requirements: &TestRequirements,
    databases: &[DatabaseEntry],
    statements: &mut Vec<proc_macro2::TokenStream>,
) {
    if index == usize::MAX {
        statements.push(in_memory_default_db_init());
        return;
    }

    let db_ident = format_ident!("__skyzen_test_named_db_{index}");
    let database_name = &databases[index].name;
    let init_message = LitStr::new(
        &format!("failed to initialize in-memory test database `{database_name}`"),
        proc_macro2::Span::call_site(),
    );
    statements.push(quote! {
        let #db_ident = ::skyzen_test::mock::InMemoryDb::new()
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", #init_message))
            .into_db();
    });

    if default_database == Some(index) && requirements.databases.default_db {
        statements.push(quote! {
            let __skyzen_test_default_db = #db_ident.clone();
        });
    }
}

fn in_memory_default_db_init() -> proc_macro2::TokenStream {
    quote! {
        let __skyzen_test_default_db = ::skyzen_test::mock::InMemoryDb::new()
            .await
            .unwrap_or_else(|error| panic!("failed to initialize in-memory test database: {error}"))
            .into_db();
    }
}

fn push_test_context_setup(
    requirements: &TestRequirements,
    statements: &mut Vec<proc_macro2::TokenStream>,
) {
    if !requirements.test_context {
        return;
    }

    statements.push(quote! {
        let mut __skyzen_test_context = ::skyzen_test::TestContext::new();
    });
    for service in &requirements.services {
        let binding = service.binding_ident();
        let builder = service.context_builder();
        statements.push(quote! {
            __skyzen_test_context = __skyzen_test_context.#builder(#binding.clone());
        });
    }
    if requirements.databases.default_db {
        statements.push(quote! {
            __skyzen_test_context = __skyzen_test_context.with_db(__skyzen_test_default_db.clone());
        });
    }
}

fn classify_test_param(
    ty: &Type,
    database_types: &[(usize, proc_macro2::Ident)],
) -> syn::Result<TestParamKind> {
    let Some(ident) = last_type_ident_token(ty) else {
        return Err(Error::new_spanned(
            ty,
            "unsupported #[skyzen::test] parameter type",
        ));
    };

    let ident_name = ident.to_string();
    match ident_name.as_str() {
        "TestContext" => Ok(TestParamKind::TestContext),
        "Kv" => Ok(TestParamKind::Service(TestService::Kv)),
        "Storage" => Ok(TestParamKind::Service(TestService::Storage)),
        "Queue" => Ok(TestParamKind::Service(TestService::Queue)),
        "Db" => Ok(TestParamKind::Db),
        "DurableKv" => Ok(TestParamKind::Service(TestService::DurableKv)),
        "DurableDb" => Ok(TestParamKind::Service(TestService::DurableDb)),
        "Alarm" => Ok(TestParamKind::Service(TestService::Alarm)),
        _ => database_types
            .iter()
            .find(|(_, type_ident)| *type_ident == ident)
            .map_or_else(
                || {
                    Err(Error::new_spanned(
                        ty,
                        "unsupported #[skyzen::test] parameter type; supported types are `TestContext`, `Kv`, `Storage`, `Queue`, `Db`, `DurableKv`, `DurableDb`, `Alarm`, and generated database wrappers",
                    ))
                },
                |(database_index, type_ident)| {
                    Ok(TestParamKind::NamedDatabase {
                        type_ident: type_ident.clone(),
                        database_index: *database_index,
                    })
                },
            ),
    }
}

fn last_type_ident_token(ty: &Type) -> Option<proc_macro2::Ident> {
    match ty {
        Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.clone()),
        Type::Group(group) => last_type_ident_token(&group.elem),
        Type::Paren(paren) => last_type_ident_token(&paren.elem),
        _ => None,
    }
}

fn documented_extractor_payload(ty: &Type) -> syn::Result<Option<(Type, &'static str)>> {
    let Some(ident) = last_type_ident_token(ty) else {
        return Ok(None);
    };

    match ident.to_string().as_str() {
        "Json" => Ok(Some((single_generic_type(ty)?, "application/json"))),
        "Form" | "Query" => Ok(Some((
            single_generic_type(ty)?,
            "application/x-www-form-urlencoded",
        ))),
        _ => Ok(None),
    }
}

/// The payload of an extractor whose schema is worth reporting when it exists, but whose absence
/// is not an error — today, `Path<T>`.
fn probed_extractor_payload(ty: &Type) -> syn::Result<Option<Type>> {
    let Some(ident) = last_type_ident_token(ty) else {
        return Ok(None);
    };

    match ident.to_string().as_str() {
        "Path" => Ok(Some(single_generic_type(ty)?)),
        _ => Ok(None),
    }
}

fn documented_response_payload(ty: &Type) -> syn::Result<Option<(Type, &'static str)>> {
    let Some(ident) = last_type_ident_token(ty) else {
        return Ok(None);
    };

    match ident.to_string().as_str() {
        "Json" | "PrettyJson" => Ok(Some((single_generic_type(ty)?, "application/json"))),
        "Form" => Ok(Some((
            single_generic_type(ty)?,
            "application/x-www-form-urlencoded",
        ))),
        "Result" => first_generic_type(ty)
            .map_or_else(|| Ok(None), |inner| documented_response_payload(&inner)),
        _ => Ok(None),
    }
}

fn parse_parameter_schema(pat_type: &mut PatType) -> syn::Result<ParameterMeta> {
    let mut retained = Vec::new();

    for attr in pat_type.attrs.drain(..) {
        if attr.path().is_ident("ignore") || attr.path().is_ident("proxy") {
            return Err(Error::new_spanned(
                attr,
                "#[ignore] and #[proxy] have been removed; remove this attribute",
            ));
        }

        retained.push(attr);
    }

    pat_type.attrs = retained;

    let name = match &*pat_type.pat {
        syn::Pat::Ident(ident) => Some(ident.ident.clone()),
        _ => None,
    };

    Ok(ParameterMeta {
        ty: (*pat_type.ty).clone(),
        name,
    })
}

/// Field references found in one `#[error("...")]` message, plus a copy of
/// the message with positional refs rewritten to binding identifiers
/// (`{0}` -> `{f0}`) so Rust's inline format-arg capture can resolve them.
struct MessageRefs {
    rewritten: String,
    positional: BTreeSet<usize>,
    named: BTreeSet<String>,
    saw_escape: bool,
}

/// Scan a format-style error message for `{0}` / `{field}` references,
/// honoring `{{` / `}}` escapes and ignoring format specs after `:`.
fn scan_message_refs(message: &LitStr) -> syn::Result<MessageRefs> {
    let source = message.value();
    let mut rewritten = String::with_capacity(source.len());
    let mut positional = BTreeSet::new();
    let mut named = BTreeSet::new();
    let mut saw_escape = false;

    let mut chars = source.chars().peekable();
    while let Some(current) = chars.next() {
        if current == '}' {
            if chars.peek() == Some(&'}') {
                chars.next();
                rewritten.push_str("}}");
                saw_escape = true;
            } else {
                rewritten.push('}');
            }
            continue;
        }
        if current != '{' {
            rewritten.push(current);
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            rewritten.push_str("{{");
            saw_escape = true;
            continue;
        }

        let mut argument = String::new();
        while let Some(&next) = chars.peek() {
            if next == ':' || next == '}' {
                break;
            }
            argument.push(next);
            chars.next();
        }
        if argument.is_empty() {
            return Err(Error::new(
                message.span(),
                "implicit `{}` placeholders are not supported in #[error(...)] messages; \
                 use `{0}` or `{field_name}` (escape literal braces as `{{`)",
            ));
        }
        rewritten.push('{');
        if argument.chars().all(|value| value.is_ascii_digit()) {
            let index: usize = argument.parse().map_err(|_| {
                Error::new(
                    message.span(),
                    format!("invalid positional placeholder `{{{argument}}}`"),
                )
            })?;
            positional.insert(index);
            rewritten.push('f');
        } else {
            if !is_valid_placeholder_ident(&argument) {
                return Err(Error::new(
                    message.span(),
                    format!(
                        "`{{{argument}}}` is not a valid field placeholder; \
                         escape literal braces as `{{{{`"
                    ),
                ));
            }
            named.insert(argument.clone());
        }
        rewritten.push_str(&argument);
        // The format spec (`:...`) and closing `}` flow through the loop
        // verbatim on subsequent iterations.
    }

    Ok(MessageRefs {
        rewritten,
        positional,
        named,
        saw_escape,
    })
}

fn is_valid_placeholder_ident(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// Build the match pattern and `Display` write expression for one error
/// message over one set of fields, interpolating referenced fields
/// thiserror-style.
fn display_pattern_and_expr(
    path: proc_macro2::TokenStream,
    fields: &Fields,
    message: &LitStr,
) -> syn::Result<(proc_macro2::TokenStream, proc_macro2::TokenStream)> {
    let refs = scan_message_refs(message)?;
    // With no field references the message is still a format string:
    // `{{`/`}}` escapes must collapse, which `write_str` would not do.
    let no_ref_expr = || {
        if refs.saw_escape {
            quote! { ::core::write!(f, #message) }
        } else {
            quote! { f.write_str(#message) }
        }
    };
    match fields {
        Fields::Unit => {
            if !refs.positional.is_empty() || !refs.named.is_empty() {
                return Err(Error::new(
                    message.span(),
                    "error message references fields but there are none",
                ));
            }
            let write_expr = no_ref_expr();
            Ok((path, write_expr))
        }
        Fields::Unnamed(unnamed) => {
            if !refs.named.is_empty() {
                return Err(Error::new(
                    message.span(),
                    "error message uses named placeholders but the fields are unnamed; \
                     use `{0}`, `{1}`, ...",
                ));
            }
            if let Some(&max) = refs.positional.iter().max()
                && max >= unnamed.unnamed.len()
            {
                return Err(Error::new(
                    message.span(),
                    format!(
                        "error message references field {{{max}}} but there are only {} fields",
                        unnamed.unnamed.len()
                    ),
                ));
            }
            if refs.positional.is_empty() {
                let write_expr = no_ref_expr();
                return Ok((quote! { #path ( .. ) }, write_expr));
            }
            let bindings = (0..unnamed.unnamed.len()).map(|index| {
                if refs.positional.contains(&index) {
                    let binding = format_ident!("f{index}");
                    quote! { #binding }
                } else {
                    quote! { _ }
                }
            });
            let rewritten = LitStr::new(&refs.rewritten, message.span());
            Ok((
                quote! { #path ( #(#bindings),* ) },
                quote! { ::core::write!(f, #rewritten) },
            ))
        }
        Fields::Named(named_fields) => {
            if !refs.positional.is_empty() {
                return Err(Error::new(
                    message.span(),
                    "error message uses positional placeholders but the fields are named; \
                     use `{field_name}`",
                ));
            }
            let field_names = named_fields
                .named
                .iter()
                .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
                .collect::<BTreeSet<_>>();
            for name in &refs.named {
                if !field_names.contains(name) {
                    return Err(Error::new(
                        message.span(),
                        format!("error message references unknown field `{name}`"),
                    ));
                }
            }
            if refs.named.is_empty() {
                let write_expr = no_ref_expr();
                return Ok((quote! { #path { .. } }, write_expr));
            }
            let bindings = refs
                .named
                .iter()
                .map(|name| format_ident!("{name}"))
                .collect::<Vec<_>>();
            Ok((
                quote! { #path { #(#bindings,)* .. } },
                quote! { ::core::write!(f, #message) },
            ))
        }
    }
}

fn expand_error(args: ErrorArgs, item: Item) -> syn::Result<TokenStream> {
    match item {
        Item::Struct(item_struct) => expand_error_struct(args, item_struct),
        Item::Enum(item_enum) => expand_error_enum(args, item_enum),
        other => Err(Error::new_spanned(
            other,
            "#[skyzen::error] may only be applied to structs or enums",
        )),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn expand_error_struct(args: ErrorArgs, mut item_struct: ItemStruct) -> syn::Result<TokenStream> {
    let generics = item_struct.generics.clone();
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let message = args.message.ok_or_else(|| {
        Error::new(
            proc_macro2::Span::call_site(),
            "missing `message = \"...\"` for struct error",
        )
    })?;

    let status = args
        .status
        .unwrap_or_else(|| parse_quote!(::skyzen::StatusCode::INTERNAL_SERVER_ERROR));

    let field_source = extract_field_source(&mut item_struct.fields)?;
    let ident = &item_struct.ident;

    let (pattern, write_expr) =
        display_pattern_and_expr(quote! { Self }, &item_struct.fields, &message)?;

    let self_path = quote! { Self };
    let mut source_arms = Vec::new();
    let mut from_conversion = quote! {};

    if let Some(field_source) = &field_source {
        let binding = source_binding();
        let source_pattern = field_source.binding.pattern(&self_path);
        source_arms.push(quote! {
            #source_pattern => ::core::option::Option::Some(#binding)
        });

        if field_source.from {
            from_conversion = from_impl(
                &impl_generics,
                ident,
                &ty_generics,
                where_clause,
                &self_path,
                field_source,
            );
        }
    }

    let error_impl = error_impl(
        &impl_generics,
        ident,
        &ty_generics,
        where_clause,
        &source_arms,
        true,
    );

    Ok(quote! {
        #[derive(::core::fmt::Debug)]
        #item_struct

        impl #impl_generics ::core::fmt::Display for #ident #ty_generics #where_clause {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    #pattern => #write_expr,
                }
            }
        }

        #error_impl

        impl #impl_generics ::skyzen::HttpError for #ident #ty_generics #where_clause {
            fn status(&self) -> ::skyzen::StatusCode {
                #status
            }
        }

        #from_conversion
    }
    .into())
}

fn expand_error_enum(args: ErrorArgs, mut item_enum: ItemEnum) -> syn::Result<TokenStream> {
    let ident = &item_enum.ident;
    let generics = &item_enum.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let ErrorArgs { status, message } = args;
    if let Some(message) = message {
        return Err(Error::new(
            message.span(),
            "`message = \"...\"` has no effect on an enum; give each variant its own \
             #[error(\"...\")] message instead",
        ));
    }

    let default_status =
        status.unwrap_or_else(|| parse_quote!(::skyzen::StatusCode::INTERNAL_SERVER_ERROR));

    let mut display_arms = Vec::new();
    let mut status_arms = Vec::new();
    let mut source_arms = Vec::new();
    let mut from_impls = Vec::new();
    let mut cleaned_variants = Punctuated::new();
    let mut variant_count = 0usize;

    for variant in item_enum.variants {
        variant_count += 1;
        let (
            variant,
            VariantMeta {
                message,
                status,
                source,
            },
        ) = parse_variant(variant)?;

        let variant_path = {
            let ident = &variant.ident;
            quote! { Self::#ident }
        };
        let wildcard_pattern = match &variant.fields {
            Fields::Unit => quote! { #variant_path },
            Fields::Unnamed(_) => quote! { #variant_path ( .. ) },
            Fields::Named(_) => quote! { #variant_path { .. } },
        };

        let status_expr = status.unwrap_or_else(|| default_status.clone());

        let (display_pattern, write_expr) =
            display_pattern_and_expr(variant_path.clone(), &variant.fields, &message)?;
        display_arms.push(quote! {
            #display_pattern => #write_expr
        });

        status_arms.push(quote! {
            #wildcard_pattern => #status_expr
        });

        if let Some(field_source) = source {
            let binding = source_binding();
            let source_pattern = field_source.binding.pattern(&variant_path);
            source_arms.push(quote! {
                #source_pattern => ::core::option::Option::Some(#binding)
            });

            if field_source.from {
                from_impls.push(from_impl(
                    &impl_generics,
                    ident,
                    &ty_generics,
                    where_clause,
                    &variant_path,
                    &field_source,
                ));
            }
        }

        cleaned_variants.push(variant);
    }

    item_enum.variants = cleaned_variants;

    let error_impl = error_impl(
        &impl_generics,
        ident,
        &ty_generics,
        where_clause,
        &source_arms,
        source_arms.len() == variant_count,
    );

    Ok(quote! {
        #[derive(::core::fmt::Debug)]
        #item_enum

        impl #impl_generics ::core::fmt::Display for #ident #ty_generics #where_clause {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    #(#display_arms),*
                }
            }
        }

        #error_impl

        impl #impl_generics ::skyzen::HttpError for #ident #ty_generics #where_clause {
            fn status(&self) -> ::skyzen::StatusCode {
                match self {
                    #(#status_arms),*
                }
            }
        }

        #(#from_impls)*
    }
    .into())
}

fn expand_http_error(input: DeriveInput) -> syn::Result<TokenStream> {
    let ident = input.ident;
    let generics = input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let variants = match input.data {
        Data::Enum(data) => data.variants,
        _ => {
            return Err(Error::new(
                ident.span(),
                "HttpError can only be derived for enums",
            ));
        }
    };

    let mut arms = Vec::new();
    for variant in variants {
        let variant_ident = &variant.ident;
        let pattern = match &variant.fields {
            Fields::Unit => quote! { Self::#variant_ident },
            Fields::Unnamed(_) => quote! { Self::#variant_ident ( .. ) },
            Fields::Named(_) => quote! { Self::#variant_ident { .. } },
        };
        let status_expr = variant_status_expr(&variant)?;
        arms.push(quote! { #pattern => #status_expr });
    }

    Ok(quote! {
        impl #impl_generics ::skyzen::HttpError for #ident #ty_generics #where_clause {
            fn status(&self) -> ::skyzen::StatusCode {
                match self {
                    #(#arms),*
                }
            }
        }
    }
    .into())
}

fn variant_status_expr(variant: &Variant) -> syn::Result<Expr> {
    let mut expr = None;
    for attr in &variant.attrs {
        if attr.path().is_ident("status") {
            if expr.is_some() {
                return Err(Error::new(attr.span(), "duplicate `status` attribute"));
            }

            let value = match &attr.meta {
                Meta::NameValue(meta) => meta.value.clone(),
                _ => return Err(Error::new_spanned(attr, "expected #[status = <expr>]")),
            };
            expr = Some(normalize_status_expr(&value)?);
        }
    }

    Ok(expr.unwrap_or_else(|| parse_quote!(::skyzen::StatusCode::INTERNAL_SERVER_ERROR)))
}

fn normalize_status_expr(expr: &Expr) -> syn::Result<Expr> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Int(lit), ..
        }) => normalize_status_lit(lit),
        Expr::Path(path) if path.path.segments.len() == 1 => {
            let ident = &path.path.segments[0].ident;
            Ok(parse_quote!(::skyzen::StatusCode::#ident))
        }
        _ => Ok(expr.clone()),
    }
}

fn normalize_status_lit(lit: &LitInt) -> syn::Result<Expr> {
    let value = lit
        .base10_parse::<u16>()
        .map_err(|_| Error::new(lit.span(), "status code literal must fit within u16"))?;
    validate_status_code(value, lit.span())?;
    Ok(parse_quote! {
        ::skyzen::StatusCode::from_u16(#value)
            .expect("invalid HTTP status code literal")
    })
}

/// Reject out-of-range HTTP status literals at expansion time instead of letting
/// `StatusCode::from_u16` panic at runtime on every request.
fn validate_status_code(value: u16, span: proc_macro2::Span) -> syn::Result<()> {
    if (100..=999).contains(&value) {
        Ok(())
    } else {
        Err(Error::new(
            span,
            format!("invalid HTTP status code `{value}`: must be in the range 100..=999"),
        ))
    }
}

#[derive(Clone, Default)]
struct ErrorArgs {
    status: Option<Expr>,
    message: Option<LitStr>,
}

impl Parse for ErrorArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut args = Self::default();
        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "status" => {
                    if args.status.is_some() {
                        return Err(Error::new(key.span(), "duplicate `status` argument"));
                    }
                    let value: Expr = input.parse()?;
                    args.status = Some(normalize_status_expr(&value)?);
                }
                "message" => {
                    if args.message.is_some() {
                        return Err(Error::new(key.span(), "duplicate `message` argument"));
                    }
                    args.message = Some(input.parse()?);
                }
                other => {
                    return Err(Error::new(
                        key.span(),
                        format!("unsupported #[skyzen::error] argument `{other}`"),
                    ));
                }
            }

            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        Ok(args)
    }
}

struct VariantMeta {
    message: LitStr,
    status: Option<Expr>,
    source: Option<FieldSource>,
}

/// The field a variant (or struct) exposes as its `Error::source`.
struct FieldSource {
    ty: Type,
    binding: FieldBinding,
    /// Whether the field was marked `#[from]`, which additionally generates a `From` impl.
    from: bool,
}

/// How to bind the source field inside a match pattern.
enum FieldBinding {
    /// A tuple field at `index`, out of `len` fields in total.
    Positional { index: usize, len: usize },
    /// A named field.
    Named(syn::Ident),
}

/// The identifier the generated `source()` binds the source field to.
fn source_binding() -> syn::Ident {
    format_ident!("__skyzen_source")
}

impl FieldBinding {
    /// Build the constructor expression a `From` impl uses to build this variant.
    fn construct(
        &self,
        path: &proc_macro2::TokenStream,
        value: &syn::Ident,
    ) -> proc_macro2::TokenStream {
        match self {
            Self::Positional { .. } => quote! { #path(#value) },
            Self::Named(ident) => quote! { #path { #ident: #value } },
        }
    }

    /// Build the match pattern that binds this field for the generated `source()`.
    fn pattern(&self, path: &proc_macro2::TokenStream) -> proc_macro2::TokenStream {
        let binding = source_binding();
        match self {
            Self::Positional { index, len } => {
                let fields = (0..*len).map(|position| {
                    if position == *index {
                        quote! { #binding }
                    } else {
                        quote! { _ }
                    }
                });
                quote! { #path(#(#fields),*) }
            }
            Self::Named(ident) => quote! { #path { #ident: #binding, .. } },
        }
    }
}

/// Build the `From<Source>` impl that constructs `path` from a `#[from]`-marked field.
fn from_impl(
    impl_generics: &syn::ImplGenerics<'_>,
    ident: &syn::Ident,
    ty_generics: &syn::TypeGenerics<'_>,
    where_clause: Option<&syn::WhereClause>,
    path: &proc_macro2::TokenStream,
    field_source: &FieldSource,
) -> proc_macro2::TokenStream {
    let value = format_ident!("__skyzen_from");
    let ctor = field_source.binding.construct(path, &value);
    let ty = &field_source.ty;
    quote! {
        impl #impl_generics ::core::convert::From<#ty> for #ident #ty_generics #where_clause {
            fn from(#value: #ty) -> Self {
                #ctor
            }
        }
    }
}

/// Assemble the `core::error::Error` impl, emitting `source()` only when some arm produces one.
fn error_impl(
    impl_generics: &syn::ImplGenerics<'_>,
    ident: &syn::Ident,
    ty_generics: &syn::TypeGenerics<'_>,
    where_clause: Option<&syn::WhereClause>,
    source_arms: &[proc_macro2::TokenStream],
    exhaustive: bool,
) -> proc_macro2::TokenStream {
    if source_arms.is_empty() {
        return quote! {
            impl #impl_generics ::core::error::Error for #ident #ty_generics #where_clause {}
        };
    }

    let fallback = if exhaustive {
        quote! {}
    } else {
        quote! { _ => ::core::option::Option::None, }
    };

    quote! {
        impl #impl_generics ::core::error::Error for #ident #ty_generics #where_clause {
            fn source(&self) -> ::core::option::Option<&(dyn ::core::error::Error + 'static)> {
                match self {
                    #(#source_arms,)*
                    #fallback
                }
            }
        }
    }
}

fn parse_variant(mut variant: Variant) -> syn::Result<(Variant, VariantMeta)> {
    let mut other_attrs = Vec::new();
    let mut meta = None;

    for attr in variant.attrs {
        if attr.path().is_ident("error") {
            if meta.is_some() {
                return Err(Error::new(attr.span(), "duplicate #[error] attribute"));
            }
            meta = Some(parse_variant_error_attr(&attr)?);
        } else {
            other_attrs.push(attr);
        }
    }

    let mut meta = meta.ok_or_else(|| {
        Error::new(
            variant.ident.span(),
            "each variant must include #[error(\"...\")]",
        )
    })?;
    meta.source = extract_field_source(&mut variant.fields)?;

    variant.attrs = other_attrs;
    Ok((variant, meta))
}

fn parse_variant_error_attr(attr: &Attribute) -> syn::Result<VariantMeta> {
    attr.parse_args_with(|input: ParseStream<'_>| {
        let mut message: Option<LitStr> = None;
        let mut status = None;

        while !input.is_empty() {
            if input.peek(Lit) {
                if message.is_some() {
                    return Err(Error::new(input.span(), "duplicate error message"));
                }
                let lit: Lit = input.parse()?;
                match lit {
                    Lit::Str(str_lit) => {
                        message = Some(str_lit);
                    }
                    other => {
                        return Err(Error::new(
                            other.span(),
                            "expected string literal for #[error(...)] message",
                        ));
                    }
                }
            } else {
                let key: syn::Ident = input.parse()?;
                input.parse::<Token![=]>()?;
                match key.to_string().as_str() {
                    "status" => {
                        if status.is_some() {
                            return Err(Error::new(key.span(), "duplicate `status` argument"));
                        }
                        let value: Expr = input.parse()?;
                        status = Some(normalize_status_expr(&value)?);
                    }
                    other => {
                        return Err(Error::new(
                            key.span(),
                            format!("unsupported #[error] argument `{other}`"),
                        ));
                    }
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            } else {
                break;
            }
        }

        let message = message.ok_or_else(|| {
            Error::new(
                attr.span(),
                "missing string literal message in #[error(...)]",
            )
        })?;

        if !input.is_empty() {
            return Err(Error::new(
                input.span(),
                "unexpected tokens in #[error] attribute",
            ));
        }

        Ok(VariantMeta {
            message,
            status,
            source: None,
        })
    })
}

/// Strip the `#[from]`/`#[source]` marker off whichever field carries it and describe it.
///
/// `#[from]` implies `#[source]`: the wrapped value becomes both the conversion input and the
/// error's cause, matching `thiserror`.
fn extract_field_source(fields: &mut Fields) -> syn::Result<Option<FieldSource>> {
    let len = fields.len();
    let mut info: Option<FieldSource> = None;

    for (index, field) in fields.iter_mut().enumerate() {
        let marker = take_source_attrs(&mut field.attrs)?;
        let Some(marker) = marker else { continue };

        if info.is_some() {
            return Err(Error::new(
                field.ty.span(),
                "only one field may be marked #[from] or #[source]",
            ));
        }

        if marker.from && len != 1 {
            return Err(Error::new(
                field.ty.span(),
                "#[from] is only supported on variants with a single field; use #[source] instead",
            ));
        }

        let binding = field
            .ident
            .clone()
            .map_or(FieldBinding::Positional { index, len }, FieldBinding::Named);

        info = Some(FieldSource {
            ty: field.ty.clone(),
            binding,
            from: marker.from,
        });
    }

    Ok(info)
}

/// Which of `#[from]` / `#[source]` a field carried.
struct SourceMarker {
    from: bool,
}

fn take_source_attrs(attrs: &mut Vec<Attribute>) -> syn::Result<Option<SourceMarker>> {
    let mut from = false;
    let mut source = false;
    let mut retained = Vec::new();

    for attr in attrs.drain(..) {
        let is_from = attr.path().is_ident("from");
        let is_source = attr.path().is_ident("source");

        if !is_from && !is_source {
            retained.push(attr);
            continue;
        }

        let name = if is_from { "#[from]" } else { "#[source]" };
        if !matches!(attr.meta, Meta::Path(_)) {
            return Err(Error::new_spanned(
                attr,
                format!("{name} does not take arguments"),
            ));
        }
        if (is_from && from) || (is_source && source) {
            return Err(Error::new(
                attr.span(),
                format!("duplicate {name} attribute"),
            ));
        }

        if is_from {
            from = true;
        } else {
            source = true;
        }
    }

    *attrs = retained;

    if from || source {
        Ok(Some(SourceMarker { from }))
    } else {
        Ok(None)
    }
}

fn doc_string(attrs: &[Attribute]) -> Option<String> {
    let mut docs = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }

        if let Meta::NameValue(meta) = &attr.meta
            && let Expr::Lit(ExprLit {
                lit: Lit::Str(lit), ..
            }) = &meta.value
        {
            docs.push(lit.value().trim().to_owned());
        }
    }

    if docs.is_empty() {
        None
    } else {
        Some(docs.join("\n"))
    }
}

/// The portable wrapper a service type is bound to.
fn service_wrapper_path(service_type: ServiceType) -> proc_macro2::TokenStream {
    match service_type {
        ServiceType::Kv => quote! { ::skyzen_services::Kv },
        ServiceType::Storage => quote! { ::skyzen_services::Storage },
        ServiceType::Queue => quote! { ::skyzen_services::Queue },
    }
}

fn expand_import_config() -> syn::Result<proc_macro2::TokenStream> {
    let manifest = load_manifest()?.unwrap_or_default();
    let services = &manifest.service;
    let databases = &manifest.database;
    let mut generated_items = Vec::with_capacity(services.len() + databases.len());
    // Services and databases land in the same module, so one set catches a collision between
    // kinds — a service named `main-db` and a database named `main` both normalize to `MainDb`.
    let mut seen_idents = HashSet::new();

    for service in services {
        let ident = service_ident_from_name(&service.name)?;
        if !seen_idents.insert(ident.to_string()) {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!("duplicate service type name after normalization: `{ident}`"),
            ));
        }

        generated_items.push(named_binding_tokens(
            &ident,
            &service_wrapper_path(service.service_type),
            &format!(
                "{} `{}` not configured. Ensure Skyzen.toml service wiring is installed.",
                service.service_type.as_str(),
                service.name
            ),
        ));
    }

    for database in databases {
        let ident = database_ident_from_name(&database.name)?;
        if !seen_idents.insert(ident.to_string()) {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!("duplicate database type name after normalization: `{ident}`"),
            ));
        }

        generated_items.push(named_binding_tokens(
            &ident,
            &quote! { ::skyzen_services::Db },
            &format!(
                "database `{}` not configured. Ensure Skyzen.toml database wiring is installed.",
                database.name
            ),
        ));
    }

    let manifest_tracking = manifest_tracking_tokens();

    Ok(quote! {
        #manifest_tracking

        #[doc(hidden)]
        pub mod __skyzen_config {
            #(#generated_items)*
        }

        pub use __skyzen_config::*;
    })
}

/// Generate the named newtype for one `[[service]]` or `[[database]]` entry.
///
/// Injection is keyed by the extension's type, so a bare `Kv` or `Db` can only ever name one
/// instance. Every manifest entry therefore gets its own type wrapping the portable wrapper, with
/// `Deref` to it, its own `*NotConfigured` error, and its own `Extractor`/`Middleware` pair — which
/// is what lets `async fn h(cache: Cache, sessions: Sessions)` name two KV namespaces.
///
/// Services and databases produce byte-identical plumbing, so it is written once here rather than
/// twice; only the wrapped type and the not-configured message differ.
fn named_binding_tokens(
    ident: &proc_macro2::Ident,
    wrapper: &proc_macro2::TokenStream,
    missing_message: &str,
) -> proc_macro2::TokenStream {
    let missing_ident = format_ident!("{ident}NotConfigured");
    let missing_message = LitStr::new(missing_message, proc_macro2::Span::call_site());

    quote! {
        #[derive(Debug, Clone)]
        pub struct #ident(#wrapper);

        impl #ident {
            #[must_use]
            pub const fn new(service: #wrapper) -> Self {
                Self(service)
            }

            #[must_use]
            pub const fn inner(&self) -> &#wrapper {
                &self.0
            }

            #[must_use]
            pub fn into_inner(self) -> #wrapper {
                self.0
            }
        }

        impl ::std::ops::Deref for #ident {
            type Target = #wrapper;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        ::skyzen::http_kit::http_error!(
            pub #missing_ident,
            ::skyzen::StatusCode::INTERNAL_SERVER_ERROR,
            #missing_message
        );

        impl ::skyzen::extract::Extractor for #ident {
            type Error = #missing_ident;

            async fn extract(
                request: &mut ::skyzen::Request,
            ) -> ::std::result::Result<Self, Self::Error> {
                request
                    .extensions()
                    .get::<Self>()
                    .cloned()
                    .ok_or_else(#missing_ident::new)
            }
        }

        impl ::skyzen::middleware::Middleware for #ident {
            async fn handle(
                &self,
                request: &mut ::skyzen::Request,
                next: ::skyzen::middleware::Next<'_>,
            ) -> ::std::result::Result<::skyzen::Response, ::skyzen::Error> {
                request.extensions_mut().insert(self.clone());
                next.run(request).await
            }

            fn provisions(&self) -> ::std::vec::Vec<::std::any::TypeId> {
                ::std::vec![::std::any::TypeId::of::<Self>()]
            }
        }
    }
}

/// Emit a `const _: &str = include_str!(...)` referencing `Skyzen.toml` (when it exists) so
/// cargo tracks the file and rebuilds when it changes; the `fs::read_to_string` performed by
/// the proc macros is invisible to cargo's change detection.
fn manifest_tracking_tokens() -> proc_macro2::TokenStream {
    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return proc_macro2::TokenStream::new();
    };
    let config_path = PathBuf::from(manifest_dir).join("Skyzen.toml");
    if !config_path.exists() {
        return proc_macro2::TokenStream::new();
    }
    let path_lit = LitStr::new(
        &config_path.display().to_string(),
        proc_macro2::Span::call_site(),
    );
    quote! {
        const _: &str = include_str!(#path_lit);
    }
}

fn portable_injection_wrap_steps() -> syn::Result<Vec<proc_macro2::TokenStream>> {
    let Some(manifest) = load_manifest()? else {
        return Ok(Vec::new());
    };

    let services = &manifest.service;
    let databases = &manifest.database;
    if services.is_empty() && databases.is_empty() {
        return Ok(Vec::new());
    }
    let default_database = default_database_index(databases)?;

    let mut steps = Vec::with_capacity(services.len() + databases.len() + 1);
    let service_type_counts = service_type_counts(services);

    // On wasm, `__skyzen_wasm_env` is bound by the factory closure parameter generated in
    // `#[skyzen::main]`; the environment is threaded explicitly rather than read from a
    // thread-local that concurrent invocations could race.

    for service in services {
        let ident = service_ident_from_name(&service.name)?;
        let native_init = generate_native_service_init(service, &manifest)?;
        let cloudflare_init = generate_cloudflare_service_init(service, &manifest);
        // The bare `Kv`/`Storage`/`Queue` extractor names whichever service of that type is the
        // only one, so it is injected exactly when the type is unambiguous.
        let inject_bare = service_type_counts[&service.service_type] == 1;
        let native = named_injection_tokens(&ident, &native_init, inject_bare);
        let cloudflare = named_injection_tokens(&ident, &cloudflare_init, inject_bare);
        steps.push(quote! {
            let endpoint = {
                #[cfg(not(target_arch = "wasm32"))]
                { #native }
                #[cfg(target_arch = "wasm32")]
                { #cloudflare }
            };
        });
    }

    for (index, database) in databases.iter().enumerate() {
        let ident = database_ident_from_name(&database.name)?;
        let native_init = generate_native_database_init(database, &manifest);
        let cloudflare_init = generate_cloudflare_database_init(database, &manifest);
        let inject_bare = default_database == Some(index);
        let native = named_injection_tokens(&ident, &native_init, inject_bare);
        let cloudflare = named_injection_tokens(&ident, &cloudflare_init, inject_bare);
        steps.push(quote! {
            let endpoint = {
                #[cfg(not(target_arch = "wasm32"))]
                { #native }
                #[cfg(target_arch = "wasm32")]
                { #cloudflare }
            };
        });
    }

    Ok(steps)
}

/// How many `[[service]]` entries each service type has.
///
/// A type with exactly one entry is unambiguous, so the bare `Kv`/`Storage`/`Queue` extractor can
/// be injected alongside the named newtype; with two or more, only the newtypes are injected.
fn service_type_counts(services: &[ServiceEntry]) -> HashMap<ServiceType, usize> {
    let mut counts = HashMap::new();
    for service in services {
        *counts.entry(service.service_type).or_insert(0) += 1;
    }
    counts
}

/// Wrap the router with one manifest entry's middleware.
///
/// The named newtype is always installed. `inject_bare` additionally installs the portable wrapper
/// itself, which is what makes `async fn h(kv: Kv)` work when the manifest declares exactly one
/// service of that type (or the default database).
fn named_injection_tokens(
    ident: &proc_macro2::Ident,
    init: &proc_macro2::TokenStream,
    inject_bare: bool,
) -> proc_macro2::TokenStream {
    if inject_bare {
        quote! {
            let __service = #init;
            let endpoint = ::skyzen::__private::with_middleware(
                endpoint,
                #ident::new(::std::clone::Clone::clone(&__service)),
            );
            ::skyzen::__private::with_middleware(endpoint, __service)
        }
    } else {
        quote! {
            let __service = #init;
            ::skyzen::__private::with_middleware(endpoint, #ident::new(__service))
        }
    }
}

/// Read the project's `Skyzen.toml` through the shared schema.
///
/// The file is optional — an application can wire every service by hand — so a missing file is
/// `Ok(None)` rather than an error. Everything else (unreadable, malformed, unknown key,
/// unsupported `type`) is a compile error, reported at the macro's call site.
fn load_manifest() -> syn::Result<Option<SkyzenManifest>> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|error| {
        Error::new(
            proc_macro2::Span::call_site(),
            format!("failed to read CARGO_MANIFEST_DIR: {error}"),
        )
    })?;
    let config_path = PathBuf::from(manifest_dir).join("Skyzen.toml");
    if !config_path.exists() {
        return Ok(None);
    }

    Manifest::load(&config_path)
        .map(|manifest| Some(manifest.data().clone()))
        .map_err(|error| Error::new(proc_macro2::Span::call_site(), error.to_string()))
}

/// The `[native.service.<name>]` wiring for one portable service.
fn native_service_wiring<'a>(
    manifest: &'a SkyzenManifest,
    name: &str,
) -> Option<&'a skyzen_manifest::NativeServiceSection> {
    manifest.native.as_ref()?.service.get(name)
}

/// The `[native.database.<name>]` wiring for one portable database.
fn native_database_wiring<'a>(
    manifest: &'a SkyzenManifest,
    name: &str,
) -> Option<&'a skyzen_manifest::NativeDatabaseSection> {
    manifest.native.as_ref()?.database.get(name)
}

/// A required `url_env`/`bucket_env` key that the wiring section left out.
fn missing_wiring_key(section: &str, key: &str) -> Error {
    Error::new(
        proc_macro2::Span::call_site(),
        format!("{section} is missing `{key}`"),
    )
}

fn generate_native_service_init(
    service: &ServiceEntry,
    manifest: &SkyzenManifest,
) -> syn::Result<proc_macro2::TokenStream> {
    let Some(wiring) = native_service_wiring(manifest, &service.name) else {
        return Ok(compile_error_block(&format!(
            "missing [native.service.{}] wiring for portable service `{}`",
            service.name, service.name
        )));
    };

    let label = format!("[native.service.{}]", service.name);
    let wrapper = service_wrapper_path(service.service_type);

    // Every env-configured backend has the same shape — read one variable, hand its value to a
    // constructor, wrap the result — so the lookup, the missing-variable panic and the wrapper
    // construction are written once and each arm supplies only the constructor call.
    let env_key = |key: &'static str, value: Option<&String>| {
        value
            .cloned()
            .ok_or_else(|| missing_wiring_key(&label, key))
    };

    let (env_key, build) = match (service.service_type, wiring.backend) {
        (ServiceType::Kv, NativeServiceBackend::Redis) => {
            let key = env_key("url_env", wiring.url_env.as_ref())?;
            let failure = connect_failure_lit(&service.name, &format!("connect to Redis using `{key}`"));
            (
                key,
                quote! {
                    ::skyzen_redis::Redis::connect(&__skyzen_env_value)
                        .await
                        .unwrap_or_else(|error| panic!("{}: {error}", #failure))
                },
            )
        }
        (ServiceType::Storage, NativeServiceBackend::S3) => (
            env_key("bucket_env", wiring.bucket_env.as_ref())?,
            quote! { ::skyzen_s3::S3Storage::from_env(&__skyzen_env_value).await },
        ),
        (ServiceType::Queue, NativeServiceBackend::Sqs) => {
            let key = env_key("url_env", wiring.url_env.as_ref())?;
            // `SqsQueue::from_env` builds a *standard* queue and refuses a `.fifo` URL, which
            // needs a message group id on every send. A FIFO queue is wired in code with
            // `SqsQueue::fifo`, so the manifest path reports the mismatch rather than hiding it.
            let failure =
                connect_failure_lit(&service.name, &format!("use the SQS queue named by `{key}`"));
            (
                key,
                quote! {
                    ::skyzen_aws::SqsQueue::from_env(&__skyzen_env_value)
                        .await
                        .unwrap_or_else(|error| panic!("{}: {error}", #failure))
                },
            )
        }
        (_, NativeServiceBackend::Memory) => {
            let mock = match service.service_type {
                ServiceType::Kv => quote! { ::skyzen_test::mock::InMemoryKv },
                ServiceType::Storage => quote! { ::skyzen_test::mock::InMemoryStorage },
                ServiceType::Queue => quote! { ::skyzen_test::mock::InMemoryQueue },
            };
            return Ok(quote! { #wrapper::new(#mock::new()) });
        }
        (service_type, backend) => {
            return Ok(compile_error_block(&format!(
                "unsupported native backend `{}` for portable service `{}` of type `{}`",
                backend.as_str(),
                service.name,
                service_type.as_str()
            )));
        }
    };

    let env_lit = LitStr::new(&env_key, proc_macro2::Span::call_site());
    let missing_message = LitStr::new(
        &format!(
            "portable service `{}` missing native env var `{env_key}`",
            service.name
        ),
        proc_macro2::Span::call_site(),
    );
    Ok(quote! {{
        let __skyzen_env_value = ::std::env::var(#env_lit)
            .unwrap_or_else(|_| panic!("{}", #missing_message));
        let backend = #build;
        #wrapper::new(backend)
    }})
}

/// The panic message for a native backend whose constructor failed.
fn connect_failure_lit(service_name: &str, what: &str) -> LitStr {
    LitStr::new(
        &format!("portable service `{service_name}` failed to {what}"),
        proc_macro2::Span::call_site(),
    )
}

fn generate_cloudflare_service_init(
    service: &ServiceEntry,
    manifest: &SkyzenManifest,
) -> proc_macro2::TokenStream {
    let Some(wiring) = manifest
        .cloudflare
        .as_ref()
        .and_then(|cloudflare| cloudflare.service.get(&service.name))
    else {
        return compile_error_block(&format!(
            "missing [cloudflare.service.{}] wiring for portable service `{}`",
            service.name, service.name
        ));
    };

    let binding = &wiring.binding;
    let binding_lit = LitStr::new(binding, proc_macro2::Span::call_site());
    let failure_message = LitStr::new(
        &format!(
            "portable service `{}` failed to resolve Cloudflare binding `{binding}`",
            service.name
        ),
        proc_macro2::Span::call_site(),
    );
    let backend = match service.service_type {
        ServiceType::Kv => quote! { ::skyzen_cloudflare::CfKv },
        ServiceType::Storage => quote! { ::skyzen_cloudflare::CfR2 },
        ServiceType::Queue => quote! { ::skyzen_cloudflare::CfQueue },
    };
    let wrapper = service_wrapper_path(service.service_type);

    quote! {{
        let backend = #backend::from_env(&__skyzen_wasm_env, #binding_lit)
            .unwrap_or_else(|error| panic!("{}: {error}", #failure_message));
        #wrapper::new(backend)
    }}
}

fn generate_native_database_init(
    database: &DatabaseEntry,
    manifest: &SkyzenManifest,
) -> proc_macro2::TokenStream {
    let Some(wiring) = native_database_wiring(manifest, &database.name) else {
        return compile_error_block(&format!(
            "missing [native.database.{}] wiring for portable database `{}`",
            database.name, database.name
        ));
    };

    // Every SQL driver takes the same shape — read one env var, hand the URL to a
    // `Db::connect_*` — so the arms differ only in which constructor they name.
    let connect = match (database.database_type, wiring.backend) {
        (DatabaseType::Sql, NativeDatabaseBackend::Postgres) => quote! { connect_postgres },
        (DatabaseType::Sql, NativeDatabaseBackend::Mysql) => quote! { connect_mysql },
        (DatabaseType::Sql, NativeDatabaseBackend::Sqlite) => quote! { connect_sqlite },
    };

    let url_env = &wiring.url_env;
    let env_lit = LitStr::new(url_env, proc_macro2::Span::call_site());
    let missing_message = LitStr::new(
        &format!(
            "portable database `{}` missing native env var `{url_env}`",
            database.name
        ),
        proc_macro2::Span::call_site(),
    );
    let connect_message = LitStr::new(
        &format!(
            "portable database `{}` failed to connect using `{url_env}`",
            database.name
        ),
        proc_macro2::Span::call_site(),
    );

    quote! {{
        let url = ::std::env::var(#env_lit)
            .unwrap_or_else(|_| panic!("{}", #missing_message));
        ::skyzen_services::Db::#connect(&url)
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", #connect_message))
    }}
}

fn generate_cloudflare_database_init(
    database: &DatabaseEntry,
    manifest: &SkyzenManifest,
) -> proc_macro2::TokenStream {
    let Some(wiring) = manifest
        .cloudflare
        .as_ref()
        .and_then(|cloudflare| cloudflare.database.get(&database.name))
    else {
        return compile_error_block(&format!(
            "missing [cloudflare.database.{}] wiring for portable database `{}`",
            database.name, database.name
        ));
    };

    let binding = &wiring.binding;
    let binding_lit = LitStr::new(binding, proc_macro2::Span::call_site());
    let failure_message = LitStr::new(
        &format!(
            "portable database `{}` failed to resolve Cloudflare binding `{binding}`",
            database.name
        ),
        proc_macro2::Span::call_site(),
    );

    match database.database_type {
        DatabaseType::Sql => quote! {{
            let backend = ::skyzen_cloudflare::CfD1::from_env(&__skyzen_wasm_env, #binding_lit)
                .unwrap_or_else(|error| panic!("{}: {error}", #failure_message));
            ::skyzen_services::Db::new(backend)
        }},
    }
}

fn compile_error_block(message: &str) -> proc_macro2::TokenStream {
    let message = LitStr::new(message, proc_macro2::Span::call_site());
    quote! {{
        compile_error!(#message);
        unreachable!()
    }}
}

/// Turn a manifest entry's name into a type name: `main-db` becomes `MainDb`.
///
/// `suffix` is appended after the conversion, so a database keeps its `Db` marker while a service
/// binding uses its name verbatim (`cache` becomes `Cache`).
fn pascal_ident_from_name(name: &str, suffix: &str, kind: &str) -> syn::Result<proc_macro2::Ident> {
    let mut normalized = String::with_capacity(name.len() + suffix.len());
    let mut uppercase_next = true;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if uppercase_next {
                normalized.push(ch.to_ascii_uppercase());
                uppercase_next = false;
            } else {
                normalized.push(ch);
            }
        } else {
            uppercase_next = true;
        }
    }

    if normalized.is_empty() {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            format!("{kind} name must contain at least one alphanumeric character"),
        ));
    }

    if normalized
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit())
    {
        normalized.insert(0, '_');
    }

    normalized.push_str(suffix);
    Ok(format_ident!("{normalized}"))
}

fn database_ident_from_name(name: &str) -> syn::Result<proc_macro2::Ident> {
    pascal_ident_from_name(name, "Db", "database")
}

fn service_ident_from_name(name: &str) -> syn::Result<proc_macro2::Ident> {
    pascal_ident_from_name(name, "", "service")
}

fn default_database_index(databases: &[DatabaseEntry]) -> syn::Result<Option<usize>> {
    if databases.is_empty() {
        return Ok(None);
    }

    let defaults = databases
        .iter()
        .enumerate()
        .filter_map(|(index, database)| database.default.then_some(index))
        .collect::<Vec<_>>();

    match defaults.as_slice() {
        [] if databases.len() == 1 => Ok(Some(0)),
        [] => Err(Error::new(
            proc_macro2::Span::call_site(),
            "multiple [[database]] entries require exactly one `default = true`",
        )),
        [index] => Ok(Some(*index)),
        _ => Err(Error::new(
            proc_macro2::Span::call_site(),
            "multiple [[database]] entries cannot mark more than one database as `default = true`",
        )),
    }
}

struct MainOptions {
    default_logger: bool,
    /// Path to an async function run once the server has finished draining.
    on_shutdown: Option<syn::Path>,
}

impl MainOptions {
    fn from_args(args: &Punctuated<MetaNameValue, Token![,]>) -> syn::Result<Self> {
        let mut options = Self {
            default_logger: true,
            on_shutdown: None,
        };

        for meta in args {
            if meta.path.is_ident("default_logger") {
                let Expr::Lit(ExprLit {
                    lit: Lit::Bool(bool_lit),
                    ..
                }) = &meta.value
                else {
                    return Err(Error::new_spanned(&meta.value, "expected boolean literal"));
                };
                options.default_logger = bool_lit.value;
            } else if meta.path.is_ident("on_shutdown") {
                let Expr::Path(path) = &meta.value else {
                    return Err(Error::new_spanned(
                        &meta.value,
                        "expected the path of an async function, such as                          `on_shutdown = flush_pool`",
                    ));
                };
                if options.on_shutdown.replace(path.path.clone()).is_some() {
                    return Err(Error::new_spanned(
                        &meta.path,
                        "duplicate `on_shutdown` argument",
                    ));
                }
            } else {
                return Err(Error::new_spanned(
                    &meta.path,
                    "unsupported option, expected `default_logger = true|false` or                      `on_shutdown = <async fn path>`",
                ));
            }
        }

        Ok(options)
    }
}

#[derive(Clone, Copy)]
enum CloudflareEventKind {
    Queue,
    Scheduled,
    Email,
    Tail,
}

impl CloudflareEventKind {
    /// The `WinterCG` export name, which is also the attribute's own name.
    const fn export_name(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Scheduled => "scheduled",
            Self::Email => "email",
            Self::Tail => "tail",
        }
    }
}

fn expand_cloudflare_event(
    mut function: ItemFn,
    kind: CloudflareEventKind,
) -> syn::Result<proc_macro2::TokenStream> {
    let is_async = function.sig.asyncness.is_some();
    let original_ident = function.sig.ident.clone();
    let export_name = kind.export_name();
    // The handler keeps its own name unless it already *is* the export name, in which case the
    // generated wrapper would collide with it.
    let internal_ident = if original_ident == export_name {
        format_ident!("__skyzen_entry_{export_name}")
    } else {
        original_ident
    };
    function.sig.ident = internal_ident.clone();

    let wrapper_ident = format_ident!("{export_name}");

    let (wrapper_signature, wrapper_args) = build_cloudflare_event_wrapper(&function, kind)?;

    let call = if is_async {
        quote! { #internal_ident(#wrapper_args).await }
    } else {
        quote! { #internal_ident(#wrapper_args) }
    };

    let wrapper_return = match kind {
        CloudflareEventKind::Queue => quote! {{
            let __skyzen_raw_cf_batch =
                ::skyzen_cloudflare::CfQueueBatch::new(__skyzen_raw_batch.clone());
            ::skyzen_cloudflare::IntoQueueWorkerResult::into_queue_worker_result(
                #call,
                &__skyzen_raw_cf_batch,
            )
        }},
        CloudflareEventKind::Scheduled | CloudflareEventKind::Email | CloudflareEventKind::Tail => {
            quote! { ::skyzen_cloudflare::IntoWorkerResult::into_worker_result(#call) }
        }
    };

    Ok(quote! {
        #function

        #[cfg(target_arch = "wasm32")]
        #[::skyzen::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = ::skyzen::wasm_bindgen)]
        pub async fn #wrapper_ident(
            #wrapper_signature
        ) -> Result<(), ::skyzen::wasm_bindgen::JsValue> {
            #wrapper_return
        }
    })
}

fn build_cloudflare_event_wrapper(
    function: &ItemFn,
    kind: CloudflareEventKind,
) -> syn::Result<(proc_macro2::TokenStream, proc_macro2::TokenStream)> {
    let args = function
        .sig
        .inputs
        .iter()
        .map(|arg| match arg {
            FnArg::Typed(pat_type) => Ok(pat_type),
            FnArg::Receiver(receiver) => Err(Error::new_spanned(
                receiver,
                "cloudflare event handlers may not take self arguments",
            )),
        })
        .collect::<syn::Result<Vec<_>>>()?;

    if args.is_empty() || args.len() > 3 {
        return Err(Error::new_spanned(
            &function.sig.inputs,
            "cloudflare event handlers must take `event_or_batch`, optional `Env`, and optional context",
        ));
    }

    let first_arg = event_argument_expr(&args[0].ty, kind)?;
    let mut call_args = vec![first_arg];

    if let Some(arg) = args.get(1) {
        let ident = last_type_ident(&arg.ty)?;
        if ident != "Env" {
            return Err(Error::new_spanned(
                &arg.ty,
                "the second event handler argument must be `skyzen::runtime::wasm::Env`",
            ));
        }
        call_args.push(quote! { __skyzen_event_env.clone() });
    }

    if let Some(arg) = args.get(2) {
        let ctx_expr = context_argument_expr(&arg.ty, kind)?;
        call_args.push(ctx_expr);
    }

    let wrapper_signature = match kind {
        CloudflareEventKind::Queue => quote! {
            __skyzen_raw_batch: ::skyzen_cloudflare::worker_sys::MessageBatch,
            __skyzen_event_env: ::skyzen::runtime::wasm::Env,
            __skyzen_raw_ctx: ::skyzen_cloudflare::worker_sys::Context
        },
        CloudflareEventKind::Scheduled => quote! {
            __skyzen_raw_event: ::skyzen_cloudflare::worker_sys::ScheduledEvent,
            __skyzen_event_env: ::skyzen::runtime::wasm::Env,
            __skyzen_raw_ctx: ::skyzen_cloudflare::worker_sys::ScheduleContext
        },
        CloudflareEventKind::Email => quote! {
            __skyzen_raw_email: ::skyzen_cloudflare::ffi::EmailMessageSys,
            __skyzen_event_env: ::skyzen::runtime::wasm::Env,
            __skyzen_raw_ctx: ::skyzen_cloudflare::worker_sys::Context
        },
        CloudflareEventKind::Tail => quote! {
            __skyzen_raw_traces: ::skyzen::js_sys::Array,
            __skyzen_event_env: ::skyzen::runtime::wasm::Env,
            __skyzen_raw_ctx: ::skyzen_cloudflare::worker_sys::Context
        },
    };

    Ok((wrapper_signature, quote! { #(#call_args),* }))
}

fn event_argument_expr(
    ty: &Type,
    kind: CloudflareEventKind,
) -> syn::Result<proc_macro2::TokenStream> {
    let ident = last_type_ident(ty)?;
    match kind {
        CloudflareEventKind::Queue if ident == "CfQueueBatch" => {
            Ok(quote! { ::skyzen_cloudflare::CfQueueBatch::new(__skyzen_raw_batch.clone()) })
        }
        CloudflareEventKind::Queue if ident == "QueueBatch" => {
            let batch_inner = single_generic_type(ty)?;
            Ok(quote! {
                ::skyzen_cloudflare::CfQueueBatch::new(__skyzen_raw_batch.clone())
                    .decode_json::<#batch_inner>()
                    .map_err(|error| ::skyzen::wasm_bindgen::JsValue::from_str(&error.to_string()))?
            })
        }
        CloudflareEventKind::Scheduled if ident == "CfScheduledEvent" => Ok(quote! {
            ::skyzen_cloudflare::CfScheduledEvent::new(__skyzen_raw_event.clone())
        }),
        CloudflareEventKind::Scheduled if ident == "ScheduledTick" => Ok(quote! {
            ::skyzen::events::ScheduledTick::new(
                ::skyzen_cloudflare::CfScheduledEvent::new(__skyzen_raw_event.clone())
                    .cron()
                    .map_err(|error| ::skyzen::wasm_bindgen::JsValue::from_str(&error.to_string()))?,
                ::skyzen_cloudflare::CfScheduledEvent::new(__skyzen_raw_event.clone())
                    .scheduled_time_ms()
                    .map_err(|error| ::skyzen::wasm_bindgen::JsValue::from_str(&error.to_string()))?,
            )
        }),
        CloudflareEventKind::Email if ident == "CfEmailMessage" => Ok(quote! {
            ::skyzen_cloudflare::CfEmailMessage::new(__skyzen_raw_email.clone())
        }),
        CloudflareEventKind::Tail if ident == "CfTailEvent" => Ok(quote! {
            ::skyzen_cloudflare::CfTailEvent::new(__skyzen_raw_traces.clone())
        }),
        CloudflareEventKind::Tail if ident == "Vec" => Ok(quote! {
            ::skyzen_cloudflare::CfTailEvent::new(__skyzen_raw_traces.clone())
                .traces()
                .map_err(|error| ::skyzen::wasm_bindgen::JsValue::from_str(&error.to_string()))?
        }),
        CloudflareEventKind::Queue => Err(Error::new_spanned(
            ty,
            "the first #[skyzen::queue] argument must be `CfQueueBatch` or `QueueBatch<T>`",
        )),
        CloudflareEventKind::Scheduled => Err(Error::new_spanned(
            ty,
            "the first #[skyzen::scheduled] argument must be `CfScheduledEvent` or `ScheduledTick`",
        )),
        CloudflareEventKind::Email => Err(Error::new_spanned(
            ty,
            "the first #[skyzen::email] argument must be `CfEmailMessage`",
        )),
        CloudflareEventKind::Tail => Err(Error::new_spanned(
            ty,
            "the first #[skyzen::tail] argument must be `CfTailEvent` or `Vec<TailTraceItem>`",
        )),
    }
}

fn context_argument_expr(
    ty: &Type,
    kind: CloudflareEventKind,
) -> syn::Result<proc_macro2::TokenStream> {
    let ident = last_type_ident(ty)?;
    match (kind, ident.as_str()) {
        (
            CloudflareEventKind::Queue | CloudflareEventKind::Email | CloudflareEventKind::Tail,
            "CfEventContext",
        ) => Ok(quote! { ::skyzen_cloudflare::CfEventContext::new(__skyzen_raw_ctx) }),
        (CloudflareEventKind::Scheduled, "CfScheduleContext") => {
            Ok(quote! { ::skyzen_cloudflare::CfScheduleContext::new(__skyzen_raw_ctx) })
        }
        (CloudflareEventKind::Scheduled, _) => Err(Error::new_spanned(
            ty,
            "the third #[skyzen::scheduled] argument must be `CfScheduleContext`",
        )),
        (kind, _) => Err(Error::new_spanned(
            ty,
            format!(
                "the third #[skyzen::{}] argument must be `CfEventContext`",
                kind.export_name()
            ),
        )),
    }
}

fn last_type_ident(ty: &Type) -> syn::Result<String> {
    let Type::Path(type_path) = ty else {
        return Err(Error::new_spanned(
            ty,
            "unsupported event handler argument type",
        ));
    };
    type_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .ok_or_else(|| Error::new_spanned(ty, "unsupported event handler argument type"))
}

fn single_generic_type(ty: &Type) -> syn::Result<Type> {
    let Type::Path(type_path) = ty else {
        return Err(Error::new_spanned(ty, "expected a generic type parameter"));
    };
    let segment = type_path
        .path
        .segments
        .last()
        .ok_or_else(|| Error::new_spanned(ty, "expected a generic type parameter"))?;
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(Error::new_spanned(
            ty,
            "expected a single generic argument, for example `QueueBatch<Job>`",
        ));
    };
    if args.args.len() != 1 {
        return Err(Error::new_spanned(
            ty,
            "expected a single generic argument, for example `QueueBatch<Job>`",
        ));
    }
    match args.args.first() {
        Some(syn::GenericArgument::Type(inner)) => Ok(inner.clone()),
        _ => Err(Error::new_spanned(
            ty,
            "expected a single generic type argument",
        )),
    }
}

fn first_generic_type(ty: &Type) -> Option<Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };

    args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(inner) => Some(inner.clone()),
        _ => None,
    })
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn expand_durable_object(item_struct: ItemStruct) -> proc_macro2::TokenStream {
    let self_ident = item_struct.ident.clone();
    let export_ident = format_ident!("{self_ident}Object");
    let clone_state_ident = format_ident!(
        "__skyzen_clone_do_state_{}",
        self_ident.to_string().to_lowercase()
    );
    let clone_env_ident = format_ident!(
        "__skyzen_clone_do_env_{}",
        self_ident.to_string().to_lowercase()
    );

    quote! {
        #item_struct

        #[cfg(target_arch = "wasm32")]
        const _: () = {
            use ::skyzen::wasm_bindgen as wasm_bindgen;
            use ::skyzen::wasm_bindgen::prelude::*;

            fn #clone_state_ident(
                state: &::skyzen_cloudflare::worker_sys::DurableObjectState
            ) -> ::skyzen_cloudflare::worker_sys::DurableObjectState {
                use ::core::convert::AsRef;
                use ::skyzen::wasm_bindgen::JsCast;

                let js: &::skyzen::wasm_bindgen::JsValue = state.as_ref();
                js.clone().unchecked_into()
            }

            fn #clone_env_ident(
                env: &::skyzen::runtime::wasm::Env
            ) -> ::skyzen::runtime::wasm::Env {
                env.clone()
            }

            #[wasm_bindgen(wasm_bindgen = ::skyzen::wasm_bindgen)]
            pub struct #export_ident {
                state: ::skyzen_cloudflare::worker_sys::DurableObjectState,
                env: ::skyzen::runtime::wasm::Env,
            }

            #[wasm_bindgen(wasm_bindgen = ::skyzen::wasm_bindgen)]
            impl #export_ident {
                #[wasm_bindgen(constructor, wasm_bindgen = ::skyzen::wasm_bindgen)]
                pub fn new(
                    state: ::skyzen_cloudflare::worker_sys::DurableObjectState,
                    env: ::skyzen::runtime::wasm::Env,
                ) -> Self {
                    Self { state, env }
                }

                #[wasm_bindgen(js_name = fetch, wasm_bindgen = ::skyzen::wasm_bindgen)]
                pub fn fetch(
                    &self,
                    request: ::skyzen_cloudflare::worker_sys::web_sys::Request,
                ) -> ::skyzen::js_sys::Promise {
                    let state = #clone_state_ident(&self.state);
                    let env = #clone_env_ident(&self.env);
                    ::skyzen::wasm_bindgen_futures::future_to_promise(async move {
                        ::skyzen_cloudflare::DurableObjectRuntime::<#self_ident>::fetch(
                            state,
                            env,
                            request,
                        )
                        .await
                        .map(::skyzen::wasm_bindgen::JsValue::from)
                    })
                }

                #[wasm_bindgen(js_name = alarm, wasm_bindgen = ::skyzen::wasm_bindgen)]
                pub fn alarm(&self) -> ::skyzen::js_sys::Promise {
                    let state = #clone_state_ident(&self.state);
                    let env = #clone_env_ident(&self.env);
                    ::skyzen::wasm_bindgen_futures::future_to_promise(async move {
                        ::skyzen_cloudflare::durable::invoke_alarm::<#self_ident>(state, env)
                            .await
                            .map(|_| ::skyzen::wasm_bindgen::JsValue::NULL)
                    })
                }

                #[wasm_bindgen(js_name = webSocketMessage, wasm_bindgen = ::skyzen::wasm_bindgen)]
                pub fn websocket_message(
                    &self,
                    websocket: ::skyzen_cloudflare::worker_sys::web_sys::WebSocket,
                    message: ::skyzen::wasm_bindgen::JsValue,
                ) -> ::skyzen::js_sys::Promise {
                    let state = #clone_state_ident(&self.state);
                    let env = #clone_env_ident(&self.env);
                    ::skyzen::wasm_bindgen_futures::future_to_promise(async move {
                        let message = if let Some(text) = message.as_string() {
                            ::skyzen::http_kit::ws::WebSocketMessage::Text(text.into())
                        } else {
                            ::skyzen::http_kit::ws::WebSocketMessage::Binary(
                                ::skyzen::js_sys::Uint8Array::new(&message).to_vec().into(),
                            )
                        };

                        ::skyzen_cloudflare::durable::invoke_websocket_message::<#self_ident>(
                            state,
                            env,
                            websocket,
                            message,
                        )
                        .await
                        .map(|_| ::skyzen::wasm_bindgen::JsValue::NULL)
                    })
                }

                #[wasm_bindgen(js_name = webSocketClose, wasm_bindgen = ::skyzen::wasm_bindgen)]
                pub fn websocket_close(
                    &self,
                    websocket: ::skyzen_cloudflare::worker_sys::web_sys::WebSocket,
                    code: usize,
                    reason: String,
                    was_clean: bool,
                ) -> ::skyzen::js_sys::Promise {
                    let state = #clone_state_ident(&self.state);
                    let env = #clone_env_ident(&self.env);
                    ::skyzen::wasm_bindgen_futures::future_to_promise(async move {
                        // Clamp out-of-range close codes to 1005 ("no status received") rather
                        // than panicking inside the Durable Object event path.
                        let code = u16::try_from(code).unwrap_or(1005);
                        ::skyzen_cloudflare::durable::invoke_websocket_close::<#self_ident>(
                            state,
                            env,
                            websocket,
                            code,
                            reason,
                            was_clean,
                        )
                        .await
                        .map(|_| ::skyzen::wasm_bindgen::JsValue::NULL)
                    })
                }

                #[wasm_bindgen(js_name = webSocketError, wasm_bindgen = ::skyzen::wasm_bindgen)]
                pub fn websocket_error(
                    &self,
                    websocket: ::skyzen_cloudflare::worker_sys::web_sys::WebSocket,
                    error: ::skyzen::wasm_bindgen::JsValue,
                ) -> ::skyzen::js_sys::Promise {
                    let state = #clone_state_ident(&self.state);
                    let env = #clone_env_ident(&self.env);
                    ::skyzen::wasm_bindgen_futures::future_to_promise(async move {
                        ::skyzen_cloudflare::durable::invoke_websocket_error::<#self_ident>(
                            state,
                            env,
                            websocket,
                            format!("{error:?}"),
                        )
                        .await
                        .map(|_| ::skyzen::wasm_bindgen::JsValue::NULL)
                    })
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DatabaseEntry, DatabaseType, ServiceType, SkyzenManifest, database_ident_from_name,
        default_database_index, documented_extractor_payload, documented_response_payload,
        first_generic_type, generate_cloudflare_database_init, generate_native_database_init,
        generate_native_service_init, single_generic_type,
    };
    use quote::ToTokens;
    use skyzen_manifest::Manifest;
    use syn::parse_quote;

    /// Parse a manifest through the same shared schema the macros use at compile time.
    fn manifest(source: &str) -> SkyzenManifest {
        Manifest::parse(source, "Skyzen.toml", ".")
            .expect("valid manifest")
            .data()
            .clone()
    }

    #[test]
    fn parses_portable_services_and_databases() {
        let manifest = manifest(
            r#"[[service]]
name = "cache"
type = "kv"

[[service]]
name = "uploads"
type = "storage"

[[database]]
name = "main"
type = "sql"
"#,
        );

        assert_eq!(manifest.service.len(), 2);
        assert_eq!(manifest.service[0].name, "cache");
        assert_eq!(manifest.service[0].service_type, ServiceType::Kv);
        assert_eq!(manifest.service[1].service_type, ServiceType::Storage);

        assert_eq!(manifest.database.len(), 1);
        assert_eq!(manifest.database[0].name, "main");
        assert_eq!(manifest.database[0].database_type, DatabaseType::Sql);
        assert!(!manifest.database[0].default);
    }

    #[test]
    fn missing_wiring_becomes_a_compile_error_naming_the_section_to_add() {
        let manifest = manifest("[[service]]\nname = \"cache\"\ntype = \"kv\"\n");

        let generated = generate_native_service_init(&manifest.service[0], &manifest)
            .expect("init tokens")
            .to_string();
        assert!(generated.contains("compile_error !"));
        assert!(generated.contains("[native.service.cache]"));
    }

    #[test]
    fn every_native_sql_driver_reaches_its_own_connect_constructor() {
        for (backend, expected) in [
            ("postgres", "connect_postgres"),
            ("mysql", "connect_mysql"),
            ("sqlite", "connect_sqlite"),
        ] {
            let manifest = manifest(&format!(
                "[[database]]\nname = \"main\"\ntype = \"sql\"\n\n\
                 [native.database.main]\nbackend = \"{backend}\"\nurl_env = \"DATABASE_URL\"\n"
            ));
            let generated =
                generate_native_database_init(&manifest.database[0], &manifest).to_string();
            assert!(
                generated.contains(expected),
                "backend `{backend}` should reach `{expected}`, got: {generated}"
            );
        }
    }

    #[test]
    fn generates_portable_wrapper_inits() {
        let manifest = manifest(
            r#"[[service]]
name = "cache"
type = "kv"

[[database]]
name = "main"
type = "sql"

[native.service.cache]
backend = "redis"
url_env = "CACHE_URL"

[cloudflare.database.main]
binding = "DB"
"#,
        );

        let native_service = generate_native_service_init(&manifest.service[0], &manifest)
            .expect("native service init")
            .to_string();
        assert!(native_service.contains("skyzen_services :: Kv :: new"));
        assert!(native_service.contains("skyzen_redis :: Redis :: connect"));

        let cloudflare_database =
            generate_cloudflare_database_init(&manifest.database[0], &manifest).to_string();
        assert!(cloudflare_database.contains("skyzen_services :: Db :: new"));
        assert!(cloudflare_database.contains("skyzen_cloudflare :: CfD1 :: from_env"));
    }

    #[test]
    fn documented_payload_helpers_detect_supported_extractors_and_responders() {
        let json_ty = parse_quote!(Json<CreateWidget>);
        let form_ty = parse_quote!(Form<LoginForm>);
        let query_ty = parse_quote!(Query<SearchParams>);
        let result_ty = parse_quote!(Result<Form<LoginForm>>);
        let plain_ty = parse_quote!(String);

        let (json_inner, json_content_type) = documented_extractor_payload(&json_ty)
            .unwrap()
            .expect("json payload");
        assert_eq!(json_inner.into_token_stream().to_string(), "CreateWidget");
        assert_eq!(json_content_type, "application/json");

        let (form_inner, form_content_type) = documented_extractor_payload(&form_ty)
            .unwrap()
            .expect("form payload");
        assert_eq!(form_inner.into_token_stream().to_string(), "LoginForm");
        assert_eq!(form_content_type, "application/x-www-form-urlencoded");

        let (query_inner, query_content_type) = documented_extractor_payload(&query_ty)
            .unwrap()
            .expect("query payload");
        assert_eq!(query_inner.into_token_stream().to_string(), "SearchParams");
        assert_eq!(query_content_type, "application/x-www-form-urlencoded");

        let (result_inner, result_content_type) = documented_response_payload(&result_ty)
            .unwrap()
            .expect("result payload");
        assert_eq!(result_inner.into_token_stream().to_string(), "LoginForm");
        assert_eq!(result_content_type, "application/x-www-form-urlencoded");

        assert!(documented_response_payload(&plain_ty).unwrap().is_none());
    }

    #[test]
    fn generic_type_helpers_extract_expected_types_and_fail_fast() {
        let queue_batch_ty = parse_quote!(QueueBatch<Job>);
        let result_ty = parse_quote!(Result<Job, Error>);
        let plain_ty = parse_quote!(String);

        let batch_inner = single_generic_type(&queue_batch_ty).expect("single generic");
        assert_eq!(batch_inner.into_token_stream().to_string(), "Job");

        let Err(error) = single_generic_type(&result_ty) else {
            panic!("expected Result<Job, Error> to be rejected");
        };
        assert!(
            error
                .to_string()
                .contains("expected a single generic argument")
        );

        let first_inner = first_generic_type(&result_ty).expect("first generic");
        assert_eq!(first_inner.into_token_stream().to_string(), "Job");
        assert!(first_generic_type(&plain_ty).is_none());
    }

    #[test]
    fn default_database_index_enforces_default_selection_rules() {
        let single = vec![DatabaseEntry {
            name: "main".to_owned(),
            database_type: DatabaseType::Sql,
            default: false,
        }];
        assert_eq!(default_database_index(&single).unwrap(), Some(0));

        let multiple_missing_default = vec![
            DatabaseEntry {
                name: "main".to_owned(),
                database_type: DatabaseType::Sql,
                default: false,
            },
            DatabaseEntry {
                name: "analytics".to_owned(),
                database_type: DatabaseType::Sql,
                default: false,
            },
        ];
        assert!(
            default_database_index(&multiple_missing_default)
                .unwrap_err()
                .to_string()
                .contains("require exactly one `default = true`")
        );

        let multiple_defaults = vec![
            DatabaseEntry {
                name: "main".to_owned(),
                database_type: DatabaseType::Sql,
                default: true,
            },
            DatabaseEntry {
                name: "analytics".to_owned(),
                database_type: DatabaseType::Sql,
                default: true,
            },
        ];
        assert!(
            default_database_index(&multiple_defaults)
                .unwrap_err()
                .to_string()
                .contains("cannot mark more than one database as `default = true`")
        );
    }

    #[test]
    fn the_test_macro_recognises_every_portable_service_including_the_durable_ones() {
        use super::{TestParamKind, TestService, classify_test_param};

        let cases = [
            ("Kv", TestService::Kv),
            ("Storage", TestService::Storage),
            ("Queue", TestService::Queue),
            ("DurableKv", TestService::DurableKv),
            ("DurableDb", TestService::DurableDb),
            ("Alarm", TestService::Alarm),
        ];

        for (name, expected) in cases {
            let ty: syn::Type = syn::parse_str(name).expect("type should parse");
            let kind = classify_test_param(&ty, &[]).expect(name);
            assert!(
                matches!(kind, TestParamKind::Service(service) if service == expected),
                "{name} should classify as {expected:?}"
            );
        }

        // A fully qualified path resolves through its last segment, too.
        let ty: syn::Type =
            syn::parse_str("skyzen_services::durable::Alarm").expect("type should parse");
        assert!(matches!(
            classify_test_param(&ty, &[]).expect("qualified Alarm"),
            TestParamKind::Service(TestService::Alarm)
        ));
    }

    #[test]
    fn an_unknown_test_parameter_lists_the_durable_types_it_could_have_been() {
        use super::classify_test_param;

        let ty: syn::Type = syn::parse_str("Nonsense").expect("type should parse");
        let message = classify_test_param(&ty, &[])
            .expect_err("unknown parameter types must be rejected")
            .to_string();

        for expected in ["`Kv`", "`DurableKv`", "`DurableDb`", "`Alarm`"] {
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn each_service_names_one_binding_and_one_context_builder() {
        use super::TestService;

        for service in TestService::ALL {
            let binding = service.binding_ident().to_string();
            let builder = service.context_builder().to_string();
            assert_eq!(binding, format!("__skyzen_test_{}", service.slug()));
            assert_eq!(builder, format!("with_{}", service.slug()));
            assert!(!service.construction().is_empty());
        }
    }

    #[test]
    fn a_service_binding_becomes_a_type_name_without_a_suffix() {
        use super::service_ident_from_name;

        assert_eq!(
            service_ident_from_name("cache").unwrap().to_string(),
            "Cache"
        );
        assert_eq!(
            service_ident_from_name("session-store")
                .unwrap()
                .to_string(),
            "SessionStore"
        );
        assert_eq!(
            service_ident_from_name("user_uploads").unwrap().to_string(),
            "UserUploads"
        );
        // A leading digit cannot start an identifier, so it is prefixed rather than dropped.
        assert_eq!(
            service_ident_from_name("9lives").unwrap().to_string(),
            "_9lives"
        );
        assert!(service_ident_from_name("---").is_err());
    }

    #[test]
    fn the_bare_wrapper_is_injected_only_for_an_unambiguous_service_type() {
        use super::service_type_counts;

        let manifest = manifest(
            r#"[[service]]
name = "cache"
type = "kv"

[[service]]
name = "sessions"
type = "kv"

[[service]]
name = "uploads"
type = "storage"
"#,
        );
        let counts = service_type_counts(&manifest.service);

        // Two KV namespaces: only `Cache` and `Sessions` are injected, never a bare `Kv`.
        assert_eq!(counts[&ServiceType::Kv], 2);
        // One bucket: `Uploads` and the bare `Storage` both are.
        assert_eq!(counts[&ServiceType::Storage], 1);
        assert!(!counts.contains_key(&ServiceType::Queue));
    }

    #[test]
    fn a_named_binding_derefs_to_its_portable_wrapper() {
        use super::named_binding_tokens;
        use quote::{format_ident, quote};

        let generated = named_binding_tokens(
            &format_ident!("Cache"),
            &quote! { ::skyzen_services::Kv },
            "kv `cache` not configured.",
        )
        .to_string();

        assert!(generated.contains("pub struct Cache (:: skyzen_services :: Kv)"));
        assert!(generated.contains("impl :: std :: ops :: Deref for Cache"));
        assert!(generated.contains("impl :: skyzen :: extract :: Extractor for Cache"));
        assert!(generated.contains("impl :: skyzen :: middleware :: Middleware for Cache"));
        assert!(generated.contains("fn provisions"));
        assert!(generated.contains("pub CacheNotConfigured"));
    }

    #[test]
    fn injection_installs_the_newtype_and_only_then_the_bare_wrapper() {
        use super::named_injection_tokens;
        use quote::{format_ident, quote};

        let init = quote! { ::skyzen_services::Kv::new(backend) };

        let unambiguous = named_injection_tokens(&format_ident!("Cache"), &init, true).to_string();
        assert!(unambiguous.contains("Cache :: new"));
        // Both the newtype and the bare wrapper are layered on.
        assert_eq!(unambiguous.matches("with_middleware").count(), 2);

        let ambiguous = named_injection_tokens(&format_ident!("Cache"), &init, false).to_string();
        assert!(ambiguous.contains("Cache :: new"));
        assert_eq!(ambiguous.matches("with_middleware").count(), 1);
    }

    #[test]
    fn status_code_literals_are_validated_at_expansion_time() {
        use super::{normalize_status_lit, validate_status_code};
        use proc_macro2::Span;

        assert!(validate_status_code(100, Span::call_site()).is_ok());
        assert!(validate_status_code(404, Span::call_site()).is_ok());
        assert!(validate_status_code(999, Span::call_site()).is_ok());

        assert!(validate_status_code(0, Span::call_site()).is_err());
        assert!(validate_status_code(1000, Span::call_site()).is_err());
        let error = validate_status_code(99, Span::call_site()).unwrap_err();
        assert!(error.to_string().contains("must be in the range 100..=999"));

        let valid: syn::LitInt = parse_quote!(404);
        assert!(normalize_status_lit(&valid).is_ok());
        let invalid: syn::LitInt = parse_quote!(99);
        let Err(error) = normalize_status_lit(&invalid) else {
            panic!("status literal 99 must be rejected");
        };
        assert!(error.to_string().contains("invalid HTTP status code `99`"));
    }

    #[test]
    fn identifier_normalization_is_stable() {
        assert_eq!(
            database_ident_from_name("primary").unwrap().to_string(),
            "PrimaryDb"
        );
        assert_eq!(
            database_ident_from_name("main-db").unwrap().to_string(),
            "MainDbDb"
        );
    }
}
