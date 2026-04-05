//! Procedural macros for the Skyzen framework.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use std::{collections::HashSet, fs, path::PathBuf};
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, parse_quote,
    punctuated::Punctuated,
    spanned::Spanned,
    Attribute, Data, DeriveInput, Error, Expr, ExprLit, Fields, FnArg, Item, ItemEnum, ItemFn,
    ItemStruct, Lit, LitInt, LitStr, Meta, MetaNameValue, PatType, ReturnType, Token, Type,
    Variant,
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
    let datasource_wrap_steps = match datasource_wrap_steps() {
        Ok(steps) => steps,
        Err(error) => return error.to_compile_error().into(),
    };
    let portable_injection_wrap_steps = match portable_injection_wrap_steps() {
        Ok(steps) => steps,
        Err(error) => return error.to_compile_error().into(),
    };
    let native_factory = quote! {
        async move {
            let endpoint = #entry_call;
            #(#datasource_wrap_steps)*
            #(#portable_injection_wrap_steps)*
            endpoint
        }
    };
    let wasm_factory = native_factory.clone();

    let init_logging = if options.default_logger {
        quote! { ::skyzen::runtime::native::init_logging(); }
    } else {
        quote! {}
    };

    let output = quote! {
        ::skyzen::import_config!();

        #function

        #[cfg(not(target_arch = "wasm32"))]
        fn main() {
            #init_logging
            ::skyzen::runtime::native::apply_cli_overrides(::std::env::args());
            ::skyzen::runtime::native::launch(|| #native_factory);
        }

        #[cfg(target_arch = "wasm32")]
        use ::skyzen::wasm_bindgen as wasm_bindgen;
        #[cfg(target_arch = "wasm32")]
        use ::skyzen::wasm_bindgen_futures as wasm_bindgen_futures;
        #[cfg(target_arch = "wasm32")]
        #[::skyzen::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = ::skyzen::wasm_bindgen)]
        pub async fn fetch(
            request: ::skyzen::runtime::wasm::Request,
            env: ::skyzen::runtime::wasm::Env,
            ctx: ::skyzen::runtime::wasm::ExecutionContext,
        ) -> Result<::skyzen::runtime::wasm::Response, ::skyzen::wasm_bindgen::JsValue> {
            ::skyzen::runtime::wasm::launch(|| #wasm_factory, request, env, ctx).await
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

/// Import datasource declarations from `Skyzen.toml` and generate strong-typed extractors.
///
/// This macro has no runtime side effects. It only generates types, initialization methods,
/// middleware implementations, and extractors.
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

    Ok(quote! {
        #function

        #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
        const _: fn() = || {
            #(#assertions)*
        };

        #(
            #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
            #schema_collector_defs
        )*

        #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
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
    Kv,
    Storage,
    Queue,
    Db,
    NamedDatabase {
        type_ident: proc_macro2::Ident,
        database_index: usize,
    },
}

fn expand_test(mut function: ItemFn) -> syn::Result<TokenStream> {
    if function.sig.asyncness.is_none() {
        return Err(Error::new_spanned(
            &function.sig.fn_token,
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

    let manifest = load_manifest_value()?;
    let databases = manifest
        .as_ref()
        .map(load_databases_from_value)
        .transpose()?
        .unwrap_or_default();
    let database_types = databases
        .iter()
        .enumerate()
        .map(|(index, database)| {
            database_ident_from_name(&database.name).map(|ident| (index, ident))
        })
        .collect::<syn::Result<Vec<_>>>()?;

    let mut requires_test_context = false;
    let mut requires_kv = false;
    let mut requires_storage = false;
    let mut requires_queue = false;
    let mut requires_default_db = false;
    let mut required_named_db_indices = Vec::new();
    let mut binding_statements = Vec::new();

    for input in inputs {
        let FnArg::Typed(pat_type) = input else {
            return Err(Error::new_spanned(
                input,
                "#[skyzen::test] does not support methods; use a free function",
            ));
        };

        let pat = pat_type.pat;
        let ty = pat_type.ty;
        let kind = classify_test_param(ty.as_ref(), &database_types)?;

        match kind {
            TestParamKind::TestContext => {
                requires_test_context = true;
                binding_statements.push(quote! {
                    let #pat: #ty = __skyzen_test_context.clone();
                });
            }
            TestParamKind::Kv => {
                requires_kv = true;
                binding_statements.push(quote! {
                    let #pat: #ty = __skyzen_test_kv.clone();
                });
            }
            TestParamKind::Storage => {
                requires_storage = true;
                binding_statements.push(quote! {
                    let #pat: #ty = __skyzen_test_storage.clone();
                });
            }
            TestParamKind::Queue => {
                requires_queue = true;
                binding_statements.push(quote! {
                    let #pat: #ty = __skyzen_test_queue.clone();
                });
            }
            TestParamKind::Db => {
                requires_default_db = true;
                binding_statements.push(quote! {
                    let #pat: #ty = __skyzen_test_default_db.clone();
                });
            }
            TestParamKind::NamedDatabase {
                type_ident,
                database_index,
            } => {
                if !required_named_db_indices.contains(&database_index) {
                    required_named_db_indices.push(database_index);
                }
                let db_ident = format_ident!("__skyzen_test_named_db_{database_index}");
                binding_statements.push(quote! {
                    let #pat: #ty = #type_ident::new(#db_ident.clone());
                });
            }
        }
    }

    if requires_test_context {
        requires_kv = true;
        requires_storage = true;
        requires_queue = true;
    }

    let mut setup_statements = Vec::new();

    if requires_kv {
        setup_statements.push(quote! {
            let __skyzen_test_kv =
                ::skyzen_services::Kv::new(::skyzen_test::mock::InMemoryKv::new());
        });
    }

    if requires_storage {
        setup_statements.push(quote! {
            let __skyzen_test_storage =
                ::skyzen_services::Storage::new(::skyzen_test::mock::InMemoryStorage::new());
        });
    }

    if requires_queue {
        setup_statements.push(quote! {
            let __skyzen_test_queue =
                ::skyzen_services::Queue::new(::skyzen_test::mock::InMemoryQueue::new());
        });
    }

    let requires_any_db = requires_default_db || !required_named_db_indices.is_empty();
    if requires_any_db {
        let default_database = default_database_index(&databases)?;
        let mut prepared_indices = required_named_db_indices.clone();
        if let Some(default_index) = default_database {
            if requires_default_db && !prepared_indices.contains(&default_index) {
                prepared_indices.push(default_index);
            }
        } else if requires_default_db {
            prepared_indices.push(usize::MAX);
        }

        prepared_indices.sort_unstable();
        let synthesized_default_db = prepared_indices.contains(&usize::MAX);

        for index in prepared_indices {
            if index == usize::MAX {
                setup_statements.push(quote! {
                    let __skyzen_test_default_db = ::skyzen_test::mock::InMemoryDb::new()
                        .await
                        .unwrap_or_else(|error| panic!("failed to initialize in-memory test database: {error}"))
                        .into_db();
                });
                continue;
            }

            let db_ident = format_ident!("__skyzen_test_named_db_{index}");
            let database_name = &databases[index].name;
            let init_message = LitStr::new(
                &format!("failed to initialize in-memory test database `{database_name}`"),
                proc_macro2::Span::call_site(),
            );
            setup_statements.push(quote! {
                let #db_ident = ::skyzen_test::mock::InMemoryDb::new()
                    .await
                    .unwrap_or_else(|error| panic!("{}: {error}", #init_message))
                    .into_db();
            });

            if default_database == Some(index) && requires_default_db {
                setup_statements.push(quote! {
                    let __skyzen_test_default_db = #db_ident.clone();
                });
            }
        }

        if requires_default_db && default_database.is_none() && !synthesized_default_db {
            setup_statements.push(quote! {
                let __skyzen_test_default_db = ::skyzen_test::mock::InMemoryDb::new()
                    .await
                    .unwrap_or_else(|error| panic!("failed to initialize in-memory test database: {error}"))
                    .into_db();
            });
        }
    }

    if requires_test_context {
        let mut context_steps = Vec::new();
        context_steps.push(quote! {
            let mut __skyzen_test_context = ::skyzen_test::TestContext::new();
        });
        if requires_kv {
            context_steps.push(quote! {
                __skyzen_test_context = __skyzen_test_context.with_kv(__skyzen_test_kv.clone());
            });
        }
        if requires_storage {
            context_steps.push(quote! {
                __skyzen_test_context =
                    __skyzen_test_context.with_storage(__skyzen_test_storage.clone());
            });
        }
        if requires_queue {
            context_steps.push(quote! {
                __skyzen_test_context =
                    __skyzen_test_context.with_queue(__skyzen_test_queue.clone());
            });
        }
        if requires_default_db {
            context_steps.push(quote! {
                __skyzen_test_context = __skyzen_test_context.with_db(__skyzen_test_default_db.clone());
            });
        }
        setup_statements.extend(context_steps);
    }

    let mut inner_statements = setup_statements;
    inner_statements.extend(binding_statements);
    let original_body = function.block.stmts.clone();
    function.block.stmts = inner_statements
        .into_iter()
        .map(syn::parse2)
        .collect::<syn::Result<Vec<_>>>()?;
    function.block.stmts.extend(original_body);

    Ok(quote! {
        #function

        #(#outer_attrs)*
        #[test]
        #vis fn #original_ident() #output {
            ::skyzen::runtime::testing::block_on(async move { #inner_ident().await })
        }
    }
    .into())
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
        "Kv" => Ok(TestParamKind::Kv),
        "Storage" => Ok(TestParamKind::Storage),
        "Queue" => Ok(TestParamKind::Queue),
        "Db" => Ok(TestParamKind::Db),
        _ => database_types
            .iter()
            .find(|(_, type_ident)| *type_ident == ident)
            .map_or_else(
                || {
                    Err(Error::new_spanned(
                        ty,
                        "unsupported #[skyzen::test] parameter type; supported types are `TestContext`, `Kv`, `Storage`, `Queue`, `Db`, and generated database wrappers",
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
fn expand_error_struct(args: ErrorArgs, item_struct: ItemStruct) -> syn::Result<TokenStream> {
    let ident = &item_struct.ident;
    let generics = &item_struct.generics;
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

    Ok(quote! {
        #[derive(::core::fmt::Debug)]
        #item_struct

        impl #impl_generics ::core::fmt::Display for #ident #ty_generics #where_clause {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(#message)
            }
        }

        impl #impl_generics ::core::error::Error for #ident #ty_generics #where_clause {}

        impl #impl_generics ::skyzen::HttpError for #ident #ty_generics #where_clause {
            fn status(&self) -> ::skyzen::StatusCode {
                #status
            }
        }
    }
    .into())
}

fn expand_error_enum(args: ErrorArgs, mut item_enum: ItemEnum) -> syn::Result<TokenStream> {
    let ident = &item_enum.ident;
    let generics = &item_enum.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let ErrorArgs { status, .. } = args;
    let default_status =
        status.unwrap_or_else(|| parse_quote!(::skyzen::StatusCode::INTERNAL_SERVER_ERROR));

    let mut display_arms = Vec::new();
    let mut status_arms = Vec::new();
    let mut from_impls = Vec::new();
    let mut cleaned_variants = Punctuated::new();

    for variant in item_enum.variants {
        let variant_ident = variant.ident.clone();
        let (
            variant,
            VariantMeta {
                message,
                status,
                from,
            },
        ) = parse_variant(variant)?;

        let pattern = match &variant.fields {
            Fields::Unit => {
                let ident = &variant.ident;
                quote! { Self::#ident }
            }
            Fields::Unnamed(_) => {
                let ident = &variant.ident;
                quote! { Self::#ident ( .. ) }
            }
            Fields::Named(_) => {
                let ident = &variant.ident;
                quote! { Self::#ident { .. } }
            }
        };

        let status_expr = status.unwrap_or_else(|| default_status.clone());

        display_arms.push(quote! {
            #pattern => f.write_str(#message)
        });

        status_arms.push(quote! {
            #pattern => #status_expr
        });

        if let Some(from_info) = from {
            let binding = format_ident!("__skyzen_from");
            let ctor = match from_info.style {
                VariantFromStyle::Unnamed => {
                    quote! { Self::#variant_ident(#binding) }
                }
                VariantFromStyle::Named(field_ident) => {
                    quote! { Self::#variant_ident { #field_ident: #binding } }
                }
            };
            let ty = from_info.ty;
            from_impls.push(quote! {
                impl #impl_generics ::core::convert::From<#ty> for #ident #ty_generics #where_clause {
                    fn from(#binding: #ty) -> Self {
                        #ctor
                    }
                }
            });
        }

        cleaned_variants.push(variant);
    }

    item_enum.variants = cleaned_variants;

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

        impl #impl_generics ::core::error::Error for #ident #ty_generics #where_clause {}

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
            ))
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
    Ok(parse_quote! {
        ::skyzen::StatusCode::from_u16(#value)
            .expect("invalid HTTP status code literal")
    })
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
    from: Option<VariantFrom>,
}

struct VariantFrom {
    ty: Type,
    style: VariantFromStyle,
}

enum VariantFromStyle {
    Unnamed,
    Named(syn::Ident),
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
    meta.from = extract_variant_from(&mut variant.fields)?;

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
            from: None,
        })
    })
}

fn extract_variant_from(fields: &mut Fields) -> syn::Result<Option<VariantFrom>> {
    match fields {
        Fields::Unit => Ok(None),
        Fields::Unnamed(unnamed) => {
            let count = unnamed.unnamed.len();
            let mut info = None;
            for field in &mut unnamed.unnamed {
                if take_from_attr(&mut field.attrs)? {
                    if info.is_some() {
                        return Err(Error::new(field.ty.span(), "duplicate #[from] attribute"));
                    }
                    if count != 1 {
                        return Err(Error::new(
                            field.ty.span(),
                            "#[from] is only supported on tuple variants with a single field",
                        ));
                    }
                    info = Some(VariantFrom {
                        ty: field.ty.clone(),
                        style: VariantFromStyle::Unnamed,
                    });
                }
            }
            Ok(info)
        }
        Fields::Named(named) => {
            let count = named.named.len();
            let mut info = None;
            for field in &mut named.named {
                if take_from_attr(&mut field.attrs)? {
                    if info.is_some() {
                        return Err(Error::new(field.ty.span(), "duplicate #[from] attribute"));
                    }
                    if count != 1 {
                        return Err(Error::new(
                            field.ty.span(),
                            "#[from] is only supported on struct variants with a single field",
                        ));
                    }
                    let ident = field.ident.clone().ok_or_else(|| {
                        Error::new(field.ty.span(), "unnamed field in struct variant")
                    })?;
                    info = Some(VariantFrom {
                        ty: field.ty.clone(),
                        style: VariantFromStyle::Named(ident),
                    });
                }
            }
            Ok(info)
        }
    }
}

fn take_from_attr(attrs: &mut Vec<Attribute>) -> syn::Result<bool> {
    let mut found = false;
    let mut retained = Vec::new();
    for attr in attrs.drain(..) {
        if attr.path().is_ident("from") {
            if !matches!(attr.meta, Meta::Path(_)) {
                return Err(Error::new_spanned(attr, "#[from] does not take arguments"));
            }
            if found {
                return Err(Error::new(attr.span(), "duplicate #[from] attribute"));
            }
            found = true;
        } else {
            retained.push(attr);
        }
    }
    *attrs = retained;
    Ok(found)
}

fn doc_string(attrs: &[Attribute]) -> Option<String> {
    let mut docs = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }

        if let Meta::NameValue(meta) = &attr.meta {
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(lit), ..
            }) = &meta.value
            {
                docs.push(lit.value().trim().to_owned());
            }
        }
    }

    if docs.is_empty() {
        None
    } else {
        Some(docs.join("\n"))
    }
}

#[derive(Debug, Clone)]
struct DatasourceConfig {
    name: String,
    engine: String,
    strategy: String,
    url_env: String,
    key_env: Option<String>,
}

#[derive(Debug, Clone)]
struct ServiceConfig {
    name: String,
    service_type: ServiceType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ServiceType {
    Kv,
    Storage,
    Queue,
}

impl ServiceType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Storage => "storage",
            Self::Queue => "queue",
        }
    }
}

#[derive(Debug, Clone)]
struct DatabaseConfig {
    name: String,
    database_type: DatabaseType,
    default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DatabaseType {
    Sql,
}

impl DatabaseType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Sql => "sql",
        }
    }
}

#[allow(clippy::too_many_lines)]
fn expand_import_config() -> syn::Result<proc_macro2::TokenStream> {
    let datasources = load_datasources()?;
    let databases = if let Some(value) = load_manifest_value()? {
        load_databases_from_value(&value)?
    } else {
        Vec::new()
    };
    let mut generated_items = Vec::with_capacity(datasources.len());
    let mut seen_idents = HashSet::new();

    for datasource in datasources {
        let ident = ident_from_name(&datasource.name)?;
        if !seen_idents.insert(ident.to_string()) {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!("duplicate datasource type name after normalization: `{ident}`"),
            ));
        }

        let init_error_ident = format_ident!("{ident}InitError");
        let missing_ident = format_ident!("{ident}NotConfigured");

        let name_lit = LitStr::new(&datasource.name, proc_macro2::Span::call_site());
        let engine_lit = LitStr::new(&datasource.engine, proc_macro2::Span::call_site());
        let strategy_lit = LitStr::new(&datasource.strategy, proc_macro2::Span::call_site());
        let url_env_lit = LitStr::new(&datasource.url_env, proc_macro2::Span::call_site());
        let key_env_const = datasource.key_env.as_ref().map_or_else(
            || quote! { None },
            |value| {
                let lit = LitStr::new(value, proc_macro2::Span::call_site());
                quote! { Some(#lit) }
            },
        );
        let missing_message = LitStr::new(
            &format!(
                "{} not configured. Ensure {}::init() is called and middleware is installed.",
                datasource.name, datasource.name
            ),
            proc_macro2::Span::call_site(),
        );
        generated_items.push(quote! {
            #[derive(Debug, Clone)]
            pub struct #ident {
                url: ::std::sync::Arc<str>,
                key: ::std::option::Option<::std::sync::Arc<str>>,
            }

            #[derive(Debug, Clone)]
            pub enum #init_error_ident {
                MissingEnv(&'static str),
                EmptyEnv(&'static str),
            }

            impl ::std::fmt::Display for #init_error_ident {
                fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    match self {
                        Self::MissingEnv(key) => write!(f, "missing environment variable: {key}"),
                        Self::EmptyEnv(key) => write!(f, "environment variable is empty: {key}"),
                    }
                }
            }

            impl ::std::error::Error for #init_error_ident {}

            impl #ident {
                pub const NAME: &'static str = #name_lit;
                pub const ENGINE: &'static str = #engine_lit;
                pub const STRATEGY: &'static str = #strategy_lit;
                pub const URL_ENV: &'static str = #url_env_lit;
                pub const KEY_ENV: ::std::option::Option<&'static str> = #key_env_const;

                pub fn init() -> ::std::result::Result<Self, #init_error_ident> {
                    static INSTANCE: ::std::sync::OnceLock<
                        ::std::result::Result<#ident, #init_error_ident>
                    > = ::std::sync::OnceLock::new();
                    INSTANCE.get_or_init(|| {
                        let url = ::std::env::var(Self::URL_ENV)
                            .map_err(|_| #init_error_ident::MissingEnv(Self::URL_ENV))?;
                        if url.trim().is_empty() {
                            return Err(#init_error_ident::EmptyEnv(Self::URL_ENV));
                        }

                        let key = match Self::KEY_ENV {
                            Some(key_env) => {
                                let value = ::std::env::var(key_env)
                                    .map_err(|_| #init_error_ident::MissingEnv(key_env))?;
                                if value.trim().is_empty() {
                                    return Err(#init_error_ident::EmptyEnv(key_env));
                                }
                                Some(value.into())
                            }
                            None => None,
                        };

                        Ok(Self {
                            url: url.into(),
                            key,
                        })
                    }).clone()
                }

                #[must_use]
                pub fn url(&self) -> &str {
                    &self.url
                }

                #[must_use]
                pub fn key(&self) -> ::std::option::Option<&str> {
                    self.key.as_deref()
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
                type Error = ::std::convert::Infallible;

                async fn handle<N: ::skyzen::Endpoint>(
                    &mut self,
                    request: &mut ::skyzen::Request,
                    mut next: N,
                ) -> ::std::result::Result<
                    ::skyzen::Response,
                    ::skyzen::http_kit::middleware::MiddlewareError<N::Error, Self::Error>,
                > {
                    request.extensions_mut().insert(self.clone());
                    next.respond(request)
                        .await
                        .map_err(::skyzen::http_kit::middleware::MiddlewareError::Endpoint)
                }
            }

            #[cfg(test)]
            impl #ident {
                #[must_use]
                pub fn from_raw_parts(
                    url: impl Into<::std::sync::Arc<str>>,
                    key: ::std::option::Option<::std::sync::Arc<str>>,
                ) -> Self {
                    Self {
                        url: url.into(),
                        key,
                    }
                }
            }
        });
    }

    let mut seen_database_idents = HashSet::new();
    for database in databases {
        let ident = database_ident_from_name(&database.name)?;
        if !seen_database_idents.insert(ident.to_string()) {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!("duplicate database type name after normalization: `{ident}`"),
            ));
        }

        let missing_ident = format_ident!("{ident}NotConfigured");
        let display_name = database.name.clone();
        let display_message = LitStr::new(
            &format!(
                "database `{display_name}` not configured. Ensure Skyzen.toml database wiring is installed."
            ),
            proc_macro2::Span::call_site(),
        );

        generated_items.push(quote! {
            #[derive(Debug, Clone)]
            pub struct #ident(::skyzen_services::Db);

            impl #ident {
                #[must_use]
                pub const fn new(db: ::skyzen_services::Db) -> Self {
                    Self(db)
                }

                #[must_use]
                pub const fn inner(&self) -> &::skyzen_services::Db {
                    &self.0
                }

                #[must_use]
                pub fn into_inner(self) -> ::skyzen_services::Db {
                    self.0
                }
            }

            impl ::std::ops::Deref for #ident {
                type Target = ::skyzen_services::Db;

                fn deref(&self) -> &Self::Target {
                    &self.0
                }
            }

            ::skyzen::http_kit::http_error!(
                pub #missing_ident,
                ::skyzen::StatusCode::INTERNAL_SERVER_ERROR,
                #display_message
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
                type Error = ::std::convert::Infallible;

                async fn handle<N: ::skyzen::Endpoint>(
                    &mut self,
                    request: &mut ::skyzen::Request,
                    mut next: N,
                ) -> ::std::result::Result<
                    ::skyzen::Response,
                    ::skyzen::http_kit::middleware::MiddlewareError<N::Error, Self::Error>,
                > {
                    request.extensions_mut().insert(self.clone());
                    next.respond(request)
                        .await
                        .map_err(::skyzen::http_kit::middleware::MiddlewareError::Endpoint)
                }
            }
        });
    }

    Ok(quote! {
        #[doc(hidden)]
        pub mod __skyzen_config {
            #(#generated_items)*
        }

        pub use __skyzen_config::*;
    })
}

fn datasource_wrap_steps() -> syn::Result<Vec<proc_macro2::TokenStream>> {
    let datasources = load_datasources()?;
    let mut steps = Vec::with_capacity(datasources.len());
    let mut seen_idents = HashSet::new();

    for datasource in datasources {
        let ident = ident_from_name(&datasource.name)?;
        if !seen_idents.insert(ident.to_string()) {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!("duplicate datasource type name after normalization: `{ident}`"),
            ));
        }

        let panic_message = LitStr::new(
            &format!("failed to initialize datasource `{}`", datasource.name),
            proc_macro2::Span::call_site(),
        );

        steps.push(quote! {
            let endpoint = ::skyzen::__private::with_middleware(
                endpoint,
                #ident::init().unwrap_or_else(|error| panic!("{}: {error}", #panic_message)),
            );
        });
    }

    Ok(steps)
}

fn portable_injection_wrap_steps() -> syn::Result<Vec<proc_macro2::TokenStream>> {
    let Some(value) = load_manifest_value()? else {
        return Ok(Vec::new());
    };

    let services = load_services_from_value(&value)?;
    let databases = load_databases_from_value(&value)?;
    if services.is_empty() && databases.is_empty() {
        return Ok(Vec::new());
    }
    let default_database = default_database_index(&databases)?;

    let mut steps = Vec::with_capacity(services.len() + databases.len() + 1);
    let mut seen_service_types = HashSet::new();

    steps.push(quote! {
        #[cfg(target_arch = "wasm32")]
        let __skyzen_wasm_env = ::skyzen::runtime::wasm::current_env()
            .expect("WinterCG env not available during portable capability initialization");
    });

    for service in &services {
        if !seen_service_types.insert(service.service_type) {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "duplicate portable service type `{}` detected; Skyzen currently supports only one default `{}` capability",
                    service.service_type.as_str(),
                    service.service_type.as_str()
                ),
            ));
        }

        let native_init = generate_native_service_init(service, &value)?;
        let cloudflare_init = generate_cloudflare_service_init(service, &value)?;
        steps.push(quote! {
            let endpoint = {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let __svc = #native_init;
                    ::skyzen::__private::with_middleware(endpoint, __svc)
                }
                #[cfg(target_arch = "wasm32")]
                {
                    let __svc = #cloudflare_init;
                    ::skyzen::__private::with_middleware(endpoint, __svc)
                }
            };
        });
    }

    for (index, database) in databases.iter().enumerate() {
        let ident = database_ident_from_name(&database.name)?;
        let native_init = generate_native_database_init(database, &value)?;
        let cloudflare_init = generate_cloudflare_database_init(database, &value)?;
        let inject_default = default_database == Some(index);
        steps.push(quote! {
            let endpoint = {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let __db = #native_init;
                    let endpoint = ::skyzen::__private::with_middleware(endpoint, #ident::new(__db.clone()));
                    if #inject_default {
                        ::skyzen::__private::with_middleware(endpoint, __db)
                    } else {
                        endpoint
                    }
                }
                #[cfg(target_arch = "wasm32")]
                {
                    let __db = #cloudflare_init;
                    let endpoint = ::skyzen::__private::with_middleware(endpoint, #ident::new(__db.clone()));
                    if #inject_default {
                        ::skyzen::__private::with_middleware(endpoint, __db)
                    } else {
                        endpoint
                    }
                }
            };
        });
    }

    Ok(steps)
}

fn load_datasources() -> syn::Result<Vec<DatasourceConfig>> {
    let Some(value) = load_manifest_value()? else {
        return Ok(Vec::new());
    };

    let Some(entries) = value.get("datasource").and_then(toml::Value::as_array) else {
        return Ok(Vec::new());
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_datasource(entry, index))
        .collect()
}

fn parse_datasource(entry: &toml::Value, index: usize) -> syn::Result<DatasourceConfig> {
    let Some(table) = entry.as_table() else {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            format!("datasource[{index}] must be a TOML table"),
        ));
    };

    let label = format!("datasource[{index}]");
    let name = required_string(table, "name", &label)?;
    let engine = optional_string(table, &["engine"]).unwrap_or_else(|| "custom".to_owned());
    let strategy = optional_string(table, &["strategy"]).unwrap_or_else(|| "tcp".to_owned());
    let url_env = required_url_env(table, &label)?;
    let key_env = optional_string(
        table,
        &[
            "key_from_env",
            "auth_from_env",
            "secret_from_env",
            "password_from_env",
        ],
    );

    Ok(DatasourceConfig {
        name,
        engine,
        strategy,
        url_env,
        key_env,
    })
}

fn required_string(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    label: &str,
) -> syn::Result<String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::new(
                proc_macro2::Span::call_site(),
                format!("{label} is missing `{key}`"),
            )
        })
}

fn optional_string(table: &toml::map::Map<String, toml::Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| table.get(*key).and_then(toml::Value::as_str))
        .map(ToOwned::to_owned)
}

fn required_url_env(
    table: &toml::map::Map<String, toml::Value>,
    label: &str,
) -> syn::Result<String> {
    if let Some(env) = optional_string(table, &["url_from_env", "url_env", "url_env_var"]) {
        return Ok(env);
    }

    if let Some(url_value) = table.get("url") {
        if let Some(url_table) = url_value.as_table() {
            if let Some(env) = url_table.get("env").and_then(toml::Value::as_str) {
                return Ok(env.to_owned());
            }
        } else if let Some(url_str) = url_value.as_str() {
            if let Some(stripped) = parse_env_ref(url_str) {
                return Ok(stripped);
            }
            if looks_like_env_name(url_str) {
                return Ok(url_str.to_owned());
            }
        }
    }

    Err(Error::new(
        proc_macro2::Span::call_site(),
        format!(
            "{label} is missing URL env reference; use `url_from_env = \"ENV_KEY\"` \
or `url = {{ env = \"ENV_KEY\" }}`"
        ),
    ))
}

fn load_manifest_value() -> syn::Result<Option<toml::Value>> {
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

    let content = fs::read_to_string(&config_path).map_err(|error| {
        Error::new(
            proc_macro2::Span::call_site(),
            format!("failed to read {}: {error}", config_path.display()),
        )
    })?;
    let value: toml::Value = toml::from_str(&content).map_err(|error| {
        Error::new(
            proc_macro2::Span::call_site(),
            format!("failed to parse {}: {error}", config_path.display()),
        )
    })?;
    Ok(Some(value))
}

fn load_services_from_value(value: &toml::Value) -> syn::Result<Vec<ServiceConfig>> {
    let Some(entries) = value.get("service").and_then(toml::Value::as_array) else {
        return Ok(Vec::new());
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_service(entry, index))
        .collect()
}

fn parse_service(entry: &toml::Value, index: usize) -> syn::Result<ServiceConfig> {
    let Some(table) = entry.as_table() else {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            format!("service[{index}] must be a TOML table"),
        ));
    };

    let label = format!("service[{index}]");
    let name = required_string(table, "name", &label)?;
    let type_name = required_string(table, "type", &label)?;
    let service_type = match type_name.as_str() {
        "kv" => ServiceType::Kv,
        "storage" => ServiceType::Storage,
        "queue" => ServiceType::Queue,
        other => {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!("{label} has unsupported type `{other}`; expected kv|storage|queue"),
            ));
        }
    };

    Ok(ServiceConfig { name, service_type })
}

fn load_databases_from_value(value: &toml::Value) -> syn::Result<Vec<DatabaseConfig>> {
    let Some(entries) = value.get("database").and_then(toml::Value::as_array) else {
        return Ok(Vec::new());
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_database(entry, index))
        .collect()
}

fn parse_database(entry: &toml::Value, index: usize) -> syn::Result<DatabaseConfig> {
    let Some(table) = entry.as_table() else {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            format!("database[{index}] must be a TOML table"),
        ));
    };

    let label = format!("database[{index}]");
    let name = required_string(table, "name", &label)?;
    let type_name = required_string(table, "type", &label)?;
    let default = table
        .get("default")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let database_type = match type_name.as_str() {
        "sql" => DatabaseType::Sql,
        other => {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!("{label} has unsupported type `{other}`; expected sql"),
            ));
        }
    };

    Ok(DatabaseConfig {
        name,
        database_type,
        default,
    })
}

fn generate_native_service_init(
    service: &ServiceConfig,
    value: &toml::Value,
) -> syn::Result<proc_macro2::TokenStream> {
    let Some(table) = lookup_table(value, &["native", "service", service.name.as_str()]) else {
        return Ok(compile_error_block(&format!(
            "missing [native.service.{}] wiring for portable service `{}`",
            service.name, service.name
        )));
    };

    let label = format!("[native.service.{}]", service.name);
    let backend = required_string(table, "backend", &label)?;

    match (service.service_type, backend.as_str()) {
        (ServiceType::Kv, "redis") => {
            let env_key = required_string(table, "url_env", &label)?;
            let env_lit = LitStr::new(&env_key, proc_macro2::Span::call_site());
            let missing_message = LitStr::new(
                &format!(
                    "portable service `{}` missing native env var `{}`",
                    service.name, env_key
                ),
                proc_macro2::Span::call_site(),
            );
            let connect_message = LitStr::new(
                &format!(
                    "portable service `{}` failed to connect to Redis using `{}`",
                    service.name, env_key
                ),
                proc_macro2::Span::call_site(),
            );
            Ok(quote! {{
                let url = ::std::env::var(#env_lit)
                    .unwrap_or_else(|_| panic!("{}", #missing_message));
                let backend = ::skyzen_redis::Redis::connect(&url)
                    .await
                    .unwrap_or_else(|error| panic!("{}: {error}", #connect_message));
                ::skyzen_services::Kv::new(backend)
            }})
        }
        (ServiceType::Kv, "memory") => Ok(quote! {
            ::skyzen_services::Kv::new(::skyzen_test::mock::InMemoryKv::new())
        }),
        (ServiceType::Storage, "s3") => {
            let env_key = required_string(table, "bucket_env", &label)?;
            let env_lit = LitStr::new(&env_key, proc_macro2::Span::call_site());
            let missing_message = LitStr::new(
                &format!(
                    "portable service `{}` missing native env var `{}`",
                    service.name, env_key
                ),
                proc_macro2::Span::call_site(),
            );
            Ok(quote! {{
                let bucket = ::std::env::var(#env_lit)
                    .unwrap_or_else(|_| panic!("{}", #missing_message));
                let backend = ::skyzen_s3::S3Storage::from_env(&bucket).await;
                ::skyzen_services::Storage::new(backend)
            }})
        }
        (ServiceType::Storage, "memory") => Ok(quote! {
            ::skyzen_services::Storage::new(::skyzen_test::mock::InMemoryStorage::new())
        }),
        (ServiceType::Queue, "sqs") => {
            let env_key = required_string(table, "url_env", &label)?;
            let env_lit = LitStr::new(&env_key, proc_macro2::Span::call_site());
            let missing_message = LitStr::new(
                &format!(
                    "portable service `{}` missing native env var `{}`",
                    service.name, env_key
                ),
                proc_macro2::Span::call_site(),
            );
            Ok(quote! {{
                let url = ::std::env::var(#env_lit)
                    .unwrap_or_else(|_| panic!("{}", #missing_message));
                let backend = ::skyzen_aws::SqsQueue::from_env(&url).await;
                ::skyzen_services::Queue::new(backend)
            }})
        }
        (ServiceType::Queue, "memory") => Ok(quote! {
            ::skyzen_services::Queue::new(::skyzen_test::mock::InMemoryQueue::new())
        }),
        (service_type, other) => Ok(compile_error_block(&format!(
            "unsupported native backend `{other}` for portable service `{}` of type `{}`",
            service.name,
            service_type.as_str()
        ))),
    }
}

fn generate_cloudflare_service_init(
    service: &ServiceConfig,
    value: &toml::Value,
) -> syn::Result<proc_macro2::TokenStream> {
    let Some(table) = lookup_table(value, &["cloudflare", "service", service.name.as_str()]) else {
        return Ok(compile_error_block(&format!(
            "missing [cloudflare.service.{}] wiring for portable service `{}`",
            service.name, service.name
        )));
    };

    let label = format!("[cloudflare.service.{}]", service.name);
    let binding = required_string(table, "binding", &label)?;
    let binding_lit = LitStr::new(&binding, proc_macro2::Span::call_site());
    let failure_message = LitStr::new(
        &format!(
            "portable service `{}` failed to resolve Cloudflare binding `{}`",
            service.name, binding
        ),
        proc_macro2::Span::call_site(),
    );

    match service.service_type {
        ServiceType::Kv => Ok(quote! {{
            let backend = ::skyzen_cloudflare::CfKv::from_env(&__skyzen_wasm_env, #binding_lit)
                .unwrap_or_else(|error| panic!("{}: {error}", #failure_message));
            ::skyzen_services::Kv::new(backend)
        }}),
        ServiceType::Storage => Ok(quote! {{
            let backend = ::skyzen_cloudflare::CfR2::from_env(&__skyzen_wasm_env, #binding_lit)
                .unwrap_or_else(|error| panic!("{}: {error}", #failure_message));
            ::skyzen_services::Storage::new(backend)
        }}),
        ServiceType::Queue => Ok(quote! {{
            let backend = ::skyzen_cloudflare::CfQueue::from_env(&__skyzen_wasm_env, #binding_lit)
                .unwrap_or_else(|error| panic!("{}: {error}", #failure_message));
            ::skyzen_services::Queue::new(backend)
        }}),
    }
}

fn generate_native_database_init(
    database: &DatabaseConfig,
    value: &toml::Value,
) -> syn::Result<proc_macro2::TokenStream> {
    let Some(table) = lookup_table(value, &["native", "database", database.name.as_str()]) else {
        return Ok(compile_error_block(&format!(
            "missing [native.database.{}] wiring for portable database `{}`",
            database.name, database.name
        )));
    };

    let label = format!("[native.database.{}]", database.name);
    let backend = required_string(table, "backend", &label)?;
    let url_env = required_string(table, "url_env", &label)?;

    match (database.database_type, backend.as_str()) {
        (DatabaseType::Sql, "postgres") => {
            let env_lit = LitStr::new(&url_env, proc_macro2::Span::call_site());
            let missing_message = LitStr::new(
                &format!(
                    "portable database `{}` missing native env var `{}`",
                    database.name, url_env
                ),
                proc_macro2::Span::call_site(),
            );
            let connect_message = LitStr::new(
                &format!(
                    "portable database `{}` failed to connect using `{}`",
                    database.name, url_env
                ),
                proc_macro2::Span::call_site(),
            );
            Ok(quote! {{
                let url = ::std::env::var(#env_lit)
                    .unwrap_or_else(|_| panic!("{}", #missing_message));
                ::skyzen_services::Db::connect_postgres(&url)
                    .await
                    .unwrap_or_else(|error| panic!("{}: {error}", #connect_message));
            }})
        }
        (DatabaseType::Sql, "mysql") => {
            let env_lit = LitStr::new(&url_env, proc_macro2::Span::call_site());
            let missing_message = LitStr::new(
                &format!(
                    "portable database `{}` missing native env var `{}`",
                    database.name, url_env
                ),
                proc_macro2::Span::call_site(),
            );
            let connect_message = LitStr::new(
                &format!(
                    "portable database `{}` failed to connect using `{}`",
                    database.name, url_env
                ),
                proc_macro2::Span::call_site(),
            );
            Ok(quote! {{
                let url = ::std::env::var(#env_lit)
                    .unwrap_or_else(|_| panic!("{}", #missing_message));
                ::skyzen_services::Db::connect_mysql(&url)
                    .await
                    .unwrap_or_else(|error| panic!("{}: {error}", #connect_message))
            }})
        }
        (DatabaseType::Sql, "sqlite") => {
            let env_lit = LitStr::new(&url_env, proc_macro2::Span::call_site());
            let missing_message = LitStr::new(
                &format!(
                    "portable database `{}` missing native env var `{}`",
                    database.name, url_env
                ),
                proc_macro2::Span::call_site(),
            );
            let connect_message = LitStr::new(
                &format!(
                    "portable database `{}` failed to connect using `{}`",
                    database.name, url_env
                ),
                proc_macro2::Span::call_site(),
            );
            Ok(quote! {{
                let url = ::std::env::var(#env_lit)
                    .unwrap_or_else(|_| panic!("{}", #missing_message));
                ::skyzen_services::Db::connect_sqlite(&url)
                    .await
                    .unwrap_or_else(|error| panic!("{}: {error}", #connect_message))
            }})
        }
        (database_type, other) => Ok(compile_error_block(&format!(
            "unsupported native backend `{other}` for portable database `{}` of type `{}`",
            database.name,
            database_type.as_str()
        ))),
    }
}

fn generate_cloudflare_database_init(
    database: &DatabaseConfig,
    value: &toml::Value,
) -> syn::Result<proc_macro2::TokenStream> {
    let Some(table) = lookup_table(value, &["cloudflare", "database", database.name.as_str()])
    else {
        return Ok(compile_error_block(&format!(
            "missing [cloudflare.database.{}] wiring for portable database `{}`",
            database.name, database.name
        )));
    };

    let label = format!("[cloudflare.database.{}]", database.name);
    let binding = required_string(table, "binding", &label)?;
    let binding_lit = LitStr::new(&binding, proc_macro2::Span::call_site());
    let failure_message = LitStr::new(
        &format!(
            "portable database `{}` failed to resolve Cloudflare binding `{}`",
            database.name, binding
        ),
        proc_macro2::Span::call_site(),
    );

    match database.database_type {
        DatabaseType::Sql => Ok(quote! {{
            let backend = ::skyzen_cloudflare::CfD1::from_env(&__skyzen_wasm_env, #binding_lit)
                .unwrap_or_else(|error| panic!("{}: {error}", #failure_message));
            ::skyzen_services::Db::new(backend)
        }}),
    }
}

fn lookup_table<'a>(
    value: &'a toml::Value,
    path: &[&str],
) -> Option<&'a toml::map::Map<String, toml::Value>> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_table()
}

fn compile_error_block(message: &str) -> proc_macro2::TokenStream {
    let message = LitStr::new(message, proc_macro2::Span::call_site());
    quote! {{
        compile_error!(#message);
        unreachable!()
    }}
}

fn parse_env_ref(value: &str) -> Option<String> {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .map(ToOwned::to_owned)
}

fn looks_like_env_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn ident_from_name(name: &str) -> syn::Result<proc_macro2::Ident> {
    let mut normalized = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            normalized.push(ch);
        } else {
            normalized.push('_');
        }
    }

    if normalized.is_empty() {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            "datasource name must contain at least one alphanumeric character",
        ));
    }

    if normalized
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit())
    {
        normalized.insert(0, '_');
    }

    Ok(format_ident!("{normalized}"))
}

fn database_ident_from_name(name: &str) -> syn::Result<proc_macro2::Ident> {
    let mut normalized = String::with_capacity(name.len() + 2);
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
            "database name must contain at least one alphanumeric character",
        ));
    }

    normalized.push_str("Db");
    Ok(format_ident!("{normalized}"))
}

fn default_database_index(databases: &[DatabaseConfig]) -> syn::Result<Option<usize>> {
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
}

impl MainOptions {
    fn from_args(args: &Punctuated<MetaNameValue, Token![,]>) -> syn::Result<Self> {
        let mut options = Self {
            default_logger: true,
        };

        for meta in args {
            if !meta.path.is_ident("default_logger") {
                return Err(Error::new_spanned(
                    &meta.path,
                    "unsupported option, expected `default_logger = true|false`",
                ));
            }

            let value = match &meta.value {
                Expr::Lit(ExprLit {
                    lit: Lit::Bool(bool_lit),
                    ..
                }) => bool_lit.value,
                other => {
                    return Err(Error::new_spanned(other, "expected boolean literal"));
                }
            };
            options.default_logger = value;
        }

        Ok(options)
    }
}

#[derive(Clone, Copy)]
enum CloudflareEventKind {
    Queue,
    Scheduled,
}

fn expand_cloudflare_event(
    mut function: ItemFn,
    kind: CloudflareEventKind,
) -> syn::Result<proc_macro2::TokenStream> {
    let is_async = function.sig.asyncness.is_some();
    let original_ident = function.sig.ident.clone();
    let internal_ident = match kind {
        CloudflareEventKind::Queue if original_ident == "queue" => {
            format_ident!("__skyzen_entry_queue")
        }
        CloudflareEventKind::Scheduled if original_ident == "scheduled" => {
            format_ident!("__skyzen_entry_scheduled")
        }
        _ => original_ident,
    };
    function.sig.ident = internal_ident.clone();

    let wrapper_ident = match kind {
        CloudflareEventKind::Queue => format_ident!("queue"),
        CloudflareEventKind::Scheduled => format_ident!("scheduled"),
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
        CloudflareEventKind::Scheduled => {
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
        CloudflareEventKind::Queue => Err(Error::new_spanned(
            ty,
            "the first #[skyzen::queue] argument must be `CfQueueBatch` or `QueueBatch<T>`",
        )),
        CloudflareEventKind::Scheduled => Err(Error::new_spanned(
            ty,
            "the first #[skyzen::scheduled] argument must be `CfScheduledEvent` or `ScheduledTick`",
        )),
    }
}

fn context_argument_expr(
    ty: &Type,
    kind: CloudflareEventKind,
) -> syn::Result<proc_macro2::TokenStream> {
    let ident = last_type_ident(ty)?;
    match (kind, ident.as_str()) {
        (CloudflareEventKind::Queue, "CfQueueContext") => {
            Ok(quote! { ::skyzen_cloudflare::CfQueueContext::new(__skyzen_raw_ctx) })
        }
        (CloudflareEventKind::Scheduled, "CfScheduleContext") => {
            Ok(quote! { ::skyzen_cloudflare::CfScheduleContext::new(__skyzen_raw_ctx) })
        }
        (CloudflareEventKind::Queue, _) => Err(Error::new_spanned(
            ty,
            "the third #[skyzen::queue] argument must be `CfQueueContext`",
        )),
        (CloudflareEventKind::Scheduled, _) => Err(Error::new_spanned(
            ty,
            "the third #[skyzen::scheduled] argument must be `CfScheduleContext`",
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
                        let code = u16::try_from(code)
                            .expect("Cloudflare websocket close code must fit within u16");
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
        database_ident_from_name, default_database_index, documented_extractor_payload,
        documented_response_payload, first_generic_type, generate_cloudflare_database_init,
        generate_native_service_init, ident_from_name, load_databases_from_value,
        load_services_from_value, lookup_table, looks_like_env_name, parse_env_ref,
        single_generic_type, DatabaseConfig, DatabaseType, ServiceType,
    };
    use quote::ToTokens;
    use syn::parse_quote;

    #[test]
    fn parses_portable_services_and_databases() {
        let value: toml::Value = toml::from_str(
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
        )
        .expect("valid toml");

        let services = load_services_from_value(&value).expect("services");
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name, "cache");
        assert_eq!(services[0].service_type, ServiceType::Kv);
        assert_eq!(services[1].service_type, ServiceType::Storage);

        let databases = load_databases_from_value(&value).expect("databases");
        assert_eq!(databases.len(), 1);
        assert_eq!(databases[0].name, "main");
        assert_eq!(databases[0].database_type, DatabaseType::Sql);
        assert!(!databases[0].default);
    }

    #[test]
    fn locates_named_wiring_tables() {
        let value: toml::Value = toml::from_str(
            r#"[native.service.cache]
backend = "redis"
url_env = "CACHE_URL"

[cloudflare.service.cache]
binding = "CACHE"

[native.database.main]
backend = "postgres"
url_env = "DATABASE_URL"
"#,
        )
        .expect("valid toml");

        let native_cache =
            lookup_table(&value, &["native", "service", "cache"]).expect("native cache section");
        assert_eq!(
            native_cache
                .get("backend")
                .and_then(toml::Value::as_str)
                .expect("backend"),
            "redis"
        );

        let cloudflare_cache = lookup_table(&value, &["cloudflare", "service", "cache"])
            .expect("cloudflare cache section");
        assert_eq!(
            cloudflare_cache
                .get("binding")
                .and_then(toml::Value::as_str)
                .expect("binding"),
            "CACHE"
        );

        let native_database =
            lookup_table(&value, &["native", "database", "main"]).expect("native db section");
        assert_eq!(
            native_database
                .get("url_env")
                .and_then(toml::Value::as_str)
                .expect("url_env"),
            "DATABASE_URL"
        );
    }

    #[test]
    fn generates_portable_wrapper_inits() {
        let value: toml::Value = toml::from_str(
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
        )
        .expect("valid toml");

        let services = load_services_from_value(&value).expect("services");
        let databases = load_databases_from_value(&value).expect("databases");

        let native_service = generate_native_service_init(&services[0], &value)
            .expect("native service init")
            .to_string();
        assert!(native_service.contains("skyzen_services :: Kv :: new"));
        assert!(native_service.contains("skyzen_redis :: Redis :: connect"));

        let cloudflare_database = generate_cloudflare_database_init(&databases[0], &value)
            .expect("cloudflare database init")
            .to_string();
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

        let (json_inner, json_content_type) =
            documented_extractor_payload(&json_ty).unwrap().expect("json payload");
        assert_eq!(json_inner.into_token_stream().to_string(), "CreateWidget");
        assert_eq!(json_content_type, "application/json");

        let (form_inner, form_content_type) =
            documented_extractor_payload(&form_ty).unwrap().expect("form payload");
        assert_eq!(form_inner.into_token_stream().to_string(), "LoginForm");
        assert_eq!(form_content_type, "application/x-www-form-urlencoded");

        let (query_inner, query_content_type) =
            documented_extractor_payload(&query_ty).unwrap().expect("query payload");
        assert_eq!(query_inner.into_token_stream().to_string(), "SearchParams");
        assert_eq!(query_content_type, "application/x-www-form-urlencoded");

        let (result_inner, result_content_type) =
            documented_response_payload(&result_ty).unwrap().expect("result payload");
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

        let error = match single_generic_type(&result_ty) {
            Ok(_) => panic!("expected Result<Job, Error> to be rejected"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("expected a single generic argument"));

        let first_inner = first_generic_type(&result_ty).expect("first generic");
        assert_eq!(first_inner.into_token_stream().to_string(), "Job");
        assert!(first_generic_type(&plain_ty).is_none());
    }

    #[test]
    fn default_database_index_enforces_default_selection_rules() {
        let single = vec![DatabaseConfig {
            name: "main".to_owned(),
            database_type: DatabaseType::Sql,
            default: false,
        }];
        assert_eq!(default_database_index(&single).unwrap(), Some(0));

        let multiple_missing_default = vec![
            DatabaseConfig {
                name: "main".to_owned(),
                database_type: DatabaseType::Sql,
                default: false,
            },
            DatabaseConfig {
                name: "analytics".to_owned(),
                database_type: DatabaseType::Sql,
                default: false,
            },
        ];
        assert!(default_database_index(&multiple_missing_default)
            .unwrap_err()
            .to_string()
            .contains("require exactly one `default = true`"));

        let multiple_defaults = vec![
            DatabaseConfig {
                name: "main".to_owned(),
                database_type: DatabaseType::Sql,
                default: true,
            },
            DatabaseConfig {
                name: "analytics".to_owned(),
                database_type: DatabaseType::Sql,
                default: true,
            },
        ];
        assert!(default_database_index(&multiple_defaults)
            .unwrap_err()
            .to_string()
            .contains("cannot mark more than one database as `default = true`"));
    }

    #[test]
    fn env_reference_helpers_and_identifier_normalization_are_stable() {
        assert_eq!(parse_env_ref("${DATABASE_URL}"), Some("DATABASE_URL".to_owned()));
        assert_eq!(parse_env_ref("DATABASE_URL"), None);
        assert!(looks_like_env_name("DATABASE_URL_2"));
        assert!(!looks_like_env_name("database-url"));

        assert_eq!(ident_from_name("cache-service").unwrap().to_string(), "cache_service");
        assert_eq!(ident_from_name("9cache").unwrap().to_string(), "_9cache");
        assert_eq!(database_ident_from_name("primary").unwrap().to_string(), "PrimaryDb");
    }
}
