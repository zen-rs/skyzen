//! Deploy-time `${NAME}` expansion in `Skyzen.toml` string values.
//!
//! The CLI expands placeholders from the process environment (and the project's `.env` files)
//! when it reads the manifest. `#[skyzen::main]` does **not**: compile-time env is not the
//! deploy runner, and a GitHub secret the compiler does not have must not fail `cargo build`.
//!
//! Expansion runs on the parsed TOML document, before the typed schema, so a value containing
//! quotes or newlines cannot break the file. Keys are never expanded. A missing, unclosed, or
//! invalid name fails the parse; there is no `${NAME:-default}`. `$$` writes a literal `$`.

use std::borrow::Cow;
use toml::Value;

/// Looks up `${NAME}` during expansion. `Ok(None)` means the name is unset.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Result<Option<String>, InterpolateError>;

/// What went wrong expanding a `${NAME}` placeholder.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum InterpolateError {
    /// A `${` with no matching `}`.
    #[error("{location}: unclosed `${{`")]
    Unclosed {
        /// TOML path of the string, e.g. `cloudflare.kv_namespaces[0].id`.
        location: String,
    },
    /// `${}` with nothing between the braces.
    #[error("{location}: empty interpolation name")]
    EmptyName {
        /// TOML path of the string.
        location: String,
    },
    /// The name is not `[A-Za-z_][A-Za-z0-9_]*`.
    #[error("{location}: invalid interpolation name `{name}`")]
    InvalidName {
        /// TOML path of the string.
        location: String,
        /// The rejected name.
        name: String,
    },
    /// `lookup` returned `Ok(None)` for this name.
    #[error("{location}: environment variable `{name}` is not set")]
    Missing {
        /// TOML path of the string.
        location: String,
        /// The variable that was asked for.
        name: String,
    },
    /// The process environment holds `name` but the value is not Unicode.
    #[error("{location}: environment variable `{name}` is not valid Unicode")]
    NotUnicode {
        /// TOML path of the string, empty when the error is produced before a path is known.
        location: String,
        /// The variable that was asked for.
        name: String,
    },
}

impl InterpolateError {
    /// Fill in the TOML path when the error was produced without one (process-env Unicode).
    #[must_use]
    pub fn at(self, location: &str) -> Self {
        match self {
            Self::NotUnicode {
                name,
                location: loc,
            } if loc.is_empty() => Self::NotUnicode {
                location: location.to_owned(),
                name,
            },
            other => other,
        }
    }
}

/// Read `name` from the process environment.
///
/// # Errors
///
/// Returns [`InterpolateError::NotUnicode`] when the variable is set but is not valid Unicode.
pub fn process_env(name: &str) -> Result<Option<String>, InterpolateError> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(InterpolateError::NotUnicode {
            location: String::new(),
            name: name.to_owned(),
        }),
    }
}

/// Expand `${NAME}` placeholders in `input`.
///
/// `lookup` returns the value for `NAME`, `Ok(None)` when it is unset (which becomes
/// [`InterpolateError::Missing`]), or [`InterpolateError::NotUnicode`].
///
/// # Errors
///
/// Returns [`InterpolateError`] when a placeholder is unclosed, empty, not an identifier, unset,
/// or not Unicode.
pub fn expand<'a>(
    input: &'a str,
    location: &str,
    lookup: EnvLookup<'_>,
) -> Result<Cow<'a, str>, InterpolateError> {
    if !input.contains('$') {
        return Ok(Cow::Borrowed(input));
    }

    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$');
            rest = stripped;
            continue;
        }
        if let Some(inner) = after.strip_prefix('{') {
            let Some(end) = inner.find('}') else {
                return Err(InterpolateError::Unclosed {
                    location: location.to_owned(),
                });
            };
            let name = &inner[..end];
            if name.is_empty() {
                return Err(InterpolateError::EmptyName {
                    location: location.to_owned(),
                });
            }
            if !is_ident(name) {
                return Err(InterpolateError::InvalidName {
                    location: location.to_owned(),
                    name: name.to_owned(),
                });
            }
            let value = match lookup(name) {
                Ok(Some(value)) => value,
                Ok(None) => {
                    return Err(InterpolateError::Missing {
                        location: location.to_owned(),
                        name: name.to_owned(),
                    });
                }
                Err(error) => return Err(error.at(location)),
            };
            out.push_str(&value);
            rest = &inner[end + 1..];
            continue;
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    Ok(Cow::Owned(out))
}

/// Expand every string **value** in `table`. Keys are left as written.
///
/// # Errors
///
/// Returns the first [`InterpolateError`] encountered, with `location` set to the TOML path.
pub fn expand_table(
    table: &mut toml::Table,
    lookup: EnvLookup<'_>,
) -> Result<(), InterpolateError> {
    expand_table_at(table, "", lookup)
}

fn expand_table_at(
    table: &mut toml::Table,
    parent: &str,
    lookup: EnvLookup<'_>,
) -> Result<(), InterpolateError> {
    for (key, value) in table.iter_mut() {
        let location = child_path(parent, key);
        expand_value(value, &location, lookup)?;
    }
    Ok(())
}

fn expand_value(
    value: &mut Value,
    location: &str,
    lookup: EnvLookup<'_>,
) -> Result<(), InterpolateError> {
    match value {
        Value::String(text) => {
            if let Cow::Owned(expanded) = expand(text, location, lookup)? {
                *text = expanded;
            }
            Ok(())
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let child = format!("{location}[{index}]");
                expand_value(item, &child, lookup)?;
            }
            Ok(())
        }
        Value::Table(table) => expand_table_at(table, location, lookup),
        Value::Integer(_) | Value::Float(_) | Value::Boolean(_) | Value::Datetime(_) => Ok(()),
    }
}

fn child_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_owned()
    } else {
        format!("{parent}.{key}")
    }
}

fn is_ident(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::{expand, expand_table, InterpolateError};
    use std::collections::BTreeMap;

    fn lookup<'a>(
        env: &'a BTreeMap<&'a str, &'a str>,
    ) -> impl Fn(&str) -> Result<Option<String>, InterpolateError> + 'a {
        |name| Ok(env.get(name).map(|value| (*value).to_owned()))
    }

    #[test]
    fn a_string_without_dollar_is_unchanged() {
        let env = BTreeMap::new();
        let expanded = expand("plain", "k", &lookup(&env)).expect("expand");
        assert!(matches!(expanded, std::borrow::Cow::Borrowed("plain")));
    }

    #[test]
    fn expands_a_placeholder_and_surrounding_text() {
        let env = BTreeMap::from([("ENV", "staging")]);
        assert_eq!(
            expand("api-${ENV}", "name", &lookup(&env)).expect("expand"),
            "api-staging"
        );
    }

    #[test]
    fn doubled_dollar_is_a_literal_dollar() {
        let env = BTreeMap::from([("ENV", "x")]);
        assert_eq!(
            expand("$${ENV}", "k", &lookup(&env)).expect("expand"),
            "${ENV}"
        );
        assert_eq!(expand("$$", "k", &lookup(&env)).expect("expand"), "$");
    }

    #[test]
    fn a_dollar_without_braces_is_literal() {
        let env = BTreeMap::from([("FOO", "x")]);
        assert_eq!(expand("$FOO", "k", &lookup(&env)).expect("expand"), "$FOO");
    }

    #[test]
    fn a_missing_variable_names_the_name_and_location() {
        let env = BTreeMap::new();
        let error =
            expand("${MISSING}", "cloudflare.account_id", &lookup(&env)).expect_err("unset");
        assert_eq!(
            error,
            InterpolateError::Missing {
                location: "cloudflare.account_id".to_owned(),
                name: "MISSING".to_owned(),
            }
        );
    }

    #[test]
    fn unclosed_empty_and_invalid_names_fail() {
        let env = BTreeMap::new();
        let get = lookup(&env);
        assert!(matches!(
            expand("${NOPE", "k", &get),
            Err(InterpolateError::Unclosed { .. })
        ));
        assert!(matches!(
            expand("${}", "k", &get),
            Err(InterpolateError::EmptyName { .. })
        ));
        assert!(matches!(
            expand("${FOO-BAR}", "k", &get),
            Err(InterpolateError::InvalidName { name, .. }) if name == "FOO-BAR"
        ));
        assert!(matches!(
            expand("${FOO:-x}", "k", &get),
            Err(InterpolateError::InvalidName { .. })
        ));
    }

    #[test]
    fn expand_table_walks_nested_tables_and_arrays_and_leaves_keys() {
        let mut table: toml::Table = toml::from_str(
            "[cloudflare.vars]\nAPI_URL = \"${API_URL}\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"${CACHE_ID}\"\n",
        )
        .expect("toml");
        let env = BTreeMap::from([("API_URL", "https://api.flyco.io"), ("CACHE_ID", "ns_abc")]);
        expand_table(&mut table, &lookup(&env)).expect("expand");

        assert_eq!(
            table["cloudflare"]["vars"]["API_URL"].as_str(),
            Some("https://api.flyco.io")
        );
        assert_eq!(
            table["cloudflare"]["kv_namespaces"][0]["id"].as_str(),
            Some("ns_abc")
        );
        assert_eq!(
            table["cloudflare"]["kv_namespaces"][0]["binding"].as_str(),
            Some("CACHE")
        );
    }

    #[test]
    fn a_value_with_quotes_and_newlines_does_not_reparse_the_document() {
        let env = BTreeMap::from([("BANNER", "he said \"hi\"\nnext")]);
        assert_eq!(
            expand("${BANNER}", "vars.BANNER", &lookup(&env)).expect("expand"),
            "he said \"hi\"\nnext"
        );
    }
}
