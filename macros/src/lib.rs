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
    let native_factory = quote! {
        async move {
            let endpoint = #entry_call;
            #(#datasource_wrap_steps)*
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

    let parameter_types: Vec<_> = parameter_schemas
        .iter()
        .map(|meta| meta.ty.clone())
        .collect();

    let assertions: Vec<_> = parameter_types
        .iter()
        .map(|ty| quote! { let _ = ::skyzen::openapi::extractor_schema_of::<#ty>; })
        .collect();

    let response_assert =
        quote! { let _ = ::skyzen::openapi::responder_schemas_of::<#response_ty>; };

    let mut parameter_schema_fns = Vec::new();
    let mut parameter_name_lists = Vec::new();
    for (included_idx, meta) in parameter_schemas.iter().enumerate() {
        let ty = &meta.ty;
        parameter_schema_fns.push(quote! { ::skyzen::openapi::extractor_schema_of::<#ty> });
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

    let response_schema_fn =
        quote! { Some(::skyzen::openapi::responder_schemas_of::<#response_ty>) };

    let mut schema_collector_idents = Vec::new();
    let mut schema_collector_defs = Vec::new();
    for (idx, ty) in parameter_types.iter().enumerate() {
        let ident = format_ident!(
            "__SKYZEN_OPENAPI_SCHEMAS_{}_{}",
            fn_ident.to_string().to_uppercase(),
            idx
        );
        schema_collector_idents.push(ident.clone());
        schema_collector_defs.push(quote! {
            fn #ident(schemas: &mut ::std::collections::BTreeMap<String, ::skyzen::openapi::SchemaRef>) {
                ::skyzen::openapi::register_extractor_schemas_for::<#ty>(schemas);
            }
        });
    }

    let response_collector_ident = format_ident!(
        "__SKYZEN_OPENAPI_SCHEMAS_{}_RESP",
        fn_ident.to_string().to_uppercase()
    );
    schema_collector_idents.push(response_collector_ident.clone());
    schema_collector_defs.push(quote! {
        fn #response_collector_ident(
            schemas: &mut ::std::collections::BTreeMap<String, ::skyzen::openapi::SchemaRef>
        ) {
            ::skyzen::openapi::register_responder_schemas_for::<#response_ty>(schemas);
        }
    });

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

        const _: fn() = || {
            #(#assertions)*
            #response_assert
        };

        #(#schema_collector_defs)*

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

#[allow(clippy::too_many_lines)]
fn expand_import_config() -> syn::Result<proc_macro2::TokenStream> {
    let datasources = load_datasources()?;
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

fn load_datasources() -> syn::Result<Vec<DatasourceConfig>> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|error| {
        Error::new(
            proc_macro2::Span::call_site(),
            format!("failed to read CARGO_MANIFEST_DIR: {error}"),
        )
    })?;
    let config_path = PathBuf::from(manifest_dir).join("Skyzen.toml");
    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&config_path).map_err(|error| {
        Error::new(
            proc_macro2::Span::call_site(),
            format!("failed to read {}: {error}", config_path.display()),
        )
    })?;
    let value: toml::Value = content.parse().map_err(|error| {
        Error::new(
            proc_macro2::Span::call_site(),
            format!("failed to parse {}: {error}", config_path.display()),
        )
    })?;

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

    let name = required_string(table, "name", index)?;
    let engine = optional_string(table, &["engine"]).unwrap_or_else(|| "custom".to_owned());
    let strategy = optional_string(table, &["strategy"]).unwrap_or_else(|| "tcp".to_owned());
    let url_env = required_url_env(table, index)?;
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
    index: usize,
) -> syn::Result<String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::new(
                proc_macro2::Span::call_site(),
                format!("datasource[{index}] is missing `{key}`"),
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
    index: usize,
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
            "datasource[{index}] is missing URL env reference; use `url_from_env = \"ENV_KEY\"` \
or `url = {{ env = \"ENV_KEY\" }}`"
        ),
    ))
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
