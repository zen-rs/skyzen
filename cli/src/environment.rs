//! Environment variables the manifest's native wiring reads, and the `.env` files that supply them.
//!
//! The variables a wiring names — `url_env`, `bucket_env`, `connection_env`, `sas_url_env` — were
//! parsed and then discarded: only the proc-macro read them, at compile time, so declaring
//! `url_env = "CACHE_URL"` and forgetting to export it produced a panic at the first request
//! rather than a message at startup. `skyzen dev` now loads the `.env` files, checks the declared
//! variables are present, and hands the result to the child process — never to the CLI's own
//! environment, so nothing global is mutated.
//!
//! A backend whose constructor fixes its own variable names (Cosmos DB, the RDS Data API) is
//! covered too: the section reports those names, so they reach this check and `.env.example` the
//! same way a key-named one does.

use anyhow::{Context, Result};
use askama::Template;
use skyzen_manifest::{SkyzenManifest, WiringEnvVar};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The dotenv files loaded, in order. A later file overrides an earlier one, and both lose to a
/// variable already present in the CLI's own environment — the usual precedence, so a one-off
/// `CACHE_URL=... skyzen dev` still wins.
const DOTENV_FILES: &[&str] = &[".env", ".env.local"];

/// An environment variable the manifest says a native run needs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequiredVariable {
    /// The variable's name.
    pub name: String,
    /// The manifest key that asked for it, for the error message.
    pub declared_by: String,
}

/// Every environment variable the manifest's native wiring reads at startup.
pub fn required_variables(manifest: &SkyzenManifest) -> Vec<RequiredVariable> {
    let mut variables = Vec::new();
    let Some(native) = &manifest.native else {
        return variables;
    };

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

    variables.sort();
    variables.dedup();
    variables
}

/// Record one wiring's variables, saying where each name came from.
///
/// A backend that fixes its own variable names — Cosmos DB's account endpoint and key, the four
/// the RDS Data API reads — is named instead of a key, because there is no key to correct.
fn collect(
    variables: &mut Vec<RequiredVariable>,
    section: &str,
    backend: &str,
    named: Vec<WiringEnvVar<'_>>,
) {
    variables.extend(named.into_iter().map(|variable| RequiredVariable {
        name: variable.name.to_owned(),
        declared_by: variable.key.map_or_else(
            || format!("{section} backend = \"{backend}\""),
            |key| format!("{section}.{key}"),
        ),
    }));
}

/// The variables loaded from the project's `.env` files.
///
/// # Errors
///
/// Fails when a dotenv file exists but cannot be read or parsed — a silently ignored typo in
/// `.env` is exactly the mystery this whole module exists to remove.
pub fn load_dotenv_files(root_dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut loaded = BTreeMap::new();
    for file in DOTENV_FILES {
        let path = root_dir.join(file);
        if !path.exists() {
            continue;
        }
        let entries = dotenvy::from_path_iter(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for entry in entries {
            let (key, value) =
                entry.with_context(|| format!("failed to parse {}", path.display()))?;
            loaded.insert(key, value);
        }
    }
    Ok(loaded)
}

/// Fail when a variable the manifest declares is available from neither the environment nor the
/// dotenv files.
///
/// # Errors
///
/// Fails listing every missing variable and the manifest key that declared it.
pub fn ensure_available(
    required: &[RequiredVariable],
    dotenv: &BTreeMap<String, String>,
) -> Result<()> {
    let missing: Vec<&RequiredVariable> = required
        .iter()
        .filter(|variable| {
            !dotenv.contains_key(&variable.name) && std::env::var_os(&variable.name).is_none()
        })
        .collect();

    if missing.is_empty() {
        return Ok(());
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

/// Render the `.env.example` a scaffolded project ships.
///
/// # Errors
///
/// Fails only when the template itself cannot render.
pub fn render_example(manifest: &SkyzenManifest) -> Result<String> {
    EnvExampleTemplate {
        variables: required_variables(manifest),
    }
    .render()
    .context("failed to render .env.example")
}

#[derive(Template)]
#[template(path = "env.example.tmpl", escape = "none")]
struct EnvExampleTemplate {
    variables: Vec<RequiredVariable>,
}

/// The dotenv files, for the watcher to notice edits to.
pub fn dotenv_paths(root_dir: &Path) -> Vec<PathBuf> {
    DOTENV_FILES
        .iter()
        .map(|file| root_dir.join(file))
        .filter(|path| path.exists())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ensure_available, load_dotenv_files, render_example, required_variables};
    use skyzen_manifest::Manifest;
    use std::collections::BTreeMap;

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

    #[test]
    fn collects_every_declared_variable_with_the_key_that_declared_it() {
        let variables = required_variables(&manifest(WIRED));
        let names: Vec<_> = variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["CACHE_URL", "DATABASE_URL", "UPLOADS_BUCKET"]);
        assert!(variables[0].declared_by.contains("[native.service.cache]"));
    }

    #[test]
    fn a_backend_that_fixes_its_own_variables_names_the_backend_rather_than_a_key() {
        let variables = required_variables(&manifest(
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
    fn a_backend_whose_variable_the_manifest_names_reports_the_key() {
        let variables = required_variables(&manifest(
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
        let variables = required_variables(&manifest(
            "[[service]]\nname = \"sessions\"\ntype = \"kv\"\n\n\
             [native.service.sessions]\nbackend = \"dynamodb\"\ntable = \"skyzen-sessions\"\n",
        ));
        assert!(variables.is_empty(), "{variables:?}");
    }

    #[test]
    fn a_memory_backend_declares_nothing() {
        let variables = required_variables(&manifest(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [native.service.cache]\nbackend = \"memory\"\n",
        ));
        assert!(variables.is_empty(), "{variables:?}");
    }

    #[test]
    fn a_missing_variable_is_reported_with_its_manifest_key() {
        let variables = required_variables(&manifest(WIRED));
        let error = ensure_available(&variables, &BTreeMap::new()).expect_err("nothing is set");
        let rendered = error.to_string();
        assert!(rendered.contains("CACHE_URL"), "{rendered}");
        assert!(
            rendered.contains("[native.service.cache].url_env"),
            "{rendered}"
        );
    }

    #[test]
    fn a_dotenv_entry_satisfies_a_declared_variable() {
        let variables = required_variables(&manifest(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [native.service.cache]\nbackend = \"redis\"\nurl_env = \"SKYZEN_TEST_CACHE_URL\"\n",
        ));
        let dotenv = BTreeMap::from([(
            "SKYZEN_TEST_CACHE_URL".to_owned(),
            "redis://127.0.0.1:6379".to_owned(),
        )]);
        ensure_available(&variables, &dotenv).expect("the dotenv entry supplies it");
    }

    #[test]
    fn later_dotenv_files_override_earlier_ones() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(".env"), "SHARED=base\nONLY_BASE=1\n").expect("write .env");
        std::fs::write(dir.path().join(".env.local"), "SHARED=local\n").expect("write .env.local");

        let loaded = load_dotenv_files(dir.path()).expect("load");
        assert_eq!(loaded["SHARED"], "local");
        assert_eq!(loaded["ONLY_BASE"], "1");
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
    fn the_example_explains_itself_when_nothing_is_declared() {
        let rendered = render_example(&manifest("")).expect("render");
        assert!(rendered.contains("declares no native environment variables"));
    }
}
