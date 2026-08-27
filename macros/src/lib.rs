//! Procedural macros for the Skyzen framework.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use skyzen_manifest::{
    AzureQueueTrigger, DatabaseEntry, DatabaseType, Manifest, NativeDatabaseSection,
    NativeQueueConsumer, NativeServiceSection, RdsEngine, ServiceEntry, ServiceType,
    SkyzenManifest,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
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
    let wiring = match portable_injection_wrap_steps() {
        Ok(wiring) => wiring,
        Err(error) => return error.to_compile_error().into(),
    };
    let PortableWiring { steps, consumers } = wiring;
    let azure_queue_triggers = match azure_queue_trigger_tokens() {
        Ok(triggers) => triggers,
        Err(error) => return error.to_compile_error().into(),
    };
    // Natively the factory yields the endpoint *and* the queue consumers, both built from the one
    // set of service instances; on wasm the platform owns event delivery, so it yields only the
    // endpoint and this whole arm is stripped.
    let factory_body = quote! {
        async move {
            let endpoint = #entry_call;
            #(#steps)*
            {
                #[cfg(not(target_arch = "wasm32"))]
                { (endpoint, #consumers) }
                #[cfg(target_arch = "wasm32")]
                { endpoint }
            }
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
        #[allow(clippy::redundant_clone)]
        fn main() {
            #init_logging
            ::skyzen::runtime::native::launch(
                ::skyzen::runtime::native::LaunchOptions {
                    listen: ::skyzen::runtime::native::apply_cli_overrides(::std::env::args()),
                    azure_queue_triggers: #azure_queue_triggers,
                },
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
///
/// # Arguments
///
/// - `migrations = <path>` — a `skyzen_services::Migrations` value (a `static`
///   produced by [`embed_migrations!`]) to apply to the test's database before the body runs. The
///   test must take a database parameter for there to be anything to migrate.
///
/// ```ignore
/// static MIGRATIONS: Migrations = skyzen::embed_migrations!("migrations");
///
/// #[skyzen::test(migrations = MIGRATIONS)]
/// async fn a_user_can_be_inserted(db: Db) {
///     db.query("INSERT INTO users (email) VALUES (?)")
///         .bind("a@b.c")
///         .execute()
///         .await
///         .unwrap();
/// }
/// ```
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated);
    let options = match TestOptions::from_args(&args) {
        Ok(options) => options,
        Err(error) => return error.to_compile_error().into(),
    };

    let function = parse_macro_input!(item as ItemFn);
    match expand_test(function, &options) {
        Ok(tokens) => tokens,
        Err(error) => error.to_compile_error().into(),
    }
}

/// Read a directory of `<version>_<name>.sql` files and embed them as a
/// `skyzen_services::Migrations` set.
///
/// The path is relative to the crate's `CARGO_MANIFEST_DIR`, so it names the same directory
/// wherever in the crate the macro is written:
///
/// ```ignore
/// use skyzen_services::Migrations;
///
/// static MIGRATIONS: Migrations = skyzen::embed_migrations!("migrations");
/// ```
///
/// The files are read, ordered and checksummed at compile time by the same code `skyzen migrate`
/// uses at deploy time, so the CLI can never apply a different set from the one the binary
/// carries. A file whose name is not `<version>_<name>.sql`, or a version claimed twice, is a
/// compile error pointing at the path literal. The contents reach the binary through
/// `include_str!`, so editing a migration rebuilds the crate — a `fs::read` inside the macro would
/// be invisible to cargo, and the stale binary would keep claiming the old checksum.
///
/// The expansion is usable anywhere a value is: a `static` item (the usual place, so
/// `#[skyzen::test(migrations = ...)]` can name it), a `const`, or an expression.
#[proc_macro]
pub fn embed_migrations(input: TokenStream) -> TokenStream {
    let directory = parse_macro_input!(input as LitStr);
    match expand_embed_migrations(&directory) {
        Ok(tokens) => tokens.into(),
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

/// Export a queue consumer entrypoint: the Cloudflare `queue` handler on wasm targets, and the
/// handler Skyzen's own polling loop drives natively.
///
/// # Dual-target consumption
///
/// On wasm the platform pushes a batch into the exported `queue` handler. Natively there is no
/// platform to do that, so `[[native.queue_consumer]]` entries in `Skyzen.toml` tell
/// `#[skyzen::main]` to run the loop itself — receive, invoke this function, settle — against the
/// portable `[[service]]` they name.
///
/// A native consumer needs a handler it can call with nothing but a batch, so with native
/// consumers declared the annotated function must take exactly one argument, `QueueBatch<T>`. The
/// wasm-only extras (`Env`, `CfEventContext`, `CfQueueBatch`) have no native counterpart and are
/// rejected rather than silently ignored.
///
/// The generated glue is referenced by `#[skyzen::main]`, so the two must sit in the same module
/// — the crate root, for the usual `main.rs` or `lib.rs`. A handler that lives elsewhere is
/// reachable with a `use` of the generated `__SkyzenNativeQueueHandler` next to `#[skyzen::main]`.
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

/// The arguments `#[skyzen::test]` accepts.
#[derive(Default)]
struct TestOptions {
    /// The path named by `migrations = <path>`, applied to the test's default database.
    migrations: Option<syn::Path>,
}

/// Written by hand rather than derived: `syn::Path` only implements `Debug` under syn's
/// `extra-traits` feature, which would be a real compile-time cost across every crate in the build
/// in exchange for one line of test output.
impl core::fmt::Debug for TestOptions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let migrations = self
            .migrations
            .as_ref()
            .map(|path| quote! { #path }.to_string());
        f.debug_struct("TestOptions")
            .field("migrations", &migrations)
            .finish()
    }
}

impl TestOptions {
    fn from_args(args: &Punctuated<MetaNameValue, Token![,]>) -> syn::Result<Self> {
        let mut options = Self::default();

        for meta in args {
            if meta.path.is_ident("migrations") {
                let Expr::Path(path) = &meta.value else {
                    return Err(Error::new_spanned(
                        &meta.value,
                        "expected the path of a `Migrations` value, such as \
                         `migrations = MIGRATIONS`",
                    ));
                };
                if options.migrations.replace(path.path.clone()).is_some() {
                    return Err(Error::new_spanned(
                        &meta.path,
                        "duplicate `migrations` argument",
                    ));
                }
            } else {
                return Err(Error::new_spanned(
                    &meta.path,
                    "unsupported option, expected `migrations = <path to a Migrations value>`",
                ));
            }
        }

        Ok(options)
    }
}

/// Expand `embed_migrations!("dir")` into a `Migrations` built from that directory.
fn expand_embed_migrations(directory: &LitStr) -> syn::Result<proc_macro2::TokenStream> {
    let resolved = resolve_migrations_directory(directory)?;
    embed_migrations_tokens(&resolved, directory.span())
}

/// Turn the macro's argument into an absolute directory under the calling crate's root.
fn resolve_migrations_directory(directory: &LitStr) -> syn::Result<PathBuf> {
    let span = directory.span();
    let relative = directory.value();
    if relative.trim().is_empty() {
        return Err(Error::new(
            span,
            "embed_migrations!() needs a directory, such as `embed_migrations!(\"migrations\")`",
        ));
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|error| Error::new(span, format!("failed to read CARGO_MANIFEST_DIR: {error}")))?;
    Ok(PathBuf::from(manifest_dir).join(&relative))
}

/// Read `directory` and render the `Migrations` expression, reporting every failure at `span`.
fn embed_migrations_tokens(
    directory: &Path,
    span: proc_macro2::Span,
) -> syn::Result<proc_macro2::TokenStream> {
    // The same reader `skyzen migrate` runs, so a directory the CLI would reject cannot compile
    // and a directory that compiles is one the CLI will read identically.
    let files = skyzen_manifest::migrations::load(directory)
        .map_err(|error| Error::new(span, error.to_string()))?;

    let entries = files.iter().map(|file| {
        let version = file.version;
        let name = LitStr::new(&file.name, span);
        // Absolute, because `include_str!` resolves relative to the *source file* that expands the
        // macro — a relative path would break the moment the macro is used from a nested module.
        let path = LitStr::new(&file.path.display().to_string(), span);
        let checksum = file.checksum;
        quote! {
            ::skyzen_services::migrate::Migration::embedded(
                #version,
                #name,
                ::core::include_str!(#path),
                [#(#checksum),*],
            )
        }
    });
    let count = files.len();

    // The array is bound to a `static` rather than passed as a literal: `Migration` holds a `Cow`,
    // so it has drop glue, and a temporary with drop glue cannot be promoted to `'static` inside a
    // `static` or `const` initializer. Taking a reference to a named `static` always can.
    Ok(quote! {
        {
            static __SKYZEN_EMBEDDED_MIGRATIONS:
                [::skyzen_services::migrate::Migration; #count] = [#(#entries),*];
            ::skyzen_services::migrate::Migrations::from_static(&__SKYZEN_EMBEDDED_MIGRATIONS)
        }
    })
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

fn expand_test(mut function: ItemFn, options: &TestOptions) -> syn::Result<TokenStream> {
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
    let setup_statements = test_setup_statements(&bindings.requirements, &databases, options)?;

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
    options: &TestOptions,
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    let mut statements = Vec::new();
    push_test_service_setup(requirements, &mut statements);
    push_test_database_setup(requirements, databases, &mut statements)?;
    push_test_migrations(requirements, options, &mut statements)?;
    push_test_context_setup(requirements, &mut statements);
    Ok(statements)
}

/// Apply `migrations = <path>` to the database the test asked for.
///
/// Only the default database: a test naming several databases has no one answer to "which one do
/// these migrations describe", and guessing would silently migrate the wrong one. Naming a set
/// with no database to apply it to is a mistake rather than a no-op, so it is refused at the
/// argument.
fn push_test_migrations(
    requirements: &TestRequirements,
    options: &TestOptions,
    statements: &mut Vec<proc_macro2::TokenStream>,
) -> syn::Result<()> {
    let Some(migrations) = &options.migrations else {
        return Ok(());
    };

    if !requirements.databases.default_db {
        return Err(Error::new_spanned(
            migrations,
            "`migrations` needs a database to apply to; add a `Db` parameter to the test",
        ));
    }

    // The production runner, against the in-memory database — so a migration that would fail on a
    // real deploy fails here too, rather than being waved through by a test-only shortcut.
    statements.push(quote! {
        ::skyzen_services::Db::migrate(&__skyzen_test_default_db, &#migrations)
            .await
            .unwrap_or_else(|error| {
                panic!("failed to apply migrations to the in-memory test database: {error}")
            });
    });
    Ok(())
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

/// What `#[skyzen::main]` inserts between building the endpoint and launching it.
struct PortableWiring {
    /// One group of statements per manifest entry: bind the service, then wrap the endpoint with
    /// the middleware that injects it.
    steps: Vec<proc_macro2::TokenStream>,
    /// The `ConsumerSet` the native runtime is launched with — `()` when the manifest declares no
    /// `[[native.queue_consumer]]`.
    consumers: proc_macro2::TokenStream,
}

fn portable_injection_wrap_steps() -> syn::Result<PortableWiring> {
    let empty = PortableWiring {
        steps: Vec::new(),
        consumers: quote! { () },
    };

    let Some(manifest) = load_manifest()? else {
        return Ok(empty);
    };

    let services = &manifest.service;
    let databases = &manifest.database;
    if services.is_empty() && databases.is_empty() {
        return Ok(empty);
    }
    let default_database = default_database_index(databases)?;

    let mut steps = Vec::with_capacity(services.len() + databases.len() + 1);
    let service_type_counts = service_type_counts(services);
    let mut service_bindings = HashMap::new();

    // On wasm, `__skyzen_wasm_env` is bound by the factory closure parameter generated in
    // `#[skyzen::main]`; the environment is threaded explicitly rather than read from a
    // thread-local that concurrent invocations could race.

    for service in services {
        let ident = service_ident_from_name(&service.name)?;
        let binding = service_binding_ident(&ident);
        let native_init = generate_native_service_init(service, &manifest);
        let cloudflare_init = generate_cloudflare_service_init(service, &manifest);
        // The bare `Kv`/`Storage`/`Queue` extractor names whichever service of that type is the
        // only one, so it is injected exactly when the type is unambiguous.
        let inject_bare = service_type_counts[&service.service_type] == 1;
        steps.push(named_injection_tokens(
            &binding,
            &ident,
            &native_init,
            &cloudflare_init,
            inject_bare,
        ));
        service_bindings.insert(service.name.clone(), binding);
    }

    for (index, database) in databases.iter().enumerate() {
        let ident = database_ident_from_name(&database.name)?;
        let binding = service_binding_ident(&ident);
        let native_init = generate_native_database_init(database, &manifest);
        let cloudflare_init = generate_cloudflare_database_init(database, &manifest);
        let inject_bare = default_database == Some(index);
        steps.push(named_injection_tokens(
            &binding,
            &ident,
            &native_init,
            &cloudflare_init,
            inject_bare,
        ));
    }

    let consumers = queue_consumer_tokens(&manifest, &service_bindings)?;

    Ok(PortableWiring { steps, consumers })
}

/// The local binding that holds one manifest entry's service for the length of the factory.
///
/// The service is built **once** and cloned into the middleware and into any consumer that reads
/// it: building it twice would give an in-memory backend two unrelated instances, so a message
/// enqueued through the injected `Queue` would never reach the consumer polling "the same" queue.
fn service_binding_ident(ident: &proc_macro2::Ident) -> proc_macro2::Ident {
    format_ident!("__skyzen_service_{}", ident.to_string().to_lowercase())
}

/// Build the `QueueConsumers` value for every `[[native.queue_consumer]]` entry.
///
/// Everything the runtime would otherwise have to re-check — that the named service exists, is a
/// queue, is declared once, and that the retry delay fits the portable `QueueRetry` — is settled
/// here, at compile time.
fn queue_consumer_tokens(
    manifest: &SkyzenManifest,
    service_bindings: &HashMap<String, proc_macro2::Ident>,
) -> syn::Result<proc_macro2::TokenStream> {
    let consumers = native_queue_consumers(manifest);
    if consumers.is_empty() {
        return Ok(quote! { () });
    }

    let mut claimed = HashSet::new();
    let mut entries = Vec::with_capacity(consumers.len());

    for consumer in consumers {
        let service = manifest
            .service
            .iter()
            .find(|service| service.name == consumer.service)
            .ok_or_else(|| {
                Error::new(
                    proc_macro2::Span::call_site(),
                    format!(
                        "[[native.queue_consumer]] names `{}`, which is not a [[service]] in this manifest",
                        consumer.service
                    ),
                )
            })?;

        if service.service_type != ServiceType::Queue {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "[[native.queue_consumer]] can only consume a queue, but service `{}` is of type `{}`",
                    consumer.service,
                    service.service_type.as_str()
                ),
            ));
        }

        if !claimed.insert(consumer.service.clone()) {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "[[native.queue_consumer]] declares `{}` twice; raise `concurrency` on the one entry instead",
                    consumer.service
                ),
            ));
        }

        let binding = service_bindings.get(&consumer.service).ok_or_else(|| {
            Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "[[native.queue_consumer]] names `{}`, which has no generated binding",
                    consumer.service
                ),
            )
        })?;

        let name = LitStr::new(&consumer.service, proc_macro2::Span::call_site());
        let concurrency = consumer.concurrency.get();
        let batch_size = consumer.batch_size.get();
        let poll_wait = duration_tokens(consumer.poll_wait);
        let visibility_timeout = consumer.visibility_timeout.map_or_else(
            || quote! { ::core::option::Option::None },
            |timeout| {
                let timeout = duration_tokens(timeout);
                quote! { ::core::option::Option::Some(#timeout) }
            },
        );
        let retry_seconds = retry_delay_seconds(consumer)?;

        entries.push(quote! {
            (
                ::skyzen::runtime::consumer::ConsumerConfig {
                    queue: ::std::string::String::from(#name),
                    concurrency: ::core::num::NonZeroUsize::new(#concurrency)
                        .expect("the manifest rejects a zero concurrency"),
                    batch_size: ::core::num::NonZeroUsize::new(#batch_size)
                        .expect("the manifest rejects a zero batch size"),
                    poll_wait: #poll_wait,
                    visibility_timeout: #visibility_timeout,
                    default_retry: ::skyzen_services::QueueRetry::new()
                        .with_delay_seconds(#retry_seconds),
                },
                ::std::clone::Clone::clone(&#binding),
            )
        });
    }

    Ok(quote! {
        ::skyzen::runtime::consumer::QueueConsumers::new(
            __SkyzenNativeQueueHandler,
            ::std::vec![#(#entries),*],
        )
    })
}

/// A `Duration` literal, always exactly the value the manifest parsed.
fn duration_tokens(duration: core::time::Duration) -> proc_macro2::TokenStream {
    let seconds = duration.as_secs();
    let nanos = duration.subsec_nanos();
    quote! { ::core::time::Duration::new(#seconds, #nanos) }
}

/// The retry delay in the whole seconds the portable [`QueueRetry`] carries.
///
/// A sub-second delay is rejected rather than rounded: the portable retry has no finer unit, so
/// silently turning `"250ms"` into "no delay" would make the manifest lie about redelivery.
fn retry_delay_seconds(consumer: &NativeQueueConsumer) -> syn::Result<u32> {
    if consumer.retry_delay.subsec_nanos() != 0 {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "[[native.queue_consumer]] `{}` sets a `retry_delay` finer than a second; queue retries are delayed in whole seconds",
                consumer.service
            ),
        ));
    }

    u32::try_from(consumer.retry_delay.as_secs()).map_err(|_| {
        Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "[[native.queue_consumer]] `{}` sets a `retry_delay` longer than a queue retry can express",
                consumer.service
            ),
        )
    })
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

/// Bind one manifest entry's service and wrap the router with the middleware that injects it.
///
/// The service lands in a named local rather than a temporary so that everything needing it —
/// the newtype middleware, the bare wrapper, a queue consumer — shares the one instance.
///
/// The named newtype is always installed. `inject_bare` additionally installs the portable wrapper
/// itself, which is what makes `async fn h(kv: Kv)` work when the manifest declares exactly one
/// service of that type (or the default database).
fn named_injection_tokens(
    binding: &proc_macro2::Ident,
    ident: &proc_macro2::Ident,
    native_init: &proc_macro2::TokenStream,
    cloudflare_init: &proc_macro2::TokenStream,
    inject_bare: bool,
) -> proc_macro2::TokenStream {
    let bare = if inject_bare {
        quote! {
            let endpoint = ::skyzen::__private::with_middleware(
                endpoint,
                ::std::clone::Clone::clone(&#binding),
            );
        }
    } else {
        quote! {}
    };

    quote! {
        let #binding = {
            #[cfg(not(target_arch = "wasm32"))]
            { #native_init }
            #[cfg(target_arch = "wasm32")]
            { #cloudflare_init }
        };
        let endpoint = ::skyzen::__private::with_middleware(
            endpoint,
            #ident::new(::std::clone::Clone::clone(&#binding)),
        );
        #bare
    }
}

/// Read the project's `Skyzen.toml` through the shared schema.
///
/// The file is optional — an application can wire every service by hand — so a missing file is
/// `Ok(None)` rather than an error. Everything else (unreadable, malformed, unknown key,
/// unsupported `type`) is a compile error, reported at the macro's call site.
fn load_manifest() -> syn::Result<Option<SkyzenManifest>> {
    Ok(load_manifest_document()?.map(|manifest| manifest.data().clone()))
}

/// Read the project's `Skyzen.toml`, keeping the resolved environment overlays.
///
/// [`load_manifest`] is enough for wiring, which only ever reads the base document; the whole
/// document is needed to answer questions about *any* environment, such as whether a queue
/// handler is consumed somewhere.
fn load_manifest_document() -> syn::Result<Option<Manifest>> {
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
        .map(Some)
        .map_err(|error| Error::new(proc_macro2::Span::call_site(), error.to_string()))
}

/// The `[[native.queue_consumer]]` entries the manifest declares.
/// The `[[azure.queue_triggers]]` entries, as the runtime's own static table.
///
/// Read at compile time for the usual reason: the set of function names this binary answers is
/// fixed by the manifest, so the runtime should not have to re-read a file to learn it. The names
/// themselves were validated when the manifest was parsed, which is what turns a malformed or
/// reserved one into a compile error rather than a bad bundle.
fn azure_queue_trigger_tokens() -> syn::Result<proc_macro2::TokenStream> {
    Ok(load_manifest()?.map_or_else(
        || quote! { &[] },
        |manifest| azure_trigger_table(azure_queue_triggers(&manifest)),
    ))
}

/// Render the trigger table the runtime is launched with.
fn azure_trigger_table(triggers: &[AzureQueueTrigger]) -> proc_macro2::TokenStream {
    let entries = triggers.iter().map(|trigger| {
        let function = &trigger.function;
        let queue = &trigger.queue;
        quote! {
            ::skyzen::runtime::azure::QueueTrigger { function: #function, queue: #queue }
        }
    });

    quote! { &[#(#entries),*] }
}

/// The `[[azure.queue_triggers]]` entries this manifest declares.
fn azure_queue_triggers(manifest: &SkyzenManifest) -> &[AzureQueueTrigger] {
    manifest
        .azure
        .as_ref()
        .map_or(&[], |azure| azure.queue_triggers.as_slice())
}

fn native_queue_consumers(manifest: &SkyzenManifest) -> &[NativeQueueConsumer] {
    manifest
        .native
        .as_ref()
        .map_or(&[], |native| native.queue_consumer.as_slice())
}

/// Whether *any* target consumes the queue handler this crate declares.
///
/// Cloudflare consumers can be declared in an environment overlay alone, so every resolved
/// environment is consulted rather than only the base document — otherwise a Worker that consumes
/// `jobs` in staging only would be told its handler is unreachable.
fn queue_handler_is_consumed(manifest: &Manifest) -> bool {
    if !native_queue_consumers(manifest.data()).is_empty()
        || !azure_queue_triggers(manifest.data()).is_empty()
    {
        return true;
    }

    core::iter::once(None)
        .chain(manifest.environment_names().map(Some))
        .any(|environment| {
            manifest
                .cloudflare(environment)
                .ok()
                .flatten()
                .is_some_and(|cloudflare| !cloudflare.queues.consumers.is_empty())
        })
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

/// Read one environment variable at startup, panicking with the entry that asked for it.
///
/// Every native backend that takes a URL, a bucket or a connection string reads it this way, so
/// the lookup and its failure message are written once here rather than in each arm.
fn env_value_expr(kind: &str, entry: &str, variable: &str) -> proc_macro2::TokenStream {
    let variable_lit = LitStr::new(variable, proc_macro2::Span::call_site());
    let missing_message = LitStr::new(
        &format!("portable {kind} `{entry}` missing native env var `{variable}`"),
        proc_macro2::Span::call_site(),
    );
    quote! {
        ::std::env::var(#variable_lit).unwrap_or_else(|_| panic!("{}", #missing_message))
    }
}

fn generate_native_service_init(
    service: &ServiceEntry,
    manifest: &SkyzenManifest,
) -> proc_macro2::TokenStream {
    let Some(wiring) = native_service_wiring(manifest, &service.name) else {
        return compile_error_block(&format!(
            "missing [native.service.{}] wiring for portable service `{}`",
            service.name, service.name
        ));
    };

    let wrapper = service_wrapper_path(service.service_type);
    let Some(backend) = native_backend_tokens(service, wiring) else {
        // Naming what the backend *does* provide is what turns this from "no" into a correction:
        // the mistake is almost always a queue backend under a `type = "kv"` entry, or the reverse.
        let backend = wiring.backend();
        let provides = backend.service_type().map_or_else(
            || "every portable service".to_owned(),
            |service_type| format!("the `{}` service", service_type.as_str()),
        );
        return compile_error_block(&format!(
            "portable service `{}` is declared as `{}`, but `backend = \"{}\"` provides {provides}",
            service.name,
            service.service_type.as_str(),
            backend.as_str(),
        ));
    };

    quote! {{
        let backend = #backend;
        #wrapper::new(backend)
    }}
}

/// The constructor call one service's wiring expands to, or `None` when the backend it names
/// implements a different portable service than the entry declares.
///
/// A required key is no longer an arm's concern: the manifest schema rejects a wiring that leaves
/// one out, so every field named below is present by construction.
fn native_backend_tokens(
    service: &ServiceEntry,
    wiring: &NativeServiceSection,
) -> Option<proc_macro2::TokenStream> {
    if matches!(wiring, NativeServiceSection::Memory(_)) {
        // The one backend that is not a service type's own: every type has a mock.
        return Some(match service.service_type {
            ServiceType::Kv => quote! { ::skyzen_test::mock::InMemoryKv::new() },
            ServiceType::Storage => quote! { ::skyzen_test::mock::InMemoryStorage::new() },
            ServiceType::Queue => quote! { ::skyzen_test::mock::InMemoryQueue::new() },
        });
    }

    let name = service.name.as_str();
    match service.service_type {
        ServiceType::Kv => native_kv_tokens(name, wiring),
        ServiceType::Storage => native_storage_tokens(name, wiring),
        ServiceType::Queue => native_queue_tokens(name, wiring),
    }
}

/// The KV backends.
fn native_kv_tokens(name: &str, wiring: &NativeServiceSection) -> Option<proc_macro2::TokenStream> {
    Some(match wiring {
        NativeServiceSection::Redis(redis) => {
            let url = env_value_expr("service", name, &redis.url_env);
            let failure =
                connect_failure_lit(name, &format!("connect to Redis using `{}`", redis.url_env));
            quote! {
                ::skyzen_redis::Redis::connect(&#url)
                    .await
                    .unwrap_or_else(|error| panic!("{}: {error}", #failure))
            }
        }
        NativeServiceSection::DynamoDb(dynamodb) => {
            // No environment variable: the table is named in the manifest, and the credentials and
            // region come from the ambient AWS chain `DynamoKv::from_env` loads. The two options
            // are applied only when the manifest asks for them, so the constructor's own defaults
            // stay the defaults.
            let table = LitStr::new(&dynamodb.table, proc_macro2::Span::call_site());
            let mut build = quote! { ::skyzen_aws::DynamoKv::from_env(#table).await };
            if let Some(ttl_attribute) = &dynamodb.ttl_attribute {
                let ttl_lit = LitStr::new(ttl_attribute, proc_macro2::Span::call_site());
                build = quote! { #build.with_ttl_attribute(#ttl_lit) };
            }
            if let Some(consistent_reads) = dynamodb.consistent_reads {
                build = quote! { #build.with_consistent_reads(#consistent_reads) };
            }
            build
        }
        NativeServiceSection::Cosmos(cosmos) => {
            // `CosmosKv::from_env` reads the container's definition before it returns, so a
            // container whose partition key or time-to-live this backend cannot work with fails
            // here, at startup, rather than on the first write.
            let database = LitStr::new(&cosmos.database, proc_macro2::Span::call_site());
            let container = LitStr::new(&cosmos.container, proc_macro2::Span::call_site());
            let failure = connect_failure_lit(
                name,
                &format!(
                    "bind to Cosmos DB container `{}` in database `{}`",
                    cosmos.container, cosmos.database
                ),
            );
            quote! {
                ::skyzen_azure::CosmosKv::from_env(#database, #container)
                    .await
                    .unwrap_or_else(|error| panic!("{}: {error}", #failure))
            }
        }
        _ => return None,
    })
}

/// The object storage backends.
fn native_storage_tokens(
    name: &str,
    wiring: &NativeServiceSection,
) -> Option<proc_macro2::TokenStream> {
    Some(match wiring {
        NativeServiceSection::S3(s3) => {
            let bucket = env_value_expr("service", name, &s3.bucket_env);
            quote! { ::skyzen_s3::S3Storage::from_env(&#bucket).await }
        }
        NativeServiceSection::Blob(blob) => {
            let connection = env_value_expr("service", name, &blob.connection_env);
            let container = LitStr::new(&blob.container, proc_macro2::Span::call_site());
            let failure = connect_failure_lit(
                name,
                &format!(
                    "reach the Azure Blob container `{}` using `{}`",
                    blob.container, blob.connection_env
                ),
            );
            quote! {
                ::skyzen_azure::AzureBlob::from_connection_string(&#connection, #container)
                    .unwrap_or_else(|error| panic!("{}: {error}", #failure))
            }
        }
        _ => return None,
    })
}

/// The message queue backends.
fn native_queue_tokens(
    name: &str,
    wiring: &NativeServiceSection,
) -> Option<proc_macro2::TokenStream> {
    Some(match wiring {
        NativeServiceSection::Sqs(sqs) => {
            let url = env_value_expr("service", name, &sqs.url_env);
            // `SqsQueue::from_env` builds a *standard* queue and refuses a `.fifo` URL, which
            // needs a message group id on every send. A FIFO queue is wired in code with
            // `SqsQueue::fifo`, so the manifest path reports the mismatch rather than hiding it.
            let failure = connect_failure_lit(
                name,
                &format!("use the SQS queue named by `{}`", sqs.url_env),
            );
            quote! {
                ::skyzen_aws::SqsQueue::from_env(&#url)
                    .await
                    .unwrap_or_else(|error| panic!("{}: {error}", #failure))
            }
        }
        NativeServiceSection::ServiceBus(service_bus) => {
            let connection = env_value_expr("service", name, &service_bus.connection_env);
            let queue = LitStr::new(&service_bus.queue, proc_macro2::Span::call_site());
            let failure = connect_failure_lit(
                name,
                &format!(
                    "reach the Service Bus queue `{}` using `{}`",
                    service_bus.queue, service_bus.connection_env
                ),
            );
            quote! {
                ::skyzen_azure::ServiceBusQueue::from_connection_string(&#connection, #queue)
                    .unwrap_or_else(|error| panic!("{}: {error}", #failure))
            }
        }
        NativeServiceSection::StorageQueue(storage_queue) => {
            // The signed URL *is* the credential, so the constructor is handed the variable's name
            // and reports it itself rather than being handed a value read here.
            let variable = LitStr::new(&storage_queue.sas_url_env, proc_macro2::Span::call_site());
            let failure = connect_failure_lit(
                name,
                &format!(
                    "reach the Azure Storage queue named by `{}`",
                    storage_queue.sas_url_env
                ),
            );
            quote! {
                ::skyzen_azure::AzureStorageQueue::from_sas_env(#variable)
                    .unwrap_or_else(|error| panic!("{}: {error}", #failure))
            }
        }
        _ => return None,
    })
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

    let name = database.name.as_str();

    // Every sqlx driver takes the same shape — read one env var, hand the URL to a `Db::connect_*`
    // — so those arms differ only in which constructor they name. The other two are not a
    // `Db::connect_*` at all, and each returns its own expression: the RDS Data API is an HTTP
    // service reached by ARN whose constructor reads what it needs from the environment, and Azure
    // SQL is reached through a synchronous constructor taking a config rather than a URL.
    let (connect, url_env) = match (database.database_type, wiring) {
        (DatabaseType::Sql, NativeDatabaseSection::Postgres(sql)) => {
            (quote! { connect_postgres }, &sql.url_env)
        }
        (DatabaseType::Sql, NativeDatabaseSection::Mysql(sql)) => {
            (quote! { connect_mysql }, &sql.url_env)
        }
        (DatabaseType::Sql, NativeDatabaseSection::Sqlite(sql)) => {
            (quote! { connect_sqlite }, &sql.url_env)
        }
        (DatabaseType::Sql, NativeDatabaseSection::AzureSql(sql)) => {
            // `AzureSqlDb::new` is synchronous and takes an `AzureSqlConfig`, not a URL: what the
            // variable holds is an ADO.NET connection string, which the config parses. Nothing is
            // dialled, so a wrong password surfaces on the first query — but a connection string
            // this backend cannot use fails right here, at startup.
            let connection = env_value_expr("database", name, &sql.url_env);
            let failure = LitStr::new(
                &format!(
                    "portable database `{name}` failed to reach Azure SQL using `{}`",
                    sql.url_env
                ),
                proc_macro2::Span::call_site(),
            );
            return quote! {{
                let backend = ::skyzen_azure::AzureSqlDb::new(
                    ::skyzen_azure::AzureSqlConfig::new(#connection)
                )
                .unwrap_or_else(|error| panic!("{}: {error}", #failure));
                ::skyzen_services::Db::new(backend)
            }};
        }
        (DatabaseType::Sql, NativeDatabaseSection::RdsData(rds)) => {
            // Either the wiring names all four values a Data API call is addressed by, and they
            // are handed straight to `from_parts`, or it names none and `from_env` reads them.
            // A half-written wiring never reaches here — the manifest parse rejects it — and is
            // reported rather than assumed, so the rule has one implementation.
            let parts = match rds.parts() {
                Ok(parts) => parts,
                Err(error) => {
                    return compile_error_block(&format!("[native.database.{name}] {error}"));
                }
            };

            let build = parts.map_or_else(|| rds_from_env_tokens(name), rds_from_parts_tokens);

            return quote! {{
                let backend = #build;
                ::skyzen_services::Db::new(backend)
            }};
        }
    };

    let url = env_value_expr("database", name, url_env);
    let connect_message = LitStr::new(
        &format!("portable database `{name}` failed to connect using `{url_env}`"),
        proc_macro2::Span::call_site(),
    );

    quote! {{
        ::skyzen_services::Db::#connect(&#url)
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", #connect_message))
    }}
}

/// The RDS Data API backend built from the four values a wiring names.
///
/// Infallible: nothing is read and nothing is parsed here, so there is no failure to report — the
/// ARNs are first exercised by the first statement, like every other Data API call.
fn rds_from_parts_tokens(parts: skyzen_manifest::RdsDataParts<'_>) -> proc_macro2::TokenStream {
    let resource_arn = LitStr::new(parts.resource_arn, proc_macro2::Span::call_site());
    let secret_arn = LitStr::new(parts.secret_arn, proc_macro2::Span::call_site());
    let database = LitStr::new(parts.database, proc_macro2::Span::call_site());
    let engine = rds_engine_tokens(parts.engine);
    quote! {
        ::skyzen_aws::RdsDataDb::from_parts(#resource_arn, #secret_arn, #database, #engine).await
    }
}

/// The RDS Data API backend built from the four variables its own constructor reads.
fn rds_from_env_tokens(name: &str) -> proc_macro2::TokenStream {
    let failure = LitStr::new(
        &format!("portable database `{name}` failed to reach Aurora through the RDS Data API"),
        proc_macro2::Span::call_site(),
    );
    quote! {
        ::skyzen_aws::RdsDataDb::from_env()
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", #failure))
    }
}

/// The `RdsEngine` variant a manifest's `engine` names.
///
/// The manifest models the engine as its own enum so a typo is a parse error; this maps that enum
/// onto the one `skyzen-aws` takes, which is the only place the two spellings meet.
fn rds_engine_tokens(engine: RdsEngine) -> proc_macro2::TokenStream {
    match engine {
        RdsEngine::AuroraPostgres => quote! { ::skyzen_aws::RdsEngine::AuroraPostgres },
        RdsEngine::AuroraMysql => quote! { ::skyzen_aws::RdsEngine::AuroraMysql },
    }
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

/// The native half of `#[skyzen::queue]`: the handler the polling loop calls.
///
/// Emitted only when the manifest declares `[[native.queue_consumer]]` entries, which is exactly
/// when `#[skyzen::main]` references it. A wasm-only Worker therefore expands byte for byte as it
/// did before native consumption existed.
fn native_queue_handler(
    function: &ItemFn,
    internal_ident: &proc_macro2::Ident,
) -> syn::Result<proc_macro2::TokenStream> {
    let arguments = &function.sig.inputs;
    let [FnArg::Typed(batch)] = arguments.iter().collect::<Vec<_>>()[..] else {
        return Err(Error::new_spanned(
            arguments,
            "a #[skyzen::queue] handler driven by [[native.queue_consumer]] takes exactly one \
             argument, `QueueBatch<T>`: `Env` and `CfEventContext` exist only on Cloudflare",
        ));
    };

    let batch_type = &batch.ty;
    if last_type_ident(batch_type)? != "QueueBatch" {
        return Err(Error::new_spanned(
            batch_type,
            "a #[skyzen::queue] handler driven by [[native.queue_consumer]] must take \
             `QueueBatch<T>`: `CfQueueBatch` wraps a Cloudflare message batch and has no native \
             counterpart",
        ));
    }
    let message_type = single_generic_type(batch_type)?;

    let call = if function.sig.asyncness.is_some() {
        quote! { #internal_ident(__skyzen_batch).await }
    } else {
        quote! { #internal_ident(__skyzen_batch) }
    };

    Ok(quote! {
        /// The `#[skyzen::queue]` handler, as the native consumer runtime drives it.
        #[cfg(not(target_arch = "wasm32"))]
        #[doc(hidden)]
        #[derive(Debug, Clone, Copy)]
        pub struct __SkyzenNativeQueueHandler;

        #[cfg(not(target_arch = "wasm32"))]
        impl ::skyzen::runtime::consumer::QueueConsumer for __SkyzenNativeQueueHandler {
            // An `async` block rather than an `async fn` so that a *synchronous* handler is still
            // run lazily, when the driver polls the future inside its panic guard, rather than
            // eagerly here — where a panicking handler would escape that guard. The lint would
            // have this written as an `async fn`, which a synchronous handler then trips the
            // opposite lint with, so the shape is pinned here rather than in every user's crate.
            #[allow(clippy::manual_async_fn)]
            fn handle(
                &self,
                __skyzen_batch: ::skyzen_services::QueueBatch<::std::vec::Vec<u8>>,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<
                    ::skyzen_services::QueueBatchDisposition,
                    ::skyzen_services::BoxError,
                >,
            > + ::std::marker::Send {
                async move {
                    let __skyzen_batch = __skyzen_batch.decode_json::<#message_type>()?;
                    ::skyzen::runtime::consumer::IntoQueueDisposition::into_queue_disposition(#call)
                }
            }
        }
    })
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

    // The native consumer glue and `#[skyzen::main]`'s reference to it are gated on the very same
    // manifest question, which is what keeps a wasm-only Worker's expansion unchanged.
    let native_glue = match kind {
        CloudflareEventKind::Queue => {
            let manifest = load_manifest_document()?;
            match manifest {
                Some(manifest) if !native_queue_consumers(manifest.data()).is_empty() => {
                    native_queue_handler(&function, &internal_ident)?
                }
                Some(manifest) if !queue_handler_is_consumed(&manifest) => {
                    return Err(Error::new_spanned(
                        &function.sig.ident,
                        "this #[skyzen::queue] handler is never invoked: declare the queue it \
                         consumes as [[native.queue_consumer]] for native targets, or as \
                         [[cloudflare.queues.consumers]] for Cloudflare",
                    ));
                }
                _ => proc_macro2::TokenStream::new(),
            }
        }
        CloudflareEventKind::Scheduled | CloudflareEventKind::Email | CloudflareEventKind::Tail => {
            proc_macro2::TokenStream::new()
        }
    };

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

        #native_glue

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
        DatabaseEntry, DatabaseType, HashMap, ServiceType, SkyzenManifest,
        database_ident_from_name, default_database_index, documented_extractor_payload,
        documented_response_payload, first_generic_type, generate_cloudflare_database_init,
        generate_native_database_init, generate_native_service_init, single_generic_type,
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

    /// The tokens `[native.service.cache]` with `wiring` expands to, for a service of `kind`.
    fn service_init(kind: &str, wiring: &str) -> String {
        let manifest = manifest(&format!(
            "[[service]]\nname = \"cache\"\ntype = \"{kind}\"\n\n[native.service.cache]\n{wiring}\n"
        ));
        generate_native_service_init(&manifest.service[0], &manifest).to_string()
    }

    /// The tokens `[native.database.main]` with `wiring` expands to.
    fn database_init(wiring: &str) -> String {
        let manifest = manifest(&format!(
            "[[database]]\nname = \"main\"\ntype = \"sql\"\n\n[native.database.main]\n{wiring}\n"
        ));
        generate_native_database_init(&manifest.database[0], &manifest).to_string()
    }

    #[test]
    fn missing_wiring_becomes_a_compile_error_naming_the_section_to_add() {
        let manifest = manifest("[[service]]\nname = \"cache\"\ntype = \"kv\"\n");

        let generated = generate_native_service_init(&manifest.service[0], &manifest).to_string();
        assert!(generated.contains("compile_error !"));
        assert!(generated.contains("[native.service.cache]"));
    }

    #[test]
    fn a_backend_of_the_wrong_service_type_is_a_compile_error_naming_both() {
        // A queue backend under a `type = "kv"` service parses — the wiring table is well formed —
        // and is caught here, where the service it belongs to is known.
        let generated = service_init("kv", "backend = \"servicebus\"\nqueue = \"jobs\"");
        assert!(generated.contains("compile_error !"), "{generated}");
        assert!(generated.contains("servicebus"), "{generated}");
        assert!(generated.contains("kv"), "{generated}");
    }

    #[test]
    fn every_native_service_backend_reaches_its_own_constructor() {
        for (kind, wiring, expected) in [
            (
                "kv",
                "backend = \"redis\"\nurl_env = \"CACHE_URL\"",
                "skyzen_redis :: Redis :: connect",
            ),
            (
                "kv",
                "backend = \"dynamodb\"\ntable = \"skyzen-sessions\"",
                "skyzen_aws :: DynamoKv :: from_env",
            ),
            (
                "kv",
                "backend = \"cosmos\"\ndatabase = \"appdb\"\ncontainer = \"sessions\"",
                "skyzen_azure :: CosmosKv :: from_env",
            ),
            (
                "storage",
                "backend = \"s3\"\nbucket_env = \"UPLOADS_BUCKET\"",
                "skyzen_s3 :: S3Storage :: from_env",
            ),
            (
                "storage",
                "backend = \"blob\"\ncontainer = \"uploads\"",
                "skyzen_azure :: AzureBlob :: from_connection_string",
            ),
            (
                "queue",
                "backend = \"sqs\"\nurl_env = \"JOBS_QUEUE_URL\"",
                "skyzen_aws :: SqsQueue :: from_env",
            ),
            (
                "queue",
                "backend = \"servicebus\"\nqueue = \"jobs\"",
                "skyzen_azure :: ServiceBusQueue :: from_connection_string",
            ),
            (
                "queue",
                "backend = \"storage-queue\"\nsas_url_env = \"JOBS_SAS_URL\"",
                "skyzen_azure :: AzureStorageQueue :: from_sas_env",
            ),
            (
                "queue",
                "backend = \"memory\"",
                "skyzen_test :: mock :: InMemoryQueue",
            ),
        ] {
            let generated = service_init(kind, wiring);
            assert!(
                generated.contains(expected),
                "`{wiring}` should reach `{expected}`, got: {generated}"
            );
            assert!(!generated.contains("compile_error !"), "{generated}");
        }
    }

    #[test]
    fn a_backend_that_reads_a_variable_names_it_and_the_service_that_asked_for_it() {
        for (kind, wiring, variable) in [
            (
                "kv",
                "backend = \"redis\"\nurl_env = \"CACHE_URL\"",
                "CACHE_URL",
            ),
            (
                "storage",
                "backend = \"blob\"\ncontainer = \"uploads\"",
                "AZURE_STORAGE_CONNECTION_STRING",
            ),
            (
                "storage",
                "backend = \"blob\"\ncontainer = \"uploads\"\nconnection_env = \"UPLOADS_ACCOUNT\"",
                "UPLOADS_ACCOUNT",
            ),
            (
                "queue",
                "backend = \"servicebus\"\nqueue = \"jobs\"",
                "SERVICEBUS_CONNECTION_STRING",
            ),
            (
                "queue",
                "backend = \"storage-queue\"\nsas_url_env = \"JOBS_SAS_URL\"",
                "JOBS_SAS_URL",
            ),
        ] {
            let generated = service_init(kind, wiring);
            assert!(generated.contains(variable), "{wiring}: {generated}");
        }

        // The two backends that authenticate through an ambient chain read nothing themselves.
        let dynamodb = service_init("kv", "backend = \"dynamodb\"\ntable = \"sessions\"");
        assert!(!dynamodb.contains("env :: var"), "{dynamodb}");
    }

    #[test]
    fn a_dynamodb_wiring_applies_only_the_options_it_declares() {
        let bare = service_init("kv", "backend = \"dynamodb\"\ntable = \"sessions\"");
        assert!(!bare.contains("with_ttl_attribute"), "{bare}");
        assert!(!bare.contains("with_consistent_reads"), "{bare}");

        let configured = service_init(
            "kv",
            "backend = \"dynamodb\"\ntable = \"sessions\"\n\
             ttl_attribute = \"ttl\"\nconsistent_reads = true",
        );
        assert!(
            configured.contains("with_ttl_attribute (\"ttl\")"),
            "{configured}"
        );
        assert!(
            configured.contains("with_consistent_reads (true)"),
            "{configured}"
        );
    }

    #[test]
    fn the_async_constructors_are_awaited_and_the_synchronous_ones_are_not() {
        // Getting this wrong is a type error in the generated code rather than a wrong result, but
        // the error lands in a user's crate — so it is asserted here, where it is cheap.
        for (kind, wiring) in [
            ("kv", "backend = \"dynamodb\"\ntable = \"sessions\""),
            (
                "kv",
                "backend = \"cosmos\"\ndatabase = \"appdb\"\ncontainer = \"sessions\"",
            ),
        ] {
            let generated = service_init(kind, wiring);
            assert!(generated.contains(". await"), "{wiring}: {generated}");
        }

        for (kind, wiring) in [
            ("storage", "backend = \"blob\"\ncontainer = \"uploads\""),
            ("queue", "backend = \"servicebus\"\nqueue = \"jobs\""),
            (
                "queue",
                "backend = \"storage-queue\"\nsas_url_env = \"JOBS_SAS_URL\"",
            ),
        ] {
            let generated = service_init(kind, wiring);
            assert!(!generated.contains(". await"), "{wiring}: {generated}");
        }
    }

    #[test]
    fn the_rds_data_api_is_built_from_its_own_environment_and_wrapped_in_a_db() {
        let generated = database_init("backend = \"rds-data\"");
        assert!(
            generated.contains("skyzen_aws :: RdsDataDb :: from_env ()"),
            "{generated}"
        );
        assert!(
            generated.contains("skyzen_services :: Db :: new"),
            "{generated}"
        );
        assert!(generated.contains(". await"), "{generated}");
        // It reads its four variables itself, so the expansion reads none.
        assert!(!generated.contains("env :: var"), "{generated}");
    }

    #[test]
    fn an_rds_data_wiring_that_names_its_four_values_is_built_from_them_instead() {
        let generated = database_init(
            "backend = \"rds-data\"\n\
             resource_arn = \"arn:aws:rds:us-east-1:111122223333:cluster:skyzen\"\n\
             secret_arn = \"arn:aws:secretsmanager:us-east-1:111122223333:secret:skyzen-Ab12Cd\"\n\
             database = \"appdb\"\nengine = \"aurora-mysql\"",
        );

        assert!(
            generated.contains("skyzen_aws :: RdsDataDb :: from_parts"),
            "{generated}"
        );
        assert!(
            generated.contains("skyzen_aws :: RdsEngine :: AuroraMysql"),
            "{generated}"
        );
        assert!(generated.contains("appdb"), "{generated}");
        assert!(
            generated.contains("skyzen_services :: Db :: new"),
            "{generated}"
        );
        assert!(generated.contains(". await"), "{generated}");
        // The values are in the manifest, so neither constructor reads a variable.
        assert!(!generated.contains("from_env"), "{generated}");
        assert!(!generated.contains("env :: var"), "{generated}");
    }

    #[test]
    fn azure_sql_is_built_from_a_config_holding_the_named_variables_connection_string() {
        let generated =
            database_init("backend = \"azure-sql\"\nurl_env = \"AZURE_SQL_CONNECTION_STRING\"");
        assert!(
            generated.contains("skyzen_azure :: AzureSqlDb :: new"),
            "{generated}"
        );
        assert!(
            generated.contains("skyzen_azure :: AzureSqlConfig :: new"),
            "{generated}"
        );
        assert!(
            generated.contains("skyzen_services :: Db :: new"),
            "{generated}"
        );
        // The connection string comes from the variable the wiring names, not from the backend's
        // own `from_env`, which would ignore it.
        assert!(
            generated.contains("AZURE_SQL_CONNECTION_STRING"),
            "{generated}"
        );
        // The constructor is synchronous; awaiting it would not compile in the user's crate.
        assert!(!generated.contains(". await"), "{generated}");
    }

    #[test]
    fn every_native_sql_driver_reaches_its_own_connect_constructor() {
        for (backend, expected) in [
            ("postgres", "connect_postgres"),
            ("mysql", "connect_mysql"),
            ("sqlite", "connect_sqlite"),
        ] {
            let generated = database_init(&format!(
                "backend = \"{backend}\"\nurl_env = \"DATABASE_URL\""
            ));
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

        let native_service =
            generate_native_service_init(&manifest.service[0], &manifest).to_string();
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
            migrations_dir: None,
        }];
        assert_eq!(default_database_index(&single).unwrap(), Some(0));

        let multiple_missing_default = vec![
            DatabaseEntry {
                name: "main".to_owned(),
                database_type: DatabaseType::Sql,
                default: false,
                migrations_dir: None,
            },
            DatabaseEntry {
                name: "analytics".to_owned(),
                database_type: DatabaseType::Sql,
                default: false,
                migrations_dir: None,
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
                migrations_dir: None,
            },
            DatabaseEntry {
                name: "analytics".to_owned(),
                database_type: DatabaseType::Sql,
                default: true,
                migrations_dir: None,
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
        use super::{named_injection_tokens, service_binding_ident};
        use quote::{format_ident, quote};

        let ident = format_ident!("Cache");
        let binding = service_binding_ident(&ident);
        let native = quote! { ::skyzen_services::Kv::new(backend) };
        let cloudflare = quote! { ::skyzen_services::Kv::new(cf_backend) };

        let unambiguous =
            named_injection_tokens(&binding, &ident, &native, &cloudflare, true).to_string();
        assert!(unambiguous.contains("Cache :: new"));
        // The service is bound once and cloned into each layer, never built twice.
        assert!(unambiguous.contains("let __skyzen_service_cache"));
        assert_eq!(unambiguous.matches("Kv :: new").count(), 2);
        // Both the newtype and the bare wrapper are layered on.
        assert_eq!(unambiguous.matches("with_middleware").count(), 2);

        let ambiguous =
            named_injection_tokens(&binding, &ident, &native, &cloudflare, false).to_string();
        assert!(ambiguous.contains("Cache :: new"));
        assert_eq!(ambiguous.matches("with_middleware").count(), 1);
    }

    #[test]
    fn a_queue_consumer_expands_its_manifest_values_into_the_runtime_config() {
        use super::{queue_consumer_tokens, service_binding_ident};
        use quote::format_ident;

        let manifest = manifest(
            r#"[[service]]
name = "jobs"
type = "queue"

[[native.queue_consumer]]
service = "jobs"
concurrency = 3
batch_size = 7
poll_wait = "5s"
visibility_timeout = "1m"
retry_delay = "45s"
"#,
        );
        let bindings = HashMap::from([(
            "jobs".to_owned(),
            service_binding_ident(&format_ident!("Jobs")),
        )]);

        let tokens = queue_consumer_tokens(&manifest, &bindings)
            .expect("the consumer is valid")
            .to_string();

        assert!(tokens.contains("QueueConsumers :: new"));
        assert!(tokens.contains("__SkyzenNativeQueueHandler"));
        assert!(tokens.contains("NonZeroUsize :: new (3usize)"));
        assert!(tokens.contains("NonZeroUsize :: new (7usize)"));
        assert!(tokens.contains("Duration :: new (5u64 , 0u32)"));
        assert!(tokens.contains("Duration :: new (60u64 , 0u32)"));
        assert!(tokens.contains("with_delay_seconds (45u32)"));
        assert!(tokens.contains("& __skyzen_service_jobs"));
    }

    #[test]
    fn an_application_with_no_consumers_launches_with_an_empty_consumer_set() {
        use super::queue_consumer_tokens;

        let manifest = manifest(
            r#"[[service]]
name = "jobs"
type = "queue"
"#,
        );

        let tokens = queue_consumer_tokens(&manifest, &HashMap::new())
            .expect("no consumers is not an error")
            .to_string();
        assert_eq!(tokens, "()");
    }

    #[test]
    fn azure_queue_triggers_become_the_runtimes_static_table() {
        use super::{azure_queue_triggers, azure_trigger_table};

        let declared = manifest(
            r#"[[azure.queue_triggers]]
function = "process"
queue = "jobs"
connection_env = "AzureWebJobsStorage"

[[azure.queue_triggers]]
function = "reindex"
queue = "search"
connection_env = "SEARCH_STORAGE"
"#,
        );

        let tokens = azure_trigger_table(azure_queue_triggers(&declared)).to_string();
        // The connection is the CLI's business — it goes in `function.json`, not into the binary.
        assert!(tokens.contains("function : \"process\""), "{tokens}");
        assert!(tokens.contains("queue : \"jobs\""), "{tokens}");
        assert!(tokens.contains("function : \"reindex\""), "{tokens}");
        assert!(!tokens.contains("AzureWebJobsStorage"), "{tokens}");
    }

    #[test]
    fn an_application_with_no_azure_triggers_launches_with_an_empty_table() {
        use super::{azure_queue_triggers, azure_trigger_table};

        let tokens = azure_trigger_table(azure_queue_triggers(&manifest(""))).to_string();
        assert_eq!(tokens, "& []");
    }

    #[test]
    fn a_queue_consumer_must_name_a_declared_queue_service() {
        use super::queue_consumer_tokens;

        let unknown = manifest(
            r#"[[native.queue_consumer]]
service = "jobs"
"#,
        );
        let error = queue_consumer_tokens(&unknown, &HashMap::new())
            .expect_err("an unknown service is refused");
        assert!(error.to_string().contains("not a [[service]]"));

        let wrong_type = manifest(
            r#"[[service]]
name = "jobs"
type = "kv"

[[native.queue_consumer]]
service = "jobs"
"#,
        );
        let error = queue_consumer_tokens(&wrong_type, &HashMap::new())
            .expect_err("a non-queue service is refused");
        assert!(error.to_string().contains("is of type `kv`"));
    }

    #[test]
    fn one_queue_is_consumed_by_one_entry_however_concurrent_it_is() {
        use super::{queue_consumer_tokens, service_binding_ident};
        use quote::format_ident;

        let manifest = manifest(
            r#"[[service]]
name = "jobs"
type = "queue"

[[native.queue_consumer]]
service = "jobs"

[[native.queue_consumer]]
service = "jobs"
concurrency = 2
"#,
        );
        let bindings = HashMap::from([(
            "jobs".to_owned(),
            service_binding_ident(&format_ident!("Jobs")),
        )]);

        let error = queue_consumer_tokens(&manifest, &bindings)
            .expect_err("a queue is consumed by one entry");
        assert!(error.to_string().contains("declares `jobs` twice"));
    }

    #[test]
    fn a_retry_delay_finer_than_the_portable_retry_is_refused_rather_than_rounded() {
        use super::retry_delay_seconds;
        use skyzen_manifest::NativeQueueConsumer;

        let consumer = |retry_delay: &str| -> NativeQueueConsumer {
            manifest(&format!(
                "[[native.queue_consumer]]\nservice = \"jobs\"\nretry_delay = \"{retry_delay}\"\n"
            ))
            .native
            .expect("native section")
            .queue_consumer
            .remove(0)
        };

        assert_eq!(retry_delay_seconds(&consumer("45s")).unwrap(), 45);
        assert_eq!(retry_delay_seconds(&consumer("0s")).unwrap(), 0);

        let error = retry_delay_seconds(&consumer("250ms")).unwrap_err();
        assert!(error.to_string().contains("finer than a second"));
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

/// `embed_migrations!` and `#[skyzen::test(migrations = ...)]`, exercised without a compiler.
///
/// The expansion itself is checked end-to-end by the `skyzen` crate's `tests/migrations.rs`, which
/// actually invokes the macro. What is worth testing here is the half a successful compile can
/// never show: the *rejections*. There is no `trybuild` in this workspace, so rather than assert on
/// compiler output, the rejecting paths are plain functions returning `syn::Result` and are called
/// directly.
#[cfg(test)]
mod embed_migrations_tests {
    use super::{TestOptions, embed_migrations_tokens};
    use quote::quote;
    use std::path::{Path, PathBuf};
    use syn::{MetaNameValue, Token, parse_quote, punctuated::Punctuated};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn expand(name: &str) -> syn::Result<String> {
        embed_migrations_tokens(&fixture(name), proc_macro2::Span::call_site())
            .map(|tokens| tokens.to_string())
    }

    fn rejection(name: &str) -> String {
        expand(name).expect_err(name).to_string()
    }

    fn options(args: proc_macro2::TokenStream) -> syn::Result<TestOptions> {
        let parsed: Punctuated<MetaNameValue, Token![,]> =
            syn::parse::Parser::parse2(Punctuated::parse_terminated, args)?;
        TestOptions::from_args(&parsed)
    }

    #[test]
    fn a_valid_directory_embeds_every_file_in_version_order() {
        let expanded = expand("good").expect("the fixture is valid");

        // Both files, with their parsed versions and names.
        assert!(expanded.contains("1u64"), "{expanded}");
        assert!(expanded.contains("\"create_users\""), "{expanded}");
        assert!(expanded.contains("2u64"), "{expanded}");
        assert!(expanded.contains("\"seed_and_index\""), "{expanded}");

        // `create_users` must come first: the array order is the run order.
        let first = expanded.find("create_users").expect("first migration");
        let second = expanded.find("seed_and_index").expect("second migration");
        assert!(first < second, "{expanded}");
    }

    #[test]
    fn the_contents_reach_the_binary_through_include_str_with_an_absolute_path() {
        // A `fs::read` inside the macro is invisible to cargo, so an edited migration would not
        // rebuild the crate and the binary would keep claiming the old checksum. And the path must
        // be absolute, because `include_str!` resolves relative to the *source file* that expands
        // the macro — a relative path breaks as soon as the macro is called from a nested module.
        let expanded = expand("good").expect("the fixture is valid");
        assert!(expanded.contains("include_str !"), "{expanded}");
        let absolute = fixture("good").join("0001_create_users.sql");
        // Render the expectation through the same string-literal escaping the expansion uses,
        // so a Windows path's backslashes compare in their escaped form.
        let literal = proc_macro2::Literal::string(&absolute.display().to_string()).to_string();
        assert!(expanded.contains(&literal), "{expanded}");
    }

    #[test]
    fn the_expansion_binds_a_static_array_rather_than_a_temporary() {
        // `Migration` holds a `Cow`, so it has drop glue and cannot be promoted to `'static` as a
        // temporary inside a `static`/`const` initializer. Passing a reference to a named `static`
        // is what makes `static MIGRATIONS: Migrations = embed_migrations!("…");` compile at all.
        let expanded = expand("good").expect("the fixture is valid");
        assert!(
            expanded.contains("static __SKYZEN_EMBEDDED_MIGRATIONS"),
            "{expanded}"
        );
        assert!(
            expanded.contains("from_static (& __SKYZEN_EMBEDDED_MIGRATIONS)"),
            "{expanded}"
        );
    }

    #[test]
    fn the_embedded_checksum_is_the_sha256_of_the_file() {
        // Pinned rather than recomputed: this is the value a deployed `_skyzen_migrations` row
        // holds, so the macro and `skyzen migrate` agreeing is the whole point.
        let expanded = expand("good").expect("the fixture is valid");
        let sql = std::fs::read_to_string(fixture("good").join("0001_create_users.sql"))
            .expect("fixture readable");
        let checksum = skyzen_manifest::migrations::checksum(&sql);
        let rendered = checksum
            .iter()
            .map(|byte| format!("{byte}u8"))
            .collect::<Vec<_>>()
            .join(" , ");
        assert!(expanded.contains(&rendered), "{expanded}");
    }

    #[test]
    fn an_empty_directory_embeds_an_empty_set() {
        let expanded = expand("empty").expect("an empty directory is not an error");
        assert!(expanded.contains("Migration ; 0usize] = []"), "{expanded}");
    }

    #[test]
    fn a_misnamed_file_is_rejected_with_the_shape_it_should_have_had() {
        let rendered = rejection("bad_name");
        assert!(rendered.contains("0001-create-a.sql"), "{rendered}");
        assert!(rendered.contains("0001_create_users.sql"), "{rendered}");
    }

    #[test]
    fn two_files_claiming_one_version_are_rejected_naming_both() {
        let rendered = rejection("duplicate_version");
        assert!(
            rendered.contains("0001_a.sql") && rendered.contains("1_b.sql"),
            "{rendered}"
        );
    }

    #[test]
    fn a_missing_directory_is_rejected_naming_it() {
        let rendered = rejection("not_a_directory_at_all");
        assert!(rendered.contains("not_a_directory_at_all"), "{rendered}");
        assert!(rendered.contains("does not exist"), "{rendered}");
    }

    #[test]
    fn a_rejection_becomes_a_real_compile_error() {
        // Every failure is built with `Error::new(<the path literal's span>, ...)`, so the
        // compiler underlines the argument rather than the whole invocation. Comparing spans
        // directly needs proc-macro2's `span-locations`, which is not worth turning on for the
        // whole build; what is checkable here is that the failure renders as a `compile_error!`
        // rather than being swallowed into an empty expansion.
        let literal: syn::LitStr = parse_quote!("tests/fixtures/bad_name");
        let error =
            embed_migrations_tokens(&fixture("bad_name"), literal.span()).expect_err("misnamed");
        assert!(
            error
                .to_compile_error()
                .to_string()
                .contains("compile_error"),
            "{error}"
        );
    }

    #[test]
    fn the_test_attribute_takes_a_migrations_path() {
        let parsed = options(quote! { migrations = crate::MIGRATIONS }).expect("valid argument");
        let path = parsed.migrations.expect("a path was given");
        assert_eq!(quote! { #path }.to_string(), "crate :: MIGRATIONS");
    }

    #[test]
    fn the_test_attribute_still_takes_no_arguments_at_all() {
        assert!(
            options(quote! {})
                .expect("no arguments")
                .migrations
                .is_none()
        );
    }

    #[test]
    fn a_migrations_value_that_is_not_a_path_is_rejected() {
        let error = options(quote! { migrations = "migrations" }).expect_err("string literal");
        assert!(error.to_string().contains("path"), "{error}");
    }

    #[test]
    fn an_unknown_argument_is_rejected_listing_the_one_that_exists() {
        let error = options(quote! { schema = MIGRATIONS }).expect_err("unknown option");
        assert!(error.to_string().contains("migrations"), "{error}");
    }

    #[test]
    fn a_repeated_migrations_argument_is_rejected() {
        let error = options(quote! { migrations = A, migrations = B }).expect_err("duplicate");
        assert!(error.to_string().contains("duplicate"), "{error}");
    }
}
