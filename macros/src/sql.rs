//! The `sql!` macro: bind order taken from the query text instead of from the call site.
//!
//! Positional binding is a silent correctness hazard, not merely verbose. Inserting one condition
//! into the middle of a statement shifts every following `.bind()` by one, and because SQL has so
//! few distinct types the shifted call usually still compiles and still runs — against the wrong
//! columns. Writing the value where the placeholder is removes the possibility: there is only one
//! ordering, and it is the one the reader sees.
//!
//! What this deliberately is *not* is a compile-time query checker. The same statement runs on D1,
//! `SQLite`, `PostgreSQL`, `MySQL` and Azure SQL, so validating it against one dialect's parser
//! would be a guarantee that does not hold. Nothing here reads a schema, opens a connection, or
//! looks at the SQL beyond finding the captures.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{
    Error, Expr, LitStr, Token,
    parse::{Parse, ParseStream},
};

/// A parsed `sql!(source, "…")` invocation.
pub struct SqlInput {
    /// The thing `.query()` is called on — a `Db`, a transaction, a `DurableDb`.
    source: Expr,
    /// The query text, captures included.
    template: LitStr,
}

impl Parse for SqlInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let source = input.parse()?;
        input.parse::<Token![,]>().map_err(|_| {
            Error::new(
                input.span(),
                "expected `sql!(source, \"…\")`: the query runs against a `Db`, a transaction or a \
                 `DurableDb`, named first",
            )
        })?;
        let template = input.parse()?;

        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
        if !input.is_empty() {
            return Err(Error::new(
                input.span(),
                "`sql!` takes no arguments after the query: every value is written inline where it \
                 is bound, as `{value}` or `{ an.expression() }`. That is the point of the macro — \
                 an argument list is exactly the thing that can fall out of step with the query",
            ));
        }

        Ok(Self { source, template })
    }
}

/// Expand `sql!`.
pub fn expand(input: &SqlInput) -> syn::Result<TokenStream> {
    let template = input.template.value();
    let span = input.template.span();
    let Template { sql, captures } = parse_template(&template, span)?;

    let source = &input.source;
    let binds = captures
        .iter()
        .map(|capture| capture.expression(span))
        .collect::<syn::Result<Vec<_>>>()?;

    let sql = LitStr::new(&sql, span);
    Ok(quote! {
        #source.query(#sql)#(.bind(#binds))*
    })
}

/// One `{…}` found in the query text.
struct Capture {
    /// The Rust source between the braces.
    source: String,
}

impl Capture {
    /// The value this capture binds.
    fn expression(&self, span: Span) -> syn::Result<Expr> {
        syn::parse_str(&self.source).map_err(|error| {
            Error::new(
                span,
                format!(
                    "`{{{}}}` is not a Rust expression: {error}. A capture is a value to bind — \
                     name a binding in scope, or write an expression; `{{{{` is a literal brace",
                    self.source,
                ),
            )
        })
    }
}

/// A query text split into the SQL a backend sees and the values bound into it.
struct Template {
    /// The statement with every capture replaced by `?`, the placeholder every dialect accepts.
    sql: String,
    /// The captures, in the order their placeholders appear.
    captures: Vec<Capture>,
}

/// Split the macro's query text into SQL and captures.
///
/// `{{` and `}}` are literal braces, as in `format!`. Everything else between a `{` and its
/// matching `}` is Rust source, which the caller parses as an expression — this only has to find
/// where the capture ends, which means tracking brace depth and not being fooled by a brace inside
/// a string or character literal.
fn parse_template(template: &str, span: Span) -> syn::Result<Template> {
    let mut sql = String::with_capacity(template.len());
    let mut captures = Vec::new();
    let mut chars = template.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        match ch {
            '{' if chars.peek().map(|(_, next)| *next) == Some('{') => {
                chars.next();
                sql.push('{');
            }
            '}' if chars.peek().map(|(_, next)| *next) == Some('}') => {
                chars.next();
                sql.push('}');
            }
            '}' => {
                return Err(Error::new(
                    span,
                    "unmatched `}` in the query; write `}}` for a literal brace",
                ));
            }
            '{' => {
                let source = read_capture(template, index, &mut chars, span)?;
                if source.trim().is_empty() {
                    return Err(Error::new(
                        span,
                        "`{}` has nothing to bind: name the value, as in `{user_id}`, or write an \
                         expression, as in `{ user.id() }`. `sql!` has no positional arguments to \
                         fall back on",
                    ));
                }
                captures.push(Capture { source });
                // Every dialect accepts `?`; the query builder rewrites it to `$1` or `@P1` where
                // the backend needs that, so the macro never has to know which backend it is for.
                sql.push('?');
            }
            other => sql.push(other),
        }
    }

    Ok(Template { sql, captures })
}

/// Read one capture's Rust source, given that the opening `{` has just been consumed.
fn read_capture(
    template: &str,
    open: usize,
    chars: &mut core::iter::Peekable<core::str::CharIndices<'_>>,
    span: Span,
) -> syn::Result<String> {
    let start = open + '{'.len_utf8();
    let mut depth = 1usize;

    while let Some((index, ch)) = chars.next() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(template[start..index].to_owned());
                }
            }
            // A brace inside a literal is text, not structure. Skipping the literal is what keeps
            // `{ format!("a}b") }` and `{ split('}') }` from ending the capture early.
            '"' => skip_string(chars, false),
            'r' if matches!(chars.peek(), Some((_, '"' | '#'))) => skip_raw_string(chars),
            '\'' => skip_char_literal(chars),
            _ => {}
        }
    }

    Err(Error::new(
        span,
        "unclosed `{` in the query; write `}}` for a literal brace, or close the capture",
    ))
}

/// Consume the rest of a `"…"` literal, honouring backslash escapes.
fn skip_string(chars: &mut core::iter::Peekable<core::str::CharIndices<'_>>, raw: bool) {
    while let Some((_, ch)) = chars.next() {
        match ch {
            '\\' if !raw => {
                chars.next();
            }
            '"' => return,
            _ => {}
        }
    }
}

/// Consume the rest of a raw string, `r"…"` or `r#"…"#`, given the `r` has been consumed.
fn skip_raw_string(chars: &mut core::iter::Peekable<core::str::CharIndices<'_>>) {
    let mut hashes = 0usize;
    while let Some((_, '#')) = chars.peek() {
        chars.next();
        hashes += 1;
    }
    if !matches!(chars.peek(), Some((_, '"'))) {
        // Not a raw string after all — an identifier such as `r#type`, or a bare `r`.
        return;
    }
    chars.next();

    loop {
        match chars.next() {
            Some((_, '"')) => {
                let mut seen = 0usize;
                while seen < hashes && matches!(chars.peek(), Some((_, '#'))) {
                    chars.next();
                    seen += 1;
                }
                if seen == hashes {
                    return;
                }
            }
            Some(_) => {}
            None => return,
        }
    }
}

/// Consume a `'x'` or `'\n'` character literal, leaving a lifetime alone.
///
/// A lifetime and a character literal both open with `'`, and only the literal closes. Looking one
/// or two characters ahead tells them apart, which matters because `{ text.split('}') }` is an
/// ordinary thing to write and a lifetime such as `&'a str` is too.
fn skip_char_literal(chars: &mut core::iter::Peekable<core::str::CharIndices<'_>>) {
    let Some((_, first)) = chars.peek().copied() else {
        return;
    };

    if first == '\\' {
        chars.next();
        // The escape body: one character for `\n`, more for `\u{1F600}`, all of which end at `'`.
        for (_, ch) in chars.by_ref() {
            if ch == '\'' {
                return;
            }
        }
        return;
    }

    // `'a'` is a literal; `'a ` and `'a>` are a lifetime.
    let mut lookahead = chars.clone();
    lookahead.next();
    if matches!(lookahead.peek(), Some((_, '\''))) {
        chars.next();
        chars.next();
    }
}

#[cfg(test)]
mod tests {
    use super::{SqlInput, parse_template};
    use proc_macro2::Span;

    fn split(template: &str) -> (String, Vec<String>) {
        let parsed = parse_template(template, Span::call_site()).expect("the template parses");
        (
            parsed.sql,
            parsed
                .captures
                .into_iter()
                .map(|capture| capture.source)
                .collect(),
        )
    }

    fn rejection(template: &str) -> String {
        parse_template(template, Span::call_site())
            .err()
            .map_or_else(
                || panic!("`{template}` should have been rejected"),
                |error| error.to_string(),
            )
    }

    #[test]
    fn every_capture_becomes_a_placeholder_in_the_order_it_is_written() {
        let (sql, captures) =
            split("SELECT id FROM users WHERE id = {user_id} AND state = {state}");
        assert_eq!(sql, "SELECT id FROM users WHERE id = ? AND state = ?");
        assert_eq!(captures, vec!["user_id", "state"]);
    }

    #[test]
    fn a_condition_inserted_in_the_middle_renumbers_itself() {
        // The whole point: the second query is the first with one condition spliced in, and no
        // call site had to be renumbered for the values to stay with their columns.
        let (_, before) = split("SELECT id FROM t WHERE a = {a} AND c = {c}");
        let (_, after) = split("SELECT id FROM t WHERE a = {a} AND b = {b} AND c = {c}");
        assert_eq!(before, vec!["a", "c"]);
        assert_eq!(after, vec!["a", "b", "c"]);
    }

    #[test]
    fn doubled_braces_are_literal_text() {
        let (sql, captures) = split("SELECT '{{\"kind\":\"email\"}}'::jsonb");
        assert_eq!(sql, "SELECT '{\"kind\":\"email\"}'::jsonb");
        assert!(captures.is_empty());
    }

    #[test]
    fn a_capture_may_be_any_expression() {
        let (sql, captures) =
            split("SELECT id FROM t WHERE id = { user.id() } AND n = {count + 1}");
        assert_eq!(sql, "SELECT id FROM t WHERE id = ? AND n = ?");
        assert_eq!(captures, vec![" user.id() ", "count + 1"]);
    }

    #[test]
    fn a_brace_inside_a_literal_does_not_end_the_capture() {
        let (_, captures) = split(r#"SELECT {  format!("a}b")  } FROM t"#);
        assert_eq!(captures, vec![r#"  format!("a}b")  "#]);

        let (_, captures) = split("SELECT { name.replace('}', \"\") } FROM t");
        assert_eq!(captures, vec![" name.replace('}', \"\") "]);

        let (_, captures) = split("SELECT { r#\"a}b\"# } FROM t");
        assert_eq!(captures, vec![" r#\"a}b\"# "]);
    }

    #[test]
    fn a_lifetime_is_not_mistaken_for_a_character_literal() {
        let (_, captures) = split("SELECT { value as &'static str } FROM t");
        assert_eq!(captures, vec![" value as &'static str "]);
    }

    #[test]
    fn nested_braces_belong_to_the_capture() {
        let (_, captures) = split("SELECT { if flag { a } else { b } } FROM t");
        assert_eq!(captures, vec![" if flag { a } else { b } "]);
    }

    #[test]
    fn a_malformed_capture_is_refused_with_the_reason() {
        assert!(rejection("SELECT {} FROM t").contains("nothing to bind"));
        assert!(rejection("SELECT {a FROM t").contains("unclosed"));
        assert!(rejection("SELECT a} FROM t").contains("unmatched"));
    }

    /// `syn`'s types are `Debug` only under its `extra-traits` feature, which costs every build of
    /// this crate to serve one assertion — so the parse result is inspected rather than unwrapped.
    fn invocation_rejection(source: &str) -> String {
        match syn::parse_str::<SqlInput>(source) {
            Ok(_) => panic!("`{source}` should have been rejected"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn the_invocation_needs_a_source_and_a_query_and_nothing_else() {
        assert!(syn::parse_str::<SqlInput>(r#"db, "SELECT 1""#).is_ok());
        assert!(
            syn::parse_str::<SqlInput>(r#"db, "SELECT 1","#).is_ok(),
            "a trailing comma is fine"
        );
        assert!(invocation_rejection(r#"db, "SELECT ?", id"#).contains("written inline"));
        assert!(invocation_rejection(r#""SELECT 1""#).contains("named first"));
    }
}
