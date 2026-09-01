//! The one walk over a TOML document's string values.
//!
//! Two passes need it: deploy-time `${NAME}` expansion, which rewrites the value, and the secret
//! scanner, which classifies it. They ran their own recursions once, and the TOML paths they
//! reported — the thing a user is told to go and edit — drifted apart. One walker, one path.
//!
//! The walk visits string **values** only. Keys are never rewritten, and a table's own key reaches
//! the visitor as [`StringSite::key`] so the scanner can ask which table a value sits in.

use toml::Value;

/// One string value in the document, with everything a visitor needs to locate and rewrite it.
#[derive(Debug)]
pub struct StringSite<'a> {
    /// TOML path of the string, e.g. `cloudflare.kv_namespaces[0].id`.
    pub location: &'a str,
    /// The key this value is stored under. For an array item, the key of the array itself.
    pub key: &'a str,
    /// The path of the table holding the key, empty at the document root.
    pub parent: &'a str,
    /// The value, rewritable in place.
    pub value: &'a mut String,
}

/// Visit every string value in `table`, depth first, in key order.
///
/// # Errors
///
/// Returns the first error `visit` produces, which stops the walk.
pub fn walk_strings<E>(
    table: &mut toml::Table,
    visit: &mut impl FnMut(StringSite<'_>) -> Result<(), E>,
) -> Result<(), E> {
    walk_table(table, "", visit)
}

fn walk_table<E>(
    table: &mut toml::Table,
    parent: &str,
    visit: &mut impl FnMut(StringSite<'_>) -> Result<(), E>,
) -> Result<(), E> {
    for (key, value) in table.iter_mut() {
        let location = child_path(parent, key);
        walk_value(value, &location, key, parent, visit)?;
    }
    Ok(())
}

fn walk_value<E>(
    value: &mut Value,
    location: &str,
    key: &str,
    parent: &str,
    visit: &mut impl FnMut(StringSite<'_>) -> Result<(), E>,
) -> Result<(), E> {
    match value {
        Value::String(text) => visit(StringSite {
            location,
            key,
            parent,
            value: text,
        }),
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let child = format!("{location}[{index}]");
                walk_value(item, &child, key, parent, visit)?;
            }
            Ok(())
        }
        Value::Table(child) => walk_table(child, location, visit),
        Value::Integer(_) | Value::Float(_) | Value::Boolean(_) | Value::Datetime(_) => Ok(()),
    }
}

/// The TOML path of `key` inside the table at `parent`.
fn child_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_owned()
    } else {
        format!("{parent}.{key}")
    }
}
