//! Procedural macros for the Skyzen framework.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, parse_quote,
    punctuated::Punctuated,
    Attribute, DeriveInput, Error, Expr, ExprLit, FnArg, Item, ItemFn, Lit, LitStr, Meta,
    MetaNameValue, ReturnType, Token,
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

/// Annotate handlers that should appear in generated `OpenAPI` documentation.
#[proc_macro_attribute]
pub fn openapi(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match expand_openapi(&function) {
        Ok(tokens) => tokens,
        Err(error) => error.to_compile_error().into(),
    }
}

/// Define a lightweight error type with documentation and optional status code.
#[proc_macro_attribute]
pub fn error(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as ErrorArgs);
    let item = parse_macro_input!(item as Item);
    match expand_error(args, item) {
        Ok(tokens) => tokens,
        Err(error) => error.to_compile_error().into(),
    }
}

/// Derive helper for bridging `utoipa::ToSchema` into `OpenApiSchema`.
#[proc_macro_derive(OpenApiSchema)]
pub fn derive_openapi_schema(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    match expand_openapi_schema(input) {
        Ok(tokens) => tokens,
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_openapi(function: &ItemFn) -> syn::Result<TokenStream> {
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

    let assertions = param_types.iter().map(|ty| {
        quote! { ::skyzen::openapi::assert_schema::<#ty>(); }
    });

    let response_assert = quote! {
        ::skyzen::openapi::assert_schema::<#response_ty>();
    };

    let schema_array = if param_types.is_empty() {
        quote! { &[] }
    } else {
        let schema_fns = param_types.iter().map(|ty| {
            quote! { ::skyzen::openapi::schema_of::<#ty> }
        });
        quote! { &[#(#schema_fns),*] }
    };

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
        #[linkme::distributed_slice(::skyzen::openapi::HANDLER_SPECS)]
        static #spec_ident: ::skyzen::openapi::HandlerSpec = ::skyzen::openapi::HandlerSpec {
            type_name: #type_name_literal,
            docs: #doc_tokens,
            parameters: #schema_array,
            response: ::skyzen::openapi::schema_of::<#response_ty>,
        };
    }
    .into())
}

fn expand_error(args: ErrorArgs, item: Item) -> syn::Result<TokenStream> {
    let message = args.message.clone().ok_or_else(|| {
        Error::new(
            proc_macro2::Span::call_site(),
            "missing `message = \"...\"`",
        )
    })?;

    match item {
        Item::Struct(item_struct) => {
            let ident = &item_struct.ident;
            let generics = &item_struct.generics;
            let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

            let display_impl = quote! {
                impl #impl_generics ::core::fmt::Display for #ident #ty_generics #where_clause {
                    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                        f.write_str(#message)
                    }
                }
            };

            let error_impl = quote! {
                impl #impl_generics ::core::error::Error for #ident #ty_generics #where_clause {}
            };

            let status_impl = args.status.map(|status| {
                quote! {
                    impl #impl_generics ::skyzen::HttpError for #ident #ty_generics #where_clause {
                        fn status(&self) -> ::skyzen::StatusCode {
                            #status
                        }
                    }
                }
            });

            Ok(quote! {
                #item_struct
                #display_impl
                #error_impl
                #status_impl
            }
            .into())
        }
        Item::Enum(item_enum) => {
            let ident = &item_enum.ident;
            let generics = &item_enum.generics;
            let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

            let display_impl = quote! {
                impl #impl_generics ::core::fmt::Display for #ident #ty_generics #where_clause {
                    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                        f.write_str(#message)
                    }
                }
            };

            let error_impl = quote! {
                impl #impl_generics ::core::error::Error for #ident #ty_generics #where_clause {}
            };

            let status_impl = args.status.map(|status| {
                quote! {
                    impl #impl_generics ::skyzen::HttpError for #ident #ty_generics #where_clause {
                        fn status(&self) -> ::skyzen::StatusCode {
                            #status
                        }
                    }
                }
            });

            Ok(quote! {
                #item_enum
                #display_impl
                #error_impl
                #status_impl
            }
            .into())
        }
        other => Err(Error::new_spanned(
            other,
            "#[skyzen::error] can only be applied to structs or enums",
        )),
    }
}

#[derive(Default)]
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
                        return Err(Error::new(key.span(), "duplicate `status`"));
                    }
                    args.status = Some(input.parse()?);
                }
                "message" => {
                    if args.message.is_some() {
                        return Err(Error::new(key.span(), "duplicate `message`"));
                    }
                    args.message = Some(input.parse()?);
                }
                other => {
                    return Err(Error::new(
                        key.span(),
                        format!("unsupported #[error] argument `{other}`"),
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

fn expand_openapi_schema(input: DeriveInput) -> syn::Result<TokenStream> {
    let ident = &input.ident;
    let generics = input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let tokens = quote! {
        impl #impl_generics ::skyzen::openapi::OpenApiSchema for #ident #ty_generics #where_clause {
            fn schema() -> ::utoipa::openapi::RefOr<::utoipa::openapi::schema::Schema> {
                <Self as ::utoipa::PartialSchema>::schema()
            }
        }
    };

    Ok(tokens.into())
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
