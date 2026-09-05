//! The one resolver for the variables a deployment has to supply, and the `.env` files behind it.
//!
//! A manifest names variables in two ways: a `[[secret]]` entry declares one outright, and a
//! native wiring names one through `url_env`, `bucket_env`, `connection_env`, `sas_url_env` or a
//! backend whose constructor fixes its own names. [`runtime_variables`] collapses both into one
//! list, and [`Environment`] is the single place that decides where a value comes from — so the
//! CLI, `.env.example` and `skyzen doctor` cannot disagree about whether something is set.
//!
//! The precedence rule between the process environment and the `.env` files is stated once, in
//! `docs/skyzen-toml-reference.md#secrets`. The same sources fill `${NAME}` placeholders in
//! `Skyzen.toml` when the CLI reads it, which is deploy-time interpolation and not the runtime
//! environment of the deployed process: `#[skyzen::main]` does not expand them.

use crate::{output, secret_files};
use anyhow::{Context, Result};
use askama::Template;
use secrecy::{ExposeSecret, SecretString};
use skyzen_manifest::{InterpolateError, Manifest, SkyzenManifest, VarName, WiringEnvVar};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The dotenv files loaded, in order. A later file overrides an earlier one.
const DOTENV_FILES: &[&str] = &[".env", ".env.local"];

/// What a runtime variable is for, which decides which providers need it delivered.
///
/// A Worker backs its services with bindings rather than connection strings, so it never wants a
/// [`Wiring`](VariableKind::Wiring) variable; the Lambda and Functions binaries *are* the native
/// binary, so they want both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VariableKind {
    /// A `[[secret]]` entry: a value the deployment delivers, never written to the manifest.
    Secret,
    /// A variable a native wiring reads to reach its backend.
    Wiring,
}

impl VariableKind {
    /// Both kinds, for a provider that runs the native binary.
    pub const ALL: &'static [Self] = &[Self::Secret, Self::Wiring];

    /// How the kind is spelled in a report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Secret => "secret",
            Self::Wiring => "wiring",
        }
    }
}

/// An environment variable the manifest says the running application needs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuntimeVariable {
    /// The variable's name.
    pub name: VarName,
    /// The manifest entry that asked for it, for the error message.
    pub declared_by: String,
    /// What it is for.
    pub kind: VariableKind,
}

/// Every environment variable the manifest declares for a running application.
///
/// `[[secret]]` entries and native wiring variables in one list, sorted by name, because a caller
/// that has to check "is this set" does not care which of the two shapes named it.
#[must_use]
pub fn runtime_variables(manifest: &SkyzenManifest) -> Vec<RuntimeVariable> {
    let mut variables: Vec<RuntimeVariable> = manifest
        .secret
        .iter()
        .map(|secret| RuntimeVariable {
            declared_by: format!("[[secret]] {}", secret.name),
            name: secret.name.clone(),
            kind: VariableKind::Secret,
        })
        .collect();

    if let Some(native) = &manifest.native {
        for (name, service) in &native.service {
            collect(
                &mut variables,
                &format!("[native.service.{name}]"),
                service.backend().as_str(),
                service.env_vars(),
            );
        }
        for (name, database) in &native.database {
            collect(
                &mut variables,
                &format!("[native.database.{name}]"),
                database.backend().as_str(),
                database.env_vars(),
            );
        }
    }

    variables.sort();
    variables.dedup();
    variables
}

/// The runtime variables of the given kinds.
///
/// A Worker resolves its services from bindings, so it needs the secrets and nothing else; the
/// native, Lambda and Functions binaries read both. `skyzen migrate` opens one connection, so it
/// needs the wiring and has no business demanding a project's production secrets.
#[must_use]
pub fn runtime_variables_of(
    manifest: &SkyzenManifest,
    kinds: &[VariableKind],
) -> Vec<RuntimeVariable> {
    let mut variables = runtime_variables(manifest);
    variables.retain(|variable| kinds.contains(&variable.kind));
    variables
}

/// Record one wiring's variables, saying where each name came from.
///
/// A backend that fixes its own variable names — Cosmos DB's account endpoint and key, the four
/// the RDS Data API reads — is named instead of a key, because there is no key to correct.
fn collect(
    variables: &mut Vec<RuntimeVariable>,
    section: &str,
    backend: &str,
    named: Vec<WiringEnvVar<'_>>,
) {
    variables.extend(named.into_iter().map(|variable| RuntimeVariable {
        name: variable.name.clone(),
        declared_by: variable.key.map_or_else(
            || format!("{section} backend = \"{backend}\""),
            |key| format!("{section}.{key}"),
        ),
        kind: VariableKind::Wiring,
    }));
}

/// The values a project's `.env` files hold, and the rule for reading one.
///
/// Loaded once per command: every caller that needs a value — the interpolator, `skyzen dev`'s
/// child environment, `skyzen migrate`'s connection URL, `skyzen doctor`'s report — asks this
/// rather than reaching for `std::env` and a `BTreeMap` of its own.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// `.env` then `.env.local`, later wins.
    dotenv: BTreeMap<String, String>,
}

impl Environment {
    /// Load the project's dotenv files.
    ///
    /// # Errors
    ///
    /// Fails when a dotenv file exists but cannot be read or parsed — a silently ignored typo in
    /// `.env` is exactly the mystery this whole module exists to remove.
    pub fn load(root: &Path) -> Result<Self> {
        let mut dotenv = BTreeMap::new();
        for file in DOTENV_FILES {
            let path = root.join(file);
            if !path.exists() {
                continue;
            }
            let entries = dotenvy::from_path_iter(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            for entry in entries {
                let (key, value) =
                    entry.with_context(|| format!("failed to parse {}", path.display()))?;
                dotenv.insert(key, value);
            }
        }
        Ok(Self { dotenv })
    }

    /// The value of one variable: the process environment first, then the dotenv files.
    ///
    /// # Errors
    ///
    /// Fails when the process environment holds the name with a value that is not Unicode, which
    /// is a broken value rather than an absent one and must not be read as "unset".
    pub fn get(&self, name: &str) -> Result<Option<SecretString>, InterpolateError> {
        Ok(skyzen_manifest::process_env(name)?
            .or_else(|| self.dotenv.get(name).cloned())
            .map(SecretString::from))
    }

    /// The lookup `Manifest::load_with` expands `${NAME}` through.
    ///
    /// An interpolated value is by definition not a secret: it is written into the generated
    /// `wrangler.toml` or an ARM request, so it hands back a plain `String`.
    pub fn lookup(&self) -> impl Fn(&str) -> Result<Option<String>, InterpolateError> + '_ {
        move |name: &str| {
            Ok(skyzen_manifest::process_env(name)?.or_else(|| self.dotenv.get(name).cloned()))
        }
    }

    /// The dotenv entries a child process should be started with.
    ///
    /// Only the ones the process environment does not already hold: `Command::envs` overrides what
    /// the child inherits, so handing it the whole map would let `.env` beat a one-off
    /// `CACHE_URL=... skyzen dev` — the precedence backwards from everywhere else.
    #[must_use]
    pub fn child_overrides(&self) -> Vec<(String, String)> {
        self.dotenv
            .iter()
            .filter(|(name, _)| std::env::var_os(name).is_none())
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// Resolve every declared variable, or fail naming all the ones that are missing.
    ///
    /// All-or-nothing: a deployment that ships half its configuration fails at cold start, where
    /// the message is a panic in a log rather than a list of manifest keys.
    ///
    /// # Errors
    ///
    /// Fails listing every missing variable and the manifest entry that declared it, or when a
    /// value in the process environment is not Unicode.
    pub fn resolve(&self, variables: &[RuntimeVariable]) -> Result<ResolvedVariables> {
        let mut resolved = BTreeMap::new();
        let mut missing = Vec::new();

        for variable in variables {
            match self.get(variable.name.as_str())? {
                Some(value) => {
                    resolved.insert(variable.name.clone(), value);
                }
                None => missing.push(variable),
            }
        }

        if missing.is_empty() {
            return Ok(ResolvedVariables(resolved));
        }

        let details = missing
            .iter()
            .map(|variable| format!("  {} (declared by {})", variable.name, variable.declared_by))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "Skyzen.toml declares environment variables that are not set and are in no .env file:\n\
             {details}\n\
             Set them in the environment, or add them to .env (see .env.example)."
        )
    }

    /// The dotenv files that exist, for the watcher to notice edits to.
    #[must_use]
    pub fn paths(root: &Path) -> Vec<PathBuf> {
        DOTENV_FILES
            .iter()
            .map(|file| root.join(file))
            .filter(|path| path.exists())
            .collect()
    }
}

/// An [`Environment`] holding exactly `contents`, for tests across the crate.
///
/// It goes through the real loader — a temporary `.env` parsed by `dotenvy` — so a test cannot
/// accidentally exercise a map the CLI would never have produced.
#[cfg(test)]
pub fn from_dotenv(contents: &str) -> Environment {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join(DOTENV_FILES[0]), contents).expect("write .env");
    Environment::load(dir.path()).expect("load")
}

/// Every declared variable with the value that was found for it.
///
/// The values stay wrapped, so a provider that puts one on a command line or in a log has to write
/// `expose_secret()` to do it — which is what makes the leak visible in a diff.
#[derive(Debug, Default)]
pub struct ResolvedVariables(BTreeMap<VarName, SecretString>);

impl ResolvedVariables {
    /// The variables, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&VarName, &SecretString)> {
        self.0.iter()
    }

    /// The names alone, which is all a report may print.
    pub fn names(&self) -> impl Iterator<Item = &VarName> {
        self.0.keys()
    }

    /// Whether nothing was declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'a> IntoIterator for &'a ResolvedVariables {
    type Item = (&'a VarName, &'a SecretString);
    type IntoIter = std::collections::btree_map::Iter<'a, VarName, SecretString>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Read `Skyzen.toml`, expanding `${NAME}` from the process environment and then `.env` files.
///
/// # Errors
///
/// Fails when the file cannot be read, a placeholder cannot be expanded, a documented
/// credential form is stored as a literal, a dotenv file is tracked by git, a dotenv file is
/// malformed, or the document does not match the schema.
pub fn load_manifest(path: &Path) -> Result<Manifest> {
    Ok(load_manifest_with(path)?.0)
}

/// Read `Skyzen.toml` and hand back the [`Environment`] it was read through.
///
/// The dotenv files are read once per command: a caller that also has to resolve runtime variables
/// takes the environment from here rather than loading the same files a second time.
///
/// # Errors
///
/// As [`load_manifest`].
pub fn load_manifest_with(path: &Path) -> Result<(Manifest, Environment)> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to read the current directory")?
            .join(path)
    };
    let root_dir = absolute.parent().unwrap_or_else(|| Path::new("."));
    let environment = Environment::load(root_dir)?;
    let manifest = {
        let lookup = environment.lookup();
        Manifest::load_with(&absolute, Some(&lookup))?
    };
    for finding in manifest.secret_warnings() {
        output::warn(format!(
            "{finding}; if this is a secret, use a ${{NAME}} placeholder or `skyzen secret set`"
        ));
    }
    for warning in secret_files::ensure(root_dir)? {
        output::warn(warning);
    }
    Ok((manifest, environment))
}

/// Render the `.env.example` a scaffolded project ships.
///
/// # Errors
///
/// Fails only when the template itself cannot render.
pub fn render_example(manifest: &SkyzenManifest) -> Result<String> {
    let mut variables = runtime_variables(manifest);
    // Secrets first: they are the ones a first run has to be told about, and the wiring variables
    // usually already have a value in the developer's shell.
    variables.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
    });
    EnvExampleTemplate { variables }
        .render()
        .context("failed to render .env.example")
}

#[derive(Template)]
#[template(path = "env.example.tmpl", escape = "none")]
struct EnvExampleTemplate {
    variables: Vec<RuntimeVariable>,
}

/// Expose a resolved value at the one place that sends it somewhere.
///
/// A free function rather than a method so that every exposure reads the same way in a diff.
#[must_use]
pub fn expose(value: &SecretString) -> &str {
    value.expose_secret()
}

/// A second, owned handle on a resolved value.
///
/// `SecretString` is deliberately not `Clone`, which is what stops a value being copied about
/// casually. A sink that has to *own* one — a command whose standard input carries it — still
/// needs a copy, and the copy zeroizes itself like the original, so this is the one way to make
/// one and it reads the same way in a diff as [`expose`].
#[must_use]
pub fn duplicate(value: &SecretString) -> SecretString {
    SecretString::from(value.expose_secret().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        from_dotenv, load_manifest, render_example, runtime_variables, Environment, VariableKind,
    };
    use skyzen_manifest::{Manifest, VarName};

    fn manifest(source: &str) -> skyzen_manifest::SkyzenManifest {
        Manifest::parse(source, "Skyzen.toml", ".")
            .expect("valid manifest")
            .data()
            .clone()
    }

    const WIRED: &str = "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
         [[service]]\nname = \"uploads\"\ntype = \"storage\"\n\n\
         [[database]]\nname = \"main\"\ntype = \"sql\"\n\n\
         [native.service.cache]\nbackend = \"redis\"\nurl_env = \"CACHE_URL\"\n\n\
         [native.service.uploads]\nbackend = \"s3\"\nbucket_env = \"UPLOADS_BUCKET\"\n\n\
         [native.database.main]\nbackend = \"postgres\"\nurl_env = \"DATABASE_URL\"\n";

    /// A name no test machine has exported, so "unset" means unset.
    const UNSET: &str = "SKYZEN_TEST_DEFINITELY_UNSET";

    #[test]
    fn collects_every_declared_variable_with_the_key_that_declared_it() {
        let variables = runtime_variables(&manifest(WIRED));
        let names: Vec<_> = variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["CACHE_URL", "DATABASE_URL", "UPLOADS_BUCKET"]);
        assert!(variables[0].declared_by.contains("[native.service.cache]"));
        assert!(variables.iter().all(|v| v.kind == VariableKind::Wiring));
    }

    #[test]
    fn a_declared_secret_is_a_runtime_variable_naming_its_entry() {
        let variables = runtime_variables(&manifest(
            "[[secret]]\nname = \"STRIPE_KEY\"\n\n[[secret]]\nname = \"JWT_SIGNING_KEY\"\n",
        ));
        let names: Vec<_> = variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["JWT_SIGNING_KEY", "STRIPE_KEY"]);
        assert!(variables.iter().all(|v| v.kind == VariableKind::Secret));
        assert_eq!(variables[1].declared_by, "[[secret]] STRIPE_KEY");
    }

    #[test]
    fn a_backend_that_fixes_its_own_variables_names_the_backend_rather_than_a_key() {
        let variables = runtime_variables(&manifest(
            "[[service]]\nname = \"sessions\"\ntype = \"kv\"\n\n\
             [native.service.sessions]\nbackend = \"cosmos\"\n\
             database = \"appdb\"\ncontainer = \"sessions\"\n\n\
             [[database]]\nname = \"main\"\ntype = \"sql\"\n\n\
             [native.database.main]\nbackend = \"rds-data\"\n",
        ));

        let names: Vec<_> = variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "AZURE_COSMOS_ENDPOINT",
                "AZURE_COSMOS_KEY",
                "RDS_DATABASE",
                "RDS_ENGINE",
                "RDS_RESOURCE_ARN",
                "RDS_SECRET_ARN",
            ]
        );
        assert_eq!(
            variables[0].declared_by,
            "[native.service.sessions] backend = \"cosmos\""
        );
        assert_eq!(
            variables[2].declared_by,
            "[native.database.main] backend = \"rds-data\""
        );
    }

    #[test]
    fn an_rds_data_wiring_that_names_its_values_declares_no_variables_to_set() {
        // The manifest replaced the four variables, so demanding them would refuse to start a
        // deployment whose configuration is complete.
        let variables = runtime_variables(&manifest(
            "[[database]]\nname = \"main\"\ntype = \"sql\"\n\n\
             [native.database.main]\nbackend = \"rds-data\"\n\
             resource_arn = \"arn:aws:rds:us-east-1:111122223333:cluster:skyzen\"\n\
             secret_arn = \"arn:aws:secretsmanager:us-east-1:111122223333:secret:skyzen-Ab12Cd\"\n\
             database = \"appdb\"\nengine = \"aurora-postgresql\"\n",
        ));
        assert!(variables.is_empty(), "{variables:?}");
    }

    #[test]
    fn an_azure_sql_wiring_declares_the_variable_holding_its_connection_string() {
        let variables = runtime_variables(&manifest(
            "[[database]]\nname = \"main\"\ntype = \"sql\"\n\n\
             [native.database.main]\nbackend = \"azure-sql\"\n\
             url_env = \"AZURE_SQL_CONNECTION_STRING\"\n",
        ));

        assert_eq!(variables[0].name, "AZURE_SQL_CONNECTION_STRING");
        assert_eq!(variables[0].declared_by, "[native.database.main].url_env");
    }

    #[test]
    fn a_backend_whose_variable_the_manifest_names_reports_the_key() {
        let variables = runtime_variables(&manifest(
            "[[service]]\nname = \"jobs\"\ntype = \"queue\"\n\n\
             [native.service.jobs]\nbackend = \"storage-queue\"\nsas_url_env = \"JOBS_SAS_URL\"\n",
        ));

        assert_eq!(variables[0].name, "JOBS_SAS_URL");
        assert_eq!(
            variables[0].declared_by,
            "[native.service.jobs].sas_url_env"
        );
    }

    #[test]
    fn a_backend_reached_through_an_ambient_credential_chain_declares_nothing() {
        // DynamoDB's credentials and region come from the AWS chain, which has its own sources —
        // naming one of them would refuse to start on a machine using another.
        let variables = runtime_variables(&manifest(
            "[[service]]\nname = \"sessions\"\ntype = \"kv\"\n\n\
             [native.service.sessions]\nbackend = \"dynamodb\"\ntable = \"skyzen-sessions\"\n",
        ));
        assert!(variables.is_empty(), "{variables:?}");
    }

    #[test]
    fn a_memory_backend_declares_nothing() {
        let variables = runtime_variables(&manifest(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [native.service.cache]\nbackend = \"memory\"\n",
        ));
        assert!(variables.is_empty(), "{variables:?}");
    }

    #[test]
    fn resolve_lists_every_missing_variable_with_the_entry_that_declared_it() {
        let variables = runtime_variables(&manifest(&format!(
            "[[secret]]\nname = \"{UNSET}\"\n\n{WIRED}"
        )));
        let error = Environment::default()
            .resolve(&variables)
            .expect_err("nothing is set");
        let rendered = error.to_string();
        assert!(rendered.contains("CACHE_URL"), "{rendered}");
        assert!(rendered.contains("UPLOADS_BUCKET"), "{rendered}");
        assert!(rendered.contains("DATABASE_URL"), "{rendered}");
        assert!(
            rendered.contains("[native.service.cache].url_env"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("[[secret]] {UNSET}")),
            "{rendered}"
        );
    }

    #[test]
    fn a_dotenv_entry_satisfies_a_declared_variable() {
        let variables = runtime_variables(&manifest(&format!(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [native.service.cache]\nbackend = \"redis\"\nurl_env = \"{UNSET}\"\n"
        )));
        let environment = from_dotenv(&format!("{UNSET}=redis://127.0.0.1:6379\n"));
        let resolved = environment
            .resolve(&variables)
            .expect("the dotenv entry supplies it");
        let names: Vec<_> = resolved.names().map(VarName::as_str).collect();
        assert_eq!(names, [UNSET]);
    }

    #[test]
    fn later_dotenv_files_override_earlier_ones() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(".env"), "SHARED=base\nONLY_BASE=1\n").expect("write .env");
        std::fs::write(dir.path().join(".env.local"), "SHARED=local\n").expect("write .env.local");

        let environment = Environment::load(dir.path()).expect("load");
        assert_eq!(
            super::expose(&environment.get("SHARED").expect("read").expect("set")),
            "local"
        );
        assert_eq!(
            super::expose(&environment.get("ONLY_BASE").expect("read").expect("set")),
            "1"
        );
    }

    #[test]
    fn the_process_environment_beats_the_dotenv_files() {
        const NAME: &str = "SKYZEN_TEST_PROCESS_WINS";
        // SAFETY: single-threaded test process, and the variable is unique to this test.
        unsafe { std::env::set_var(NAME, "from_process") };
        let environment = from_dotenv(&format!("{NAME}=from_dotenv\n"));

        assert_eq!(
            super::expose(&environment.get(NAME).expect("read").expect("set")),
            "from_process"
        );
        assert!(
            !environment
                .child_overrides()
                .iter()
                .any(|(name, _)| name == NAME),
            "a name the process environment already holds must not be overridden for the child"
        );

        unsafe { std::env::remove_var(NAME) };
    }

    #[test]
    fn child_overrides_carry_the_names_the_process_environment_lacks() {
        let environment = from_dotenv(&format!("{UNSET}=from_dotenv\n"));
        assert_eq!(
            environment.child_overrides(),
            vec![(UNSET.to_owned(), "from_dotenv".to_owned())]
        );
    }

    #[test]
    fn the_example_names_every_variable_and_where_it_came_from() {
        let rendered = render_example(&manifest(WIRED)).expect("render");
        assert!(rendered.contains("CACHE_URL="), "{rendered}");
        assert!(rendered.contains("UPLOADS_BUCKET="), "{rendered}");
        assert!(rendered.contains("DATABASE_URL="), "{rendered}");
        assert!(
            rendered.contains("# [native.database.main].url_env"),
            "{rendered}"
        );
    }

    #[test]
    fn the_example_lists_declared_secrets_before_the_wiring_variables() {
        let rendered = render_example(&manifest(&format!(
            "[[secret]]\nname = \"STRIPE_KEY\"\n\n{WIRED}"
        )))
        .expect("render");
        assert!(rendered.contains("STRIPE_KEY="), "{rendered}");
        assert!(rendered.contains("# [[secret]] STRIPE_KEY"), "{rendered}");
        let secret = rendered.find("STRIPE_KEY=").expect("the secret");
        let wiring = rendered.find("CACHE_URL=").expect("the wiring variable");
        assert!(secret < wiring, "{rendered}");
    }

    #[test]
    fn the_example_explains_itself_when_nothing_is_declared() {
        let rendered = render_example(&manifest("")).expect("render");
        assert!(rendered.contains("declares no runtime environment variables"));
    }

    #[test]
    fn load_manifest_interpolates_from_dotenv() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("Skyzen.toml"),
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\
             account_id = \"${SKYZEN_TEST_ACCOUNT_ID}\"\n",
        )
        .expect("write manifest");
        std::fs::write(
            dir.path().join(".env"),
            "SKYZEN_TEST_ACCOUNT_ID=acct_from_dotenv\n",
        )
        .expect("write dotenv");

        let loaded = load_manifest(&dir.path().join("Skyzen.toml")).expect("load");
        assert_eq!(
            loaded
                .data()
                .cloudflare
                .as_ref()
                .expect("cloudflare")
                .account_id
                .as_deref(),
            Some("acct_from_dotenv")
        );
    }

    #[test]
    fn load_manifest_fails_when_a_placeholder_is_unset() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("Skyzen.toml"),
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\
             account_id = \"${SKYZEN_TEST_UNSET_INTERPOLATION}\"\n",
        )
        .expect("write manifest");

        let error = load_manifest(&dir.path().join("Skyzen.toml")).expect_err("unset");
        let rendered = error.to_string();
        assert!(
            rendered.contains("SKYZEN_TEST_UNSET_INTERPOLATION"),
            "{rendered}"
        );
        assert!(rendered.contains("account_id"), "{rendered}");
    }

    #[test]
    fn load_manifest_blocks_a_github_token_without_echoing_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("Skyzen.toml"),
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.vars]\nTOKEN = \"ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
        )
        .expect("write manifest");

        let error = load_manifest(&dir.path().join("Skyzen.toml")).expect_err("block");
        let rendered = error.to_string();
        assert!(
            rendered.contains("GitHub personal access token"),
            "{rendered}"
        );
        assert!(!rendered.contains("ghp_"), "{rendered}");
    }
}
