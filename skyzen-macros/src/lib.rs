//! Procedural macros for the Skyzen framework.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, parse_quote,
    punctuated::Punctuated,
    spanned::Spanned,
    Attribute, Data, DeriveInput, Error, Expr, ExprLit, Fields, FnArg, Item, ItemEnum, ItemFn,
    ItemStruct, Lit, LitInt, LitStr, Meta, MetaNameValue, Path, ReturnType, Token, Type, Variant,
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

    let native_factory = if is_async {
        quote! { #entry_ident() }
    } else {
        quote! { async move { #entry_ident() } }
    };
    let wasm_factory = native_factory.clone();

    let init_logging = if options.default_logger {
        quote! { ::skyzen::runtime::native::init_logging(); }
    } else {
        quote! {}
    };

    let output = quote! {
        #function

        #[cfg(not(target_arch = "wasm32"))]
        fn main() {
            #init_logging
            ::skyzen::runtime::native::apply_cli_overrides(::std::env::args());
            ::log::info!("Skyzen application starting up");
            ::skyzen::runtime::native::launch(|| #native_factory);
        }

        #[cfg(target_arch = "wasm32")]
        #[wasm_bindgen::prelude::wasm_bindgen]
        pub async fn fetch(
            request: ::skyzen::runtime::wasm::Request,
            env: ::skyzen::runtime::wasm::Env,
            ctx: ::skyzen::runtime::wasm::ExecutionContext,
        ) -> Result<::skyzen::runtime::wasm::Response, wasm_bindgen::JsValue> {
            ::skyzen::runtime::wasm::launch(|| #wasm_factory, request, env, ctx).await
        }
    };

    output.into()
}

/// Annotate handlers that should appear in generated `OpenAPI` documentation or mark types for
/// schema generation.
#[proc_macro_attribute]
pub fn openapi(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as Item);
    match item {
        Item::Fn(function) => match expand_openapi_fn(&function) {
            Ok(tokens) => tokens,
            Err(error) => error.to_compile_error().into(),
        },
        Item::Struct(struct_item) => match expand_openapi_schema_item(struct_item) {
            Ok(tokens) => tokens,
            Err(error) => error.to_compile_error().into(),
        },
        Item::Enum(enum_item) => match expand_openapi_schema_enum(enum_item) {
            Ok(tokens) => tokens,
            Err(error) => error.to_compile_error().into(),
        },
        other => Error::new_spanned(
            other,
            "#[skyzen::openapi] may only be applied to functions, structs, or enums",
        )
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

fn expand_openapi_fn(function: &ItemFn) -> syn::Result<TokenStream> {
    let fn_ident = &function.sig.ident;

    let doc = doc_string(&function.attrs);
    let doc_tokens = doc.as_deref().map_or_else(
        || quote! { None },
        |docs| {
            let lit = Lit::Str(syn::LitStr::new(docs, fn_ident.span()));
            quote! { Some(#lit) }
        },
    );

    let mut param_types = Vec::new();
    for input in &function.sig.inputs {
        match input {
            FnArg::Receiver(receiver) => {
                return Err(Error::new_spanned(
                    receiver,
                    "handlers annotated with #[skyzen::openapi] cannot take self arguments",
                ));
            }
            FnArg::Typed(pat_type) => {
                param_types.push((*pat_type.ty).clone());
            }
        }
    }

    let raw_response_ty = match &function.sig.output {
        ReturnType::Type(_, ty) => (*ty).clone(),
        ReturnType::Default => parse_quote!(()),
    };
    let response_ty = unwrap_result_type(&raw_response_ty);

    let assertions = param_types
        .iter()
        .map(|ty| quote! { ::skyzen::openapi::assert_schema::<#ty>(); });

    let response_assert = quote! { ::skyzen::openapi::assert_schema::<#response_ty>(); };

    let schema_array = if param_types.is_empty() {
        quote! { &[] }
    } else {
        let schema_fns = param_types.iter().map(|ty| {
            quote! { ::skyzen::openapi::schema_of::<#ty> }
        });
        quote! { &[#(#schema_fns),*] }
    };

    let response_schema_fn = quote! { Some(::skyzen::openapi::schema_of::<#response_ty>) };

    let type_name_literal = quote! { concat!(module_path!(), "::", stringify!(#fn_ident)) };
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

        #[cfg(debug_assertions)]
        #[::skyzen::openapi::distributed_slice(::skyzen::openapi::HANDLER_SPECS)]
        static #spec_ident: ::skyzen::openapi::HandlerSpec = ::skyzen::openapi::HandlerSpec {
            type_name: #type_name_literal,
            docs: #doc_tokens,
            parameters: #schema_array,
            response: #response_schema_fn,
        };
    }
    .into())
}

fn expand_openapi_schema_item(mut item_struct: ItemStruct) -> syn::Result<TokenStream> {
    ensure_to_schema(&mut item_struct.attrs);
    let ident = &item_struct.ident;
    let (impl_generics, ty_generics, where_clause) = item_struct.generics.split_for_impl();

    Ok(quote! {
        #item_struct

        impl #impl_generics ::skyzen::openapi::OpenApiSchema for #ident #ty_generics #where_clause {
            fn schema() -> ::core::option::Option<::skyzen::openapi::SchemaRef> {
                ::core::option::Option::Some(<Self as ::skyzen::PartialSchema>::schema())
            }
        }
    }
    .into())
}

fn expand_openapi_schema_enum(mut item_enum: ItemEnum) -> syn::Result<TokenStream> {
    ensure_to_schema(&mut item_enum.attrs);
    let ident = &item_enum.ident;
    let (impl_generics, ty_generics, where_clause) = item_enum.generics.split_for_impl();

    Ok(quote! {
        #item_enum

        impl #impl_generics ::skyzen::openapi::OpenApiSchema for #ident #ty_generics #where_clause {
            fn schema() -> ::core::option::Option<::skyzen::openapi::SchemaRef> {
                ::core::option::Option::Some(<Self as ::skyzen::PartialSchema>::schema())
            }
        }
    }
    .into())
}

fn ensure_to_schema(attrs: &mut Vec<Attribute>) {
    if has_to_schema(attrs) {
        return;
    }
    attrs.push(parse_quote!(#[derive(::utoipa::ToSchema)]));
}

fn has_to_schema(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("derive") {
            return false;
        }
        attr.parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)
            .map(|paths| {
                paths.iter().any(|path| {
                    path.segments
                        .last()
                        .map(|seg| seg.ident == "ToSchema")
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
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
            fn status(&self) -> ::core::option::Option<::skyzen::StatusCode> {
                Some(#status)
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

    for variant in item_enum.variants.into_iter() {
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
            #pattern => ::core::option::Option::Some(#status_expr)
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
            fn status(&self) -> ::core::option::Option<::skyzen::StatusCode> {
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

    for attr in variant.attrs.into_iter() {
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

fn unwrap_result_type(ty: &syn::Type) -> syn::Type {
    if let syn::Type::Path(type_path) = ty {
        if type_path.qself.is_none() {
            if let Some(segment) = type_path.path.segments.last() {
                if segment.ident == "Result" {
                    if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                        if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                            return inner.clone();
                        }
                    }
                }
            }
        }
    }
    ty.clone()
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
