//! The `FromRow` and `Column` derives: the read side of Skyzen's SQL value mapping.
//!
//! Binding is typed already — `DbValue` has a `From` for every type a parameter can be — and these
//! two derives are the symmetric read direction. `FromRow` turns a result row into a struct one
//! typed column at a time; `Column` makes a domain type (a newtype id, a state-machine enum) into
//! something both directions understand.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Attribute, Data, DataEnum, DataStruct, DeriveInput, Error, Fields, LitStr, Meta, Type};

/// The path every generated impl names Skyzen's service types through.
///
/// Applications reach `skyzen-services` as a dependency of `skyzen`, so an expansion that named it
/// directly would compile only for the ones that also declared it themselves.
fn services() -> TokenStream {
    quote! { ::skyzen::__services }
}

/// Expand `#[derive(FromRow)]`.
pub fn expand_from_row(input: &DeriveInput) -> syn::Result<TokenStream> {
    reject_generics(input, "FromRow")?;

    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(Error::new_spanned(
            &input.ident,
            "`#[derive(FromRow)]` describes a result row, so it applies to a struct with named \
             fields; decode an enum through the type of the column that discriminates it",
        ));
    };
    let Fields::Named(fields) = fields else {
        return Err(Error::new_spanned(
            &input.ident,
            "`#[derive(FromRow)]` needs named fields: a column is matched to a field by name",
        ));
    };
    if fields.named.is_empty() {
        return Err(Error::new_spanned(
            &input.ident,
            "`#[derive(FromRow)]` needs at least one field, since it reads one column per field",
        ));
    }

    let rename_all = container_rename_all(&input.attrs, "row")?;
    let mut bindings = Vec::with_capacity(fields.named.len());
    for field in &fields.named {
        let options = FieldOptions::parse(&field.attrs, "row")?;
        let ident = field
            .ident
            .as_ref()
            .ok_or_else(|| Error::new_spanned(field, "field has no name"))?;
        let column = options.rename.map_or_else(
            || {
                rename_all.map_or_else(
                    || ident.to_string(),
                    |rule| rule.apply_to_field(&ident.to_string()),
                )
            },
            |rename| rename.value(),
        );
        let column = LitStr::new(&column, ident.span());
        let read = if options.json {
            quote! { row.get_json(#column)? }
        } else {
            quote! { row.get(#column)? }
        };
        bindings.push(quote! { #ident: #read });
    }

    let ident = &input.ident;
    let services = services();
    Ok(quote! {
        #[automatically_derived]
        impl #services::sql::FromRow for #ident {
            fn from_row(
                row: #services::sql::Row,
            ) -> ::core::result::Result<Self, #services::sql::RowError> {
                ::core::result::Result::Ok(Self { #(#bindings),* })
            }
        }
    })
}

/// Expand `#[derive(Column)]`.
pub fn expand_column(input: &DeriveInput) -> syn::Result<TokenStream> {
    reject_generics(input, "Column")?;

    match &input.data {
        Data::Enum(data) => expand_column_enum(input, data),
        Data::Struct(data) => expand_column_newtype(input, data),
        Data::Union(_) => Err(Error::new_spanned(
            &input.ident,
            "`#[derive(Column)]` applies to a newtype struct or to an enum whose variants have no \
             fields",
        )),
    }
}

/// A unit-variant enum, stored as the token naming its variant.
fn expand_column_enum(input: &DeriveInput, data: &DataEnum) -> syn::Result<TokenStream> {
    if data.variants.is_empty() {
        return Err(Error::new_spanned(
            &input.ident,
            "`#[derive(Column)]` needs at least one variant: an empty enum has no value to store",
        ));
    }

    let rename_all = container_rename_all(&input.attrs, "column")?.unwrap_or(RenameRule::Snake);
    let mut variants = Vec::with_capacity(data.variants.len());
    let mut tokens: Vec<LitStr> = Vec::with_capacity(data.variants.len());
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(Error::new_spanned(
                variant,
                "`#[derive(Column)]` stores a variant as one text token, so no variant may carry \
                 fields",
            ));
        }
        let options = FieldOptions::parse(&variant.attrs, "column")?;
        if options.json {
            return Err(Error::new_spanned(
                variant,
                "`json` is a `#[derive(FromRow)]` field option and means nothing on a variant",
            ));
        }
        let token = options.rename.map_or_else(
            || rename_all.apply_to_variant(&variant.ident.to_string()),
            |rename| rename.value(),
        );
        if let Some(position) = tokens.iter().position(|seen| seen.value() == token) {
            return Err(Error::new_spanned(
                variant,
                format!(
                    "two variants would be stored as the same token `{token}`; the first is \
                     `{}`. A stored token has to name exactly one variant for a read to be \
                     unambiguous",
                    data.variants[position].ident,
                ),
            ));
        }
        tokens.push(LitStr::new(&token, variant.ident.span()));
        variants.push(&variant.ident);
    }

    let ident = &input.ident;
    let services = services();
    let expected = LitStr::new(&format!("one of the tokens `{ident}` names"), ident.span());
    Ok(quote! {
        #[automatically_derived]
        impl #services::sql::ColumnEnum for #ident {
            const TOKENS: &'static [&'static str] = &[#(#tokens),*];

            fn token(&self) -> &'static str {
                match self { #(Self::#variants => #tokens,)* }
            }

            fn from_token(token: &str) -> ::core::option::Option<Self> {
                match token {
                    #(#tokens => ::core::option::Option::Some(Self::#variants),)*
                    _ => ::core::option::Option::None,
                }
            }
        }

        #[automatically_derived]
        impl ::core::convert::From<#ident> for #services::sql::DbValue {
            fn from(value: #ident) -> Self {
                Self::from(&value)
            }
        }

        #[automatically_derived]
        impl ::core::convert::From<&#ident> for #services::sql::DbValue {
            fn from(value: &#ident) -> Self {
                Self::Text(::std::borrow::ToOwned::to_owned(
                    #services::sql::ColumnEnum::token(value),
                ))
            }
        }

        #[automatically_derived]
        impl #services::sql::FromColumn for #ident {
            fn from_column(
                value: &#services::serde_json::Value,
            ) -> ::core::result::Result<Self, #services::sql::ColumnError> {
                let token = <::std::string::String as #services::sql::FromColumn>::from_column(
                    value,
                )?;
                <Self as #services::sql::ColumnEnum>::from_token(&token).ok_or_else(|| {
                    #services::sql::ColumnError::invalid(
                        #expected,
                        value,
                        &::std::format!(
                            "the tokens are: {}",
                            <Self as #services::sql::ColumnEnum>::TOKENS.join(", "),
                        ),
                    )
                })
            }
        }
    })
}

/// A newtype wrapping a type that is itself one column.
fn expand_column_newtype(input: &DeriveInput, data: &DataStruct) -> syn::Result<TokenStream> {
    let Fields::Unnamed(fields) = &data.fields else {
        return Err(Error::new_spanned(
            &input.ident,
            "`#[derive(Column)]` applies to a newtype struct — one unnamed field wrapping the type \
             the column actually holds. A struct with named fields is a row, not a column: derive \
             `FromRow` instead",
        ));
    };
    let [field] = fields.unnamed.iter().collect::<Vec<_>>()[..] else {
        return Err(Error::new_spanned(
            &fields.unnamed,
            "`#[derive(Column)]` needs exactly one field, since one column holds one value",
        ));
    };

    if let Some(rule) = container_rename_all(&input.attrs, "column")? {
        let _ = rule;
        return Err(Error::new_spanned(
            &input.ident,
            "`rename_all` renames the variants of an enum; a newtype has no variants to rename",
        ));
    }

    let ident = &input.ident;
    let inner: &Type = &field.ty;
    let services = services();
    Ok(quote! {
        #[automatically_derived]
        impl ::core::convert::From<#ident> for #services::sql::DbValue {
            fn from(value: #ident) -> Self {
                ::core::convert::Into::into(value.0)
            }
        }

        #[automatically_derived]
        impl #services::sql::FromColumn for #ident {
            fn from_column(
                value: &#services::serde_json::Value,
            ) -> ::core::result::Result<Self, #services::sql::ColumnError> {
                ::core::result::Result::Ok(Self(
                    <#inner as #services::sql::FromColumn>::from_column(value)?,
                ))
            }
        }
    })
}

/// Neither derive supports generics: a row type is decoded from named columns, and a bound that
/// could not be checked here would surface as an error inside the expansion instead.
fn reject_generics(input: &DeriveInput, derive: &str) -> syn::Result<()> {
    if input.generics.params.is_empty() {
        return Ok(());
    }
    Err(Error::new_spanned(
        &input.generics,
        format!(
            "`#[derive({derive})]` does not support generic parameters; decode into a concrete \
             type"
        ),
    ))
}

/// The per-field (or per-variant) options of both derives.
#[derive(Default)]
struct FieldOptions {
    /// The column name, or the stored token, when it is not the identifier's own.
    rename: Option<LitStr>,
    /// Whether the column holds a JSON document to be deserialized with `serde`.
    json: bool,
}

impl FieldOptions {
    /// Read the options in `namespace`, so a `#[column(…)]` on a `FromRow` field — or the reverse
    /// — is a typo the author hears about rather than an attribute nothing reads.
    fn parse(attrs: &[Attribute], namespace: &str) -> syn::Result<Self> {
        let mut options = Self::default();
        for attr in attrs {
            if !attr.path().is_ident(namespace) {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    options.rename = Some(meta.value()?.parse()?);
                    return Ok(());
                }
                if meta.path.is_ident("json") {
                    options.json = true;
                    return Ok(());
                }
                if meta.path.is_ident("rename_all") {
                    return Err(meta.error(
                        "`rename_all` names a rule for a whole struct or enum, so it belongs on \
                         the type rather than on one field",
                    ));
                }
                Err(meta.error("expected `rename = \"…\"` or `json`"))
            })?;
        }
        Ok(options)
    }
}

/// Read a container's `rename_all` rule, if it declared one.
fn container_rename_all(attrs: &[Attribute], namespace: &str) -> syn::Result<Option<RenameRule>> {
    let mut rule = None;
    for attr in attrs {
        if !attr.path().is_ident(namespace) {
            continue;
        }
        let Meta::List(_) = &attr.meta else {
            return Err(Error::new_spanned(
                attr,
                format!("expected `#[{namespace}(…)]`"),
            ));
        };
        attr.parse_nested_meta(|meta| {
            if !meta.path.is_ident("rename_all") {
                return Err(meta.error(
                    "expected `rename_all = \"…\"`; `rename` and `json` belong on a field",
                ));
            }
            let literal: LitStr = meta.value()?.parse()?;
            rule = Some(RenameRule::parse(&literal)?);
            Ok(())
        })?;
    }
    Ok(rule)
}

/// How an identifier is spelled once it reaches the database.
///
/// These are `serde`'s eight rules, with `serde`'s own semantics — right down to treating every
/// capital as a word boundary, so that `#[row(rename_all = "…")]` and `#[serde(rename_all = "…")]`
/// never disagree about a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameRule {
    /// `orderstate`
    Lower,
    /// `ORDERSTATE`
    Upper,
    /// `OrderState`
    Pascal,
    /// `orderState`
    Camel,
    /// `order_state`
    Snake,
    /// `ORDER_STATE`
    ScreamingSnake,
    /// `order-state`
    Kebab,
    /// `ORDER-STATE`
    ScreamingKebab,
}

impl RenameRule {
    /// Every spelling a `rename_all` may name, in the form it is written.
    const NAMES: [(&'static str, Self); 8] = [
        ("lowercase", Self::Lower),
        ("UPPERCASE", Self::Upper),
        ("PascalCase", Self::Pascal),
        ("camelCase", Self::Camel),
        ("snake_case", Self::Snake),
        ("SCREAMING_SNAKE_CASE", Self::ScreamingSnake),
        ("kebab-case", Self::Kebab),
        ("SCREAMING-KEBAB-CASE", Self::ScreamingKebab),
    ];

    fn parse(literal: &LitStr) -> syn::Result<Self> {
        let value = literal.value();
        Self::NAMES
            .iter()
            .find(|(name, _)| *name == value)
            .map(|(_, rule)| *rule)
            .ok_or_else(|| {
                Error::new_spanned(
                    literal,
                    format!(
                        "unknown rename rule `{value}`; expected one of {}",
                        Self::NAMES
                            .iter()
                            .map(|(name, _)| format!("`{name}`"))
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                )
            })
    }

    /// Apply the rule to a `PascalCase` variant name.
    fn apply_to_variant(self, variant: &str) -> String {
        match self {
            Self::Lower => variant.to_ascii_lowercase(),
            Self::Upper => variant.to_ascii_uppercase(),
            Self::Pascal => variant.to_owned(),
            Self::Camel => {
                let mut chars = variant.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_lowercase().collect::<String>() + chars.as_str()
                })
            }
            Self::Snake => {
                let mut snake = String::with_capacity(variant.len());
                for (index, ch) in variant.char_indices() {
                    if index > 0 && ch.is_uppercase() {
                        snake.push('_');
                    }
                    snake.extend(ch.to_lowercase());
                }
                snake
            }
            Self::ScreamingSnake => Self::Snake.apply_to_variant(variant).to_ascii_uppercase(),
            Self::Kebab => Self::Snake.apply_to_variant(variant).replace('_', "-"),
            Self::ScreamingKebab => Self::ScreamingSnake
                .apply_to_variant(variant)
                .replace('_', "-"),
        }
    }

    /// Apply the rule to a `snake_case` field name.
    fn apply_to_field(self, field: &str) -> String {
        match self {
            Self::Lower | Self::Snake => field.to_owned(),
            Self::Upper | Self::ScreamingSnake => field.to_ascii_uppercase(),
            Self::Pascal => field
                .split('_')
                .map(|word| {
                    let mut chars = word.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_uppercase().collect::<String>() + chars.as_str()
                    })
                })
                .collect(),
            Self::Camel => {
                let pascal = Self::Pascal.apply_to_field(field);
                let mut chars = pascal.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_lowercase().collect::<String>() + chars.as_str()
                })
            }
            Self::Kebab => field.replace('_', "-"),
            Self::ScreamingKebab => Self::ScreamingSnake.apply_to_field(field).replace('_', "-"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RenameRule;

    #[test]
    fn variant_rules_match_serdes_spelling() {
        let cases = [
            (RenameRule::Lower, "orderstate"),
            (RenameRule::Upper, "ORDERSTATE"),
            (RenameRule::Pascal, "OrderState"),
            (RenameRule::Camel, "orderState"),
            (RenameRule::Snake, "order_state"),
            (RenameRule::ScreamingSnake, "ORDER_STATE"),
            (RenameRule::Kebab, "order-state"),
            (RenameRule::ScreamingKebab, "ORDER-STATE"),
        ];
        for (rule, expected) in cases {
            assert_eq!(rule.apply_to_variant("OrderState"), expected, "{rule:?}");
        }
    }

    #[test]
    fn field_rules_match_serdes_spelling() {
        let cases = [
            (RenameRule::Lower, "order_state"),
            (RenameRule::Upper, "ORDER_STATE"),
            (RenameRule::Pascal, "OrderState"),
            (RenameRule::Camel, "orderState"),
            (RenameRule::Snake, "order_state"),
            (RenameRule::ScreamingSnake, "ORDER_STATE"),
            (RenameRule::Kebab, "order-state"),
            (RenameRule::ScreamingKebab, "ORDER-STATE"),
        ];
        for (rule, expected) in cases {
            assert_eq!(rule.apply_to_field("order_state"), expected, "{rule:?}");
        }
    }

    #[test]
    fn every_capital_starts_a_word_the_way_serde_does() {
        // serde treats each capital as a boundary rather than folding acronyms, and matching it
        // exactly is the whole point of writing the rules out here.
        assert_eq!(
            RenameRule::Snake.apply_to_variant("HTTPStatus"),
            "h_t_t_p_status"
        );
    }
}

#[cfg(test)]
mod expansion_tests {
    use super::{expand_column, expand_from_row};
    use syn::DeriveInput;

    /// The message a derive rejects `source` with, so a mistake reads as a sentence rather than as
    /// an error inside an expansion the author never wrote.
    fn rejection(
        source: &str,
        derive: fn(&DeriveInput) -> syn::Result<proc_macro2::TokenStream>,
    ) -> String {
        let input: DeriveInput = syn::parse_str(source).expect("the fixture parses");
        derive(&input).err().map_or_else(
            || panic!("`{source}` should have been rejected"),
            |error| error.to_string(),
        )
    }

    fn expansion(
        source: &str,
        derive: fn(&DeriveInput) -> syn::Result<proc_macro2::TokenStream>,
    ) -> String {
        let input: DeriveInput = syn::parse_str(source).expect("the fixture parses");
        derive(&input).expect("the fixture is accepted").to_string()
    }

    #[test]
    fn a_row_reads_one_column_per_field_and_honours_the_renames() {
        let expanded = expansion(
            r#"#[row(rename_all = "camelCase")]
               struct Order {
                   placed_at: DateTime<Utc>,
                   #[row(rename = "customer")] customer_id: CustomerId,
                   #[row(json)] items: Vec<LineItem>,
               }"#,
            expand_from_row,
        );
        assert!(expanded.contains(r#"row . get ("placedAt")"#), "{expanded}");
        assert!(expanded.contains(r#"row . get ("customer")"#), "{expanded}");
        assert!(
            expanded.contains(r#"row . get_json ("items")"#),
            "{expanded}"
        );
    }

    #[test]
    fn a_row_has_to_be_a_struct_with_named_fields() {
        assert!(
            rejection("struct Order(i64);", expand_from_row).contains("named fields"),
            "a tuple struct has no column names to match"
        );
        assert!(rejection("enum Order { A }", expand_from_row).contains("named fields"));
        assert!(rejection("struct Order {}", expand_from_row).contains("at least one field"));
        assert!(
            rejection("struct Order<T> { id: T }", expand_from_row).contains("generic"),
            "a generic row type has no concrete column types to decode"
        );
    }

    #[test]
    fn a_column_enum_is_stored_as_snake_case_unless_told_otherwise() {
        let expanded = expansion(
            "enum OrderState { AwaitingPayment, #[column(rename = \"cancelled\")] Canceled }",
            expand_column,
        );
        assert!(expanded.contains(r#""awaiting_payment""#), "{expanded}");
        assert!(expanded.contains(r#""cancelled""#), "{expanded}");

        let screaming = expansion(
            r#"#[column(rename_all = "SCREAMING_SNAKE_CASE")] enum OrderState { AwaitingPayment }"#,
            expand_column,
        );
        assert!(screaming.contains(r#""AWAITING_PAYMENT""#), "{screaming}");
    }

    #[test]
    fn a_column_enum_may_not_carry_fields_or_repeat_a_token() {
        assert!(rejection("enum State { Shipped(u8) }", expand_column).contains("no variant"));
        assert!(rejection("enum State {}", expand_column).contains("at least one variant"));
        assert!(
            rejection(
                "enum State { Shipped, #[column(rename = \"shipped\")] Sent }",
                expand_column,
            )
            .contains("same token"),
            "two variants sharing a token would make a read ambiguous"
        );
        assert!(
            rejection(
                r#"#[column(rename_all = "wat")] enum State { Shipped }"#,
                expand_column
            )
            .contains("unknown rename rule"),
        );
    }

    #[test]
    fn a_column_struct_has_to_be_a_newtype() {
        let expanded = expansion("struct CustomerId(Uuid);", expand_column);
        assert!(expanded.contains("DbValue"), "{expanded}");
        assert!(expanded.contains("FromColumn"), "{expanded}");

        assert!(rejection("struct Order { id: Uuid }", expand_column).contains("FromRow"));
        assert!(rejection("struct Pair(u8, u8);", expand_column).contains("exactly one field"));
    }
}
