//! Detect secrets that have been written into `Skyzen.toml` as literal strings.
//!
//! Two classes, because a false block on a resource id is worse than a missed warning:
//!
//! * **Block** — the value is a documented credential form (a GitHub PAT prefix, a PEM
//!   private key, a URL with a password). The CLI refuses to load the file.
//! * **Heuristic** — a `vars` / `[aws.env]` key whose *name* looks like a secret, or a
//!   JWT-shaped string. The CLI warns. `${NAME}` placeholders are neither.

use crate::{
    schema::{VarName, PLAINTEXT_VARIABLE_TABLES},
    walk::{walk_strings, StringSite},
};
use std::{
    convert::Infallible,
    fmt::{Display, Formatter},
};

/// One string in the document that looks like a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    /// TOML path, e.g. `cloudflare.vars.API_KEY`.
    pub location: String,
    /// What kind of secret, for the error or warning. Never the value itself.
    pub kind: &'static str,
}

impl Display for SecretFinding {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.location, self.kind)
    }
}

/// 100% credential forms found in string values.
///
/// The value is never stored: printing this error cannot leak the secret a second time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretError {
    /// Every blocking finding. Never empty.
    pub findings: Vec<SecretFinding>,
}

impl std::error::Error for SecretError {}

impl Display for SecretError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "committed secret in Skyzen.toml ({}); use a ${{NAME}} placeholder or `skyzen secret set`",
            self.findings
                .iter()
                .map(SecretFinding::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
}

/// Block vs warn findings from one document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretReport {
    /// Documented credential forms. The CLI treats these as a load error.
    pub blocks: Vec<SecretFinding>,
    /// Name-based or JWT-shaped values. The CLI warns and continues.
    pub warnings: Vec<SecretFinding>,
}

/// Walk every string **value** in `table`. Keys are not inspected for credential forms.
///
/// The table is taken by mutable reference because the walk is shared with `${NAME}` expansion,
/// which rewrites what it visits; this pass only reads.
#[must_use]
pub fn scan_table(table: &mut toml::Table) -> SecretReport {
    let mut report = SecretReport::default();
    let visit = &mut |site: StringSite<'_>| -> Result<(), Infallible> {
        classify(
            site.location,
            site.key,
            site.parent,
            site.value,
            &mut report,
        );
        Ok(())
    };
    walk_strings(table, visit).unwrap_or_else(|never| match never {});
    report
}

fn classify(location: &str, key: &str, parent: &str, text: &str, report: &mut SecretReport) {
    if text.trim().is_empty() || is_placeholder(text) {
        return;
    }
    if let Some(kind) = block_kind(text) {
        report.blocks.push(SecretFinding {
            location: location.to_owned(),
            kind,
        });
        return;
    }
    if looks_like_jwt(text) {
        report.warnings.push(SecretFinding {
            location: location.to_owned(),
            kind: "JWT-shaped string",
        });
        return;
    }
    if is_secret_key_table(parent) && is_secret_named_key(key) {
        report.warnings.push(SecretFinding {
            location: location.to_owned(),
            kind: SECRET_NAMED_KEY,
        });
    }
}

/// The warning a plaintext value under a secret-named key produces.
///
/// Named because the scanner emits it and two tests assert on it; the fix it names is the one
/// portable one, `[[secret]]`, rather than a per-provider table.
const SECRET_NAMED_KEY: &str =
    "plaintext value of a secret-named key; declare it as [[secret]] and run `skyzen secret push`";

/// Documented forms that cannot reasonably be a resource id or a URL.
fn block_kind(text: &str) -> Option<&'static str> {
    if text.contains("BEGIN ") && text.contains("PRIVATE KEY") {
        return Some("PEM private key");
    }
    if url_has_password(text) {
        return Some("URL with a password");
    }
    if is_aws_access_key_id(text) {
        return Some("AWS access key id");
    }
    for rule in BLOCK_PREFIXES {
        if token_starting(text, rule.prefix).is_some_and(|token| token.len() >= rule.min_len) {
            return Some(rule.kind);
        }
    }
    None
}

struct PrefixRule {
    prefix: &'static str,
    min_len: usize,
    kind: &'static str,
}

/// Prefixes assigned by the issuer. Lengths are the documented minimums, so a truncated
/// paste still has to look like that issuer's token, not like a Worker name.
const BLOCK_PREFIXES: &[PrefixRule] = &[
    PrefixRule {
        prefix: "ghp_",
        min_len: 40,
        kind: "GitHub personal access token",
    },
    PrefixRule {
        prefix: "gho_",
        min_len: 40,
        kind: "GitHub OAuth access token",
    },
    PrefixRule {
        prefix: "ghu_",
        min_len: 40,
        kind: "GitHub user-to-server token",
    },
    PrefixRule {
        prefix: "ghs_",
        min_len: 40,
        kind: "GitHub server-to-server token",
    },
    PrefixRule {
        prefix: "ghr_",
        min_len: 40,
        kind: "GitHub refresh token",
    },
    PrefixRule {
        prefix: "github_pat_",
        min_len: 82,
        kind: "GitHub fine-grained personal access token",
    },
    PrefixRule {
        prefix: "glpat-",
        min_len: 20,
        kind: "GitLab personal access token",
    },
    PrefixRule {
        prefix: "xoxb-",
        min_len: 50,
        kind: "Slack bot token",
    },
    PrefixRule {
        prefix: "xoxp-",
        min_len: 50,
        kind: "Slack user token",
    },
    PrefixRule {
        prefix: "sk_live_",
        min_len: 32,
        kind: "Stripe live secret key",
    },
    PrefixRule {
        prefix: "sk_test_",
        min_len: 32,
        kind: "Stripe test secret key",
    },
    PrefixRule {
        prefix: "rk_live_",
        min_len: 32,
        kind: "Stripe live restricted key",
    },
    PrefixRule {
        prefix: "rk_test_",
        min_len: 32,
        kind: "Stripe test restricted key",
    },
    PrefixRule {
        prefix: "sk-ant-",
        min_len: 40,
        kind: "Anthropic API key",
    },
    PrefixRule {
        prefix: "sk-proj-",
        min_len: 40,
        kind: "OpenAI project API key",
    },
    PrefixRule {
        prefix: "AIza",
        min_len: 39,
        kind: "Google API key",
    },
    PrefixRule {
        prefix: "npm_",
        min_len: 40,
        kind: "npm access token",
    },
    PrefixRule {
        prefix: "shpat_",
        min_len: 32,
        kind: "Shopify admin API token",
    },
];

fn token_starting<'a>(text: &'a str, prefix: &'a str) -> Option<&'a str> {
    let pos = text.find(prefix)?;
    text[pos..].split_whitespace().next()
}

fn is_aws_access_key_id(text: &str) -> bool {
    for prefix in ["AKIA", "ASIA"] {
        if let Some(token) = token_starting(text, prefix) {
            let rest = &token[prefix.len()..];
            if rest.len() == 16
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            {
                return true;
            }
        }
    }
    false
}

fn url_has_password(text: &str) -> bool {
    let Some((_scheme, rest)) = text.split_once("://") else {
        return false;
    };
    let Some((userinfo, _host)) = rest.split_once('@') else {
        return false;
    };
    match userinfo.split_once(':') {
        Some((_, password)) => !password.is_empty(),
        None => false,
    }
}

/// A JWT anywhere in the value, matched token by token like the issuer prefixes above: a token
/// pasted into a longer string ("Authorization: Bearer eyJ…") is the same leak as one on its own.
fn looks_like_jwt(text: &str) -> bool {
    text.split_whitespace().any(is_jwt_token)
}

fn is_jwt_token(token: &str) -> bool {
    let mut parts = token.split('.');
    let Some(header) = parts.next() else {
        return false;
    };
    let Some(payload) = parts.next() else {
        return false;
    };
    parts.next().is_some()
        && parts.next().is_none()
        && header.starts_with("eyJ")
        && payload.starts_with("eyJ")
}

fn is_placeholder(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("${")
        .and_then(|inner| inner.strip_suffix('}'))
        .is_some_and(VarName::is_valid)
}

/// Whether `parent` is one of the tables whose keys are plaintext platform variables.
///
/// Matched against the schema's own list, plus the Cloudflare environment overlay spelling of it:
/// `[cloudflare.env.staging.vars]` is `[cloudflare.vars]` for one environment, and a secret
/// committed there is committed just the same.
fn is_secret_key_table(parent: &str) -> bool {
    PLAINTEXT_VARIABLE_TABLES
        .iter()
        .any(|table| parent == *table || is_cloudflare_overlay_of(parent, table))
}

/// Whether `parent` is `cloudflare.env.<name>.<rest>` for a `cloudflare.<rest>` table.
fn is_cloudflare_overlay_of(parent: &str, table: &str) -> bool {
    let Some(rest) = table.strip_prefix("cloudflare.") else {
        return false;
    };
    let Some(overlay) = parent.strip_prefix("cloudflare.env.") else {
        return false;
    };
    overlay
        .split_once('.')
        .is_some_and(|(name, tail)| !name.is_empty() && tail == rest)
}

const SECRET_KEY_NEEDLES: &[&str] = &[
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "PRIVATE_KEY",
    "API_KEY",
    "ACCESS_KEY",
    "AUTH_KEY",
    "CREDENTIAL",
    "CONNECTION_STRING",
];

fn is_secret_named_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SECRET_KEY_NEEDLES
        .iter()
        .any(|needle| upper.contains(needle))
}

/// Turn blocking findings into an error. `None` when there are none.
#[must_use]
pub fn blocking_error(report: &SecretReport) -> Option<SecretError> {
    if report.blocks.is_empty() {
        None
    } else {
        Some(SecretError {
            findings: report.blocks.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::scan_table;

    fn scan(source: &str) -> super::SecretReport {
        let mut table: toml::Table = toml::from_str(source).expect("toml");
        scan_table(&mut table)
    }

    #[test]
    fn a_github_pat_is_a_block() {
        let report =
            scan("[cloudflare.vars]\nTOKEN = \"ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n");
        assert_eq!(report.blocks.len(), 1, "{report:?}");
        assert_eq!(report.blocks[0].kind, "GitHub personal access token");
        assert!(report.blocks[0].location.contains("TOKEN"));
    }

    #[test]
    fn a_placeholder_is_neither_a_block_nor_a_warning() {
        let report = scan("[cloudflare.vars]\nAPI_KEY = \"${API_KEY}\"\n");
        assert_eq!(report.blocks, []);
        assert_eq!(report.warnings, []);
    }

    #[test]
    fn a_secret_named_literal_that_is_not_a_known_form_is_a_warning() {
        let report = scan("[cloudflare.vars]\nAPI_KEY = \"dev-only\"\n");
        assert_eq!(report.blocks, []);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].kind, super::SECRET_NAMED_KEY);
    }

    #[test]
    fn a_postgres_url_with_a_password_is_a_block() {
        let report =
            scan("[cloudflare.vars]\nDATABASE = \"postgres://flyco:s3cret@127.0.0.1/app\"\n");
        assert_eq!(report.blocks[0].kind, "URL with a password");
    }

    #[test]
    fn a_url_without_a_password_is_clean() {
        let report = scan("[cloudflare.vars]\nAPI_URL = \"https://api.flyco.io\"\n");
        assert_eq!(report.blocks, []);
        assert_eq!(report.warnings, []);
    }

    #[test]
    fn an_rds_secret_arn_is_not_a_secret() {
        let report = scan(
            "[native.database.main]\nbackend = \"rds-data\"\n\
             secret_arn = \"arn:aws:secretsmanager:us-east-1:111122223333:secret:skyzen-Ab12Cd\"\n",
        );
        assert_eq!(report.blocks, []);
        assert_eq!(report.warnings, []);
    }

    #[test]
    fn a_pem_private_key_is_a_block() {
        let report = scan(
            "[cloudflare.vars]\nKEY = \"-----BEGIN PRIVATE KEY-----\\nMIIB\\n-----END PRIVATE KEY-----\"\n",
        );
        assert_eq!(report.blocks[0].kind, "PEM private key");
    }

    #[test]
    fn an_aws_access_key_id_is_a_block() {
        let report = scan("[aws.env]\nUSER = \"AKIAAAAAAAAAAAAAAAAA\"\n");
        assert_eq!(report.blocks[0].kind, "AWS access key id");
    }

    #[test]
    fn a_jwt_shaped_string_is_a_warning() {
        let report =
            scan("[cloudflare.vars]\nCLAIM = \"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.aaaa\"\n");
        assert_eq!(report.blocks, []);
        assert_eq!(report.warnings[0].kind, "JWT-shaped string");
    }

    #[test]
    fn findings_never_include_the_secret_value() {
        let report =
            scan("[cloudflare.vars]\nTOKEN = \"ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n");
        let rendered = report.blocks[0].to_string();
        assert!(!rendered.contains("ghp_"), "{rendered}");
    }
}
