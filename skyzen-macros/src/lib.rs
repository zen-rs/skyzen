//! Procedural macros for the Skyzen framework.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, punctuated::Punctuated, Error, Expr, ExprLit, ItemFn, Lit, MetaNameValue,
    Token,
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
