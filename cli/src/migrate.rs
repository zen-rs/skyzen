//! `skyzen migrate` against a native database.
//!
//! The Cloudflare path shells out to `wrangler d1 migrations apply` and is planned like every
//! other provider action. This path cannot be: there is no external tool to run, only a connection
//! to open and a set of files to apply, so it does the work in process.
//!
//! # Why the CLI reads the directory rather than the binary
//!
//! An application embeds its migrations with `skyzen::embed_migrations!`, but the CLI cannot see
//! inside a crate it has not compiled. It therefore reads the *same directory* — the one
//! `[[database]].migrations_dir` names, defaulting to `migrations/` — through the *same* reader the
//! macro uses, `skyzen_manifest::migrations`. Versions, names and checksums are computed
//! identically on both paths, so `skyzen migrate` cannot record a checksum the application would
//! then reject as edited history, and the two cannot disagree about which files count.

use crate::{
    cli::{MigrateCommand, Provider},
    environment::{ensure_available, load_dotenv_files, required_variables},
    output,
};
use anyhow::{Context, Result};
use skyzen_manifest::{
    migrations::MigrationFile, DatabaseEntry, Manifest, NativeDatabaseBackend,
    NativeDatabaseSection,
};
use skyzen_services::{Db, Migration, Migrations};
use std::{collections::BTreeMap, path::PathBuf};

/// One `[[database]]` entry with everything the runner needs resolved.
#[derive(Debug)]
struct Target {
    /// The logical name from `[[database]]`, for output.
    name: String,
    /// The driver its `[native.database.<name>]` names.
    backend: NativeDatabaseBackend,
    /// The environment variable holding the connection URL.
    url_env: String,
    /// The migrations directory, resolved against the project root.
    directory: PathBuf,
    /// The files that directory holds, already validated and checksummed.
    files: Vec<MigrationFile>,
}

impl Target {
    /// The embedded-equivalent set, built from the files on disk.
    fn migrations(&self) -> Migrations {
        Migrations::from_owned(
            self.files
                .iter()
                .map(|file| {
                    Migration::owned(
                        file.version,
                        file.name.clone(),
                        file.sql.clone(),
                        file.checksum,
                    )
                })
                .collect(),
        )
    }
}

/// Which `skyzen migrate` implementation an invocation selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Open a connection and apply the files in process, through this module.
    Native,
    /// Hand the action to the provider's own tooling — `wrangler d1 migrations apply` for D1.
    Provider,
}

/// Decide which implementation runs, refusing the combinations that would quietly do something
/// other than what was asked.
///
/// `migrate` is the one action with two genuinely different implementations rather than two
/// renderings of one plan, so the choice is made here rather than inside a provider plan. Applying
/// and reporting take the same branch: `--provider native` runs in process, anything else is the
/// provider's own tooling (`wrangler d1 migrations apply` / `... list` for D1), so `status` reports
/// on whichever database `migrate` would have written to.
///
/// # Errors
///
/// Fails when `--local` is combined with the native path: `--local` selects a provider's emulator,
/// and the native runner has none to select.
pub fn dispatch(provider: Option<Provider>, local: bool) -> Result<Dispatch> {
    if provider != Some(Provider::Native) {
        return Ok(Dispatch::Provider);
    }
    if local {
        anyhow::bail!(
            "`--local` selects the provider's local emulator and has no native meaning; point the \
             database's url_env at a local database instead"
        );
    }

    Ok(Dispatch::Native)
}

/// Run `skyzen migrate` (or `skyzen migrate status`) natively.
///
/// # Errors
///
/// Fails when the manifest cannot be read, when it wires no native SQL database, when a migrations
/// directory is missing or malformed, when a declared connection variable is set nowhere, or when
/// a migration does not apply.
pub fn run(
    manifest_path: &std::path::Path,
    command: Option<MigrateCommand>,
    dry_run: bool,
) -> Result<()> {
    let manifest = Manifest::load(manifest_path)?;
    let targets = resolve_targets(&manifest)?;

    // A dry run neither connects nor creates anything: it reads and validates the directories and
    // says what would run. Connecting would be a side effect the flag exists to promise against.
    if dry_run {
        for target in &targets {
            report_plan(target);
        }
        return Ok(());
    }

    let dotenv = load_dotenv_files(manifest.root_dir())
        .context("failed to load the project's .env files")?;
    ensure_available(&required_variables(manifest.data()), &dotenv)?;

    // A current-thread runtime: this is one connection doing a handful of statements, so a thread
    // pool would be pure startup cost. `enable_all` is what gives sqlx's TCP drivers a reactor.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime the database drivers need")?;

    runtime.block_on(async {
        for target in &targets {
            run_target(target, &dotenv, command).await?;
        }
        Ok(())
    })
}

/// Apply or report one database's migrations.
async fn run_target(
    target: &Target,
    dotenv: &BTreeMap<String, String>,
    command: Option<MigrateCommand>,
) -> Result<()> {
    let url = connection_url(target, dotenv)?;
    let migrations = target.migrations();

    output::step(format!(
        "database `{}` ({}): {} migration(s) in {}",
        target.name,
        target.backend.as_str(),
        migrations.len(),
        target.directory.display()
    ));

    let db = connect(target, &url).await?;

    match command {
        Some(MigrateCommand::Status) => report_status(target, &db, &migrations).await,
        None => apply(target, &db, &migrations).await,
    }
}

/// Apply the pending migrations and say what happened.
async fn apply(target: &Target, db: &Db, migrations: &Migrations) -> Result<()> {
    let report = db
        .migrate(migrations)
        .await
        .with_context(|| format!("failed to migrate database `{}`", target.name))?;

    if report.is_empty() {
        output::ok(format!(
            "database `{}` is up to date ({} already applied)",
            target.name, report.skipped
        ));
        return Ok(());
    }

    for version in &report.applied {
        let name = migrations
            .get(*version)
            .map_or("?", skyzen_services::Migration::name);
        output::ok(format!("applied {version} ({name})"));
    }
    output::ok(format!(
        "database `{}`: {} applied, {} already up to date",
        target.name,
        report.applied.len(),
        report.skipped
    ));
    Ok(())
}

/// Print what has been applied and what has not.
async fn report_status(target: &Target, db: &Db, migrations: &Migrations) -> Result<()> {
    let status = db
        .migration_status(migrations)
        .await
        .with_context(|| format!("failed to read migration status for `{}`", target.name))?;

    for row in &status.applied {
        // A version the directory no longer holds is worth seeing: it means the database is ahead
        // of this checkout, which is exactly what a rollback looks like.
        let known = if migrations.get(row.version).is_some() {
            ""
        } else {
            " (not in this checkout)"
        };
        output::ok(format!(
            "applied  {} {} at {}{known}",
            row.version, row.name, row.applied_at
        ));
    }
    for migration in &status.pending {
        output::step(format!(
            "pending  {} {}",
            migration.version(),
            migration.name()
        ));
    }
    output::step(format!(
        "database `{}`: {} applied, {} pending",
        target.name,
        status.applied.len(),
        status.pending.len()
    ));
    Ok(())
}

/// Say what a real run would do, without opening a connection.
fn report_plan(target: &Target) {
    output::dry_run(format!(
        "database `{}` ({} via {}): apply {} migration(s) from {}",
        target.name,
        target.backend.as_str(),
        target.url_env,
        target.files.len(),
        target.directory.display()
    ));
    for file in &target.files {
        output::dry_run(format!("  {} {}", file.version, file.name));
    }
}

/// Open the connection its backend calls for.
async fn connect(target: &Target, url: &str) -> Result<Db> {
    let db = match target.backend {
        NativeDatabaseBackend::Postgres => Db::connect_postgres(url).await,
        NativeDatabaseBackend::Mysql => Db::connect_mysql(url).await,
        NativeDatabaseBackend::Sqlite => Db::connect_sqlite(url).await,
        // Never reached: a database with no connection URL is refused by `resolve_targets`, which
        // is where the explanation belongs. Reported rather than unreachable so that adding a
        // backend to the schema cannot turn into a panic here.
        NativeDatabaseBackend::RdsData => anyhow::bail!(
            "database `{}` is reached through the RDS Data API, which is an HTTP service rather \
             than a connection this runner can open",
            target.name
        ),
        NativeDatabaseBackend::AzureSql => anyhow::bail!(
            "database `{}` is an Azure SQL database, which speaks TDS; this runner links sqlx, \
             which has no T-SQL driver. Apply its migrations from the application with \
             `Db::migrate`",
            target.name
        ),
    };

    db.with_context(|| {
        format!(
            "failed to connect to database `{}` using {} (from {})",
            target.name,
            target.backend.as_str(),
            target.url_env
        )
    })
}

/// The connection URL for `target`, preferring the real environment over the dotenv files.
fn connection_url(target: &Target, dotenv: &BTreeMap<String, String>) -> Result<String> {
    std::env::var(&target.url_env)
        .ok()
        .or_else(|| dotenv.get(&target.url_env).cloned())
        .with_context(|| {
            format!(
                "{} is not set; it holds the connection URL for database `{}` \
                 ([native.database.{}].url_env)",
                target.url_env, target.name, target.name
            )
        })
}

/// Resolve every `[[database]]` that has native wiring into a [`Target`].
///
/// Every wired database, not just the default one, so this matches the Cloudflare path — which
/// runs one `wrangler d1 migrations apply` per declared database — rather than quietly migrating
/// one of several.
fn resolve_targets(manifest: &Manifest) -> Result<Vec<Target>> {
    let databases = &manifest.data().database;
    if databases.is_empty() {
        anyhow::bail!(
            "{} declares no [[database]]; there is nothing to migrate",
            manifest.path().display()
        );
    }

    let native = manifest
        .data()
        .native
        .as_ref()
        .map(|native| &native.database);

    let mut targets = Vec::new();
    let mut unwired = Vec::new();
    let mut unreachable = Vec::new();
    for database in databases {
        match native.and_then(|wiring| wiring.get(&database.name)) {
            // A backend with no connection URL is one this runner cannot open: it links sqlx and
            // its drivers, and the RDS Data API is an HTTP service reached by ARN.
            Some(section) if section.url_env().is_none() => {
                unreachable.push((database.name.clone(), section.backend().as_str()));
            }
            Some(section) => targets.push(target_for(manifest, database, section)?),
            None => unwired.push(database.name.clone()),
        }
    }

    if targets.is_empty() {
        anyhow::bail!(
            "no [[database]] in {} has a [native.database.<name>] wiring this runner can open a \
             connection through (no wiring: {}; wired to a backend that is not a connection: {}). \
             Add one, apply the migrations from the application itself with `Db::migrate` (see \
             docs/migrations.md), or migrate through the provider that hosts the database — for \
             Cloudflare D1 that is `skyzen migrate --provider cloudflare`, which is also where \
             `skyzen migrate status` reports from.",
            manifest.path().display(),
            list(&unwired),
            list(
                &unreachable
                    .iter()
                    .map(|(name, backend)| format!("{name} ({backend})"))
                    .collect::<Vec<_>>()
            ),
        );
    }
    for name in unwired {
        output::warn(format!(
            "database `{name}` has no [native.database.{name}] wiring; skipping it"
        ));
    }
    for (name, backend) in unreachable {
        output::warn(format!(
            "database `{name}` is wired to `{backend}`, which this runner cannot open a connection \
             to; skipping it. Apply its migrations from the application with `Db::migrate`."
        ));
    }

    Ok(targets)
}

/// A comma-separated list, or `none` when there is nothing to list.
fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// Read one database's migrations directory.
fn target_for(
    manifest: &Manifest,
    database: &DatabaseEntry,
    section: &NativeDatabaseSection,
) -> Result<Target> {
    let directory = manifest.root_dir().join(database.migrations_dir());
    let files = skyzen_manifest::migrations::load(&directory).with_context(|| {
        format!(
            "failed to read the migrations for database `{}`",
            database.name
        )
    })?;

    Ok(Target {
        name: database.name.clone(),
        backend: section.backend(),
        url_env: section
            .url_env()
            .context("a database with no connection URL is not resolved into a target")?
            .to_owned(),
        directory,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::{connection_url, dispatch, resolve_targets, run, Dispatch, Target};
    use crate::cli::{MigrateCommand, Provider};
    use skyzen_manifest::{Manifest, NativeDatabaseBackend};
    use std::{collections::BTreeMap, fs, path::Path};

    /// A manifest wiring one SQLite database whose URL comes from `variable`.
    fn manifest_source(variable: &str) -> String {
        format!(
            "[[database]]\nname = \"main\"\ntype = \"sql\"\ndefault = true\n\n\
             [native.database.main]\nbackend = \"sqlite\"\nurl_env = \"{variable}\"\n"
        )
    }

    /// Write a project with a manifest and a migrations directory, and return its root.
    fn project(dir: &Path, variable: &str, migrations_dir: Option<&str>) -> std::path::PathBuf {
        let mut source = manifest_source(variable);
        if let Some(custom) = migrations_dir {
            source = source.replace(
                "default = true\n",
                &format!("default = true\nmigrations_dir = \"{custom}\"\n"),
            );
        }
        let manifest_path = dir.join("Skyzen.toml");
        fs::write(&manifest_path, source).expect("write manifest");

        let migrations = dir.join(migrations_dir.unwrap_or("migrations"));
        fs::create_dir_all(&migrations).expect("create migrations dir");
        fs::write(
            migrations.join("0001_create_users.sql"),
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);\n",
        )
        .expect("write migration");
        fs::write(
            migrations.join("0002_seed_users.sql"),
            "INSERT INTO users (id, email) VALUES (1, 'ada@example.invalid');\n\
             INSERT INTO users (id, email) VALUES (2, 'semi;colon@example.invalid');\n",
        )
        .expect("write migration");

        manifest_path
    }

    /// `?mode=rwc` is what makes sqlx create the file; without it a fresh path is a connect error.
    fn sqlite_url(dir: &Path) -> String {
        format!("sqlite://{}?mode=rwc", dir.join("app.db").display())
    }

    fn load(manifest_path: &Path) -> Manifest {
        Manifest::load(manifest_path).expect("manifest parses")
    }

    #[test]
    fn every_wired_database_becomes_a_target_with_its_files_in_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manifest_path = project(dir.path(), "SKYZEN_TEST_DB_URL", None);
        let targets = resolve_targets(&load(&manifest_path)).expect("one wired database");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "main");
        assert_eq!(targets[0].backend, NativeDatabaseBackend::Sqlite);
        assert_eq!(
            targets[0]
                .files
                .iter()
                .map(|f| f.version)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn the_migrations_directory_is_configurable_per_database() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manifest_path = project(dir.path(), "SKYZEN_TEST_DB_URL", Some("db/changes"));
        let targets = resolve_targets(&load(&manifest_path)).expect("one wired database");
        assert!(
            targets[0].directory.ends_with("db/changes"),
            "{:?}",
            targets[0].directory
        );
        assert_eq!(targets[0].files.len(), 2);
    }

    #[test]
    fn a_plain_migrate_still_goes_to_the_provider_and_native_is_opt_in() {
        // The default has to stay `wrangler d1 migrations apply`: a Worker project that types
        // `skyzen migrate` is asking about D1.
        assert_eq!(dispatch(None, false).expect("default"), Dispatch::Provider);
        assert_eq!(
            dispatch(Some(Provider::Cloudflare), false).expect("explicit cloud"),
            Dispatch::Provider
        );
        assert_eq!(
            dispatch(Some(Provider::Cloudflare), true).expect("the emulator"),
            Dispatch::Provider
        );
        assert_eq!(
            dispatch(Some(Provider::Native), false).expect("opt in"),
            Dispatch::Native
        );
    }

    #[test]
    fn local_has_no_meaning_on_the_native_path() {
        let error = dispatch(Some(Provider::Native), true).expect_err("no emulator");
        assert!(error.to_string().contains("--local"), "{error}");
    }

    #[test]
    fn a_project_with_no_native_wiring_says_what_to_do_instead() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manifest_path = dir.path().join("Skyzen.toml");
        fs::write(
            &manifest_path,
            "[[database]]\nname = \"main\"\ntype = \"sql\"\ndefault = true\n",
        )
        .expect("write manifest");

        let error = resolve_targets(&load(&manifest_path)).expect_err("nothing to connect through");
        let rendered = error.to_string();
        assert!(rendered.contains("[native.database."), "{rendered}");
        // The D1 alternative has to be named, because that is where such a project's database is.
        assert!(
            rendered.contains("skyzen migrate --provider cloudflare"),
            "{rendered}"
        );
    }

    #[test]
    fn a_database_that_is_not_a_connection_is_skipped_with_the_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manifest_path = dir.path().join("Skyzen.toml");
        fs::write(
            &manifest_path,
            "[[database]]\nname = \"main\"\ntype = \"sql\"\ndefault = true\n\n\
             [native.database.main]\nbackend = \"rds-data\"\n",
        )
        .expect("write manifest");

        // The RDS Data API is wired, so the message must not claim the database has no wiring —
        // it must say the runner cannot open a connection to it, and where the migrations can run.
        let error = resolve_targets(&load(&manifest_path)).expect_err("nothing to connect through");
        let rendered = error.to_string();
        assert!(rendered.contains("main (rds-data)"), "{rendered}");
        assert!(rendered.contains("Db::migrate"), "{rendered}");
    }

    #[test]
    fn a_database_the_runner_can_open_is_still_migrated_beside_one_it_cannot() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manifest_path = project(dir.path(), "SKYZEN_TEST_DB_URL", None);
        let source = fs::read_to_string(&manifest_path).expect("read manifest");
        fs::write(
            &manifest_path,
            format!(
                "{source}\n[[database]]\nname = \"reports\"\ntype = \"sql\"\n\n\
                 [native.database.reports]\nbackend = \"rds-data\"\n"
            ),
        )
        .expect("write manifest");

        let targets = resolve_targets(&load(&manifest_path)).expect("one openable database");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "main");
    }

    #[test]
    fn a_malformed_migration_name_fails_before_anything_connects() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manifest_path = project(dir.path(), "SKYZEN_TEST_DB_URL", None);
        fs::write(dir.path().join("migrations/0003-bad.sql"), "SELECT 1;\n").expect("write");

        let error = resolve_targets(&load(&manifest_path)).expect_err("bad file name");
        assert!(error.to_string().contains("main"), "{error}");
        let chain = format!("{error:#}");
        assert!(chain.contains("0003-bad.sql"), "{chain}");
    }

    #[test]
    fn the_environment_wins_over_the_dotenv_files() {
        let target = Target {
            name: "main".to_owned(),
            backend: NativeDatabaseBackend::Sqlite,
            url_env: "SKYZEN_TEST_URL_PRECEDENCE".to_owned(),
            directory: std::path::PathBuf::from("migrations"),
            files: Vec::new(),
        };
        let dotenv = BTreeMap::from([(target.url_env.clone(), "from-dotenv".to_owned())]);
        assert_eq!(
            connection_url(&target, &dotenv).expect("dotenv"),
            "from-dotenv"
        );

        // SAFETY: single-threaded test process, and the variable is unique to this test.
        unsafe { std::env::set_var(&target.url_env, "from-env") };
        assert_eq!(connection_url(&target, &dotenv).expect("env"), "from-env");
        unsafe { std::env::remove_var(&target.url_env) };
    }

    #[test]
    fn an_unset_connection_variable_names_the_manifest_key_that_asked_for_it() {
        let target = Target {
            name: "main".to_owned(),
            backend: NativeDatabaseBackend::Sqlite,
            url_env: "SKYZEN_TEST_NEVER_SET_URL".to_owned(),
            directory: std::path::PathBuf::from("migrations"),
            files: Vec::new(),
        };
        let error =
            connection_url(&target, &BTreeMap::new()).expect_err("the variable is set nowhere");
        let rendered = error.to_string();
        assert!(rendered.contains("SKYZEN_TEST_NEVER_SET_URL"), "{rendered}");
        assert!(
            rendered.contains("[native.database.main].url_env"),
            "{rendered}"
        );
    }

    #[test]
    fn a_dry_run_validates_the_directory_without_connecting() {
        let dir = tempfile::tempdir().expect("temp dir");
        // A variable that is deliberately never set: a dry run must not need it, because it never
        // opens a connection.
        let manifest_path = project(dir.path(), "SKYZEN_TEST_DRY_RUN_URL", None);
        run(&manifest_path, None, true).expect("a dry run needs no connection");
        assert!(!dir.path().join("app.db").exists());
    }

    #[test]
    fn applying_and_then_reporting_status_walks_a_real_sqlite_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let variable = "SKYZEN_TEST_MIGRATE_E2E_URL";
        let manifest_path = project(dir.path(), variable, None);
        // SAFETY: single-threaded test process, and the variable is unique to this test.
        unsafe { std::env::set_var(variable, sqlite_url(dir.path())) };

        run(&manifest_path, None, false).expect("migrations apply");
        assert!(dir.path().join("app.db").exists());

        // Idempotent: a second apply finds nothing pending and is not an error.
        run(&manifest_path, None, false).expect("second apply is a no-op");
        run(&manifest_path, Some(MigrateCommand::Status), false).expect("status reads");

        // Editing an applied migration is refused, naming the file.
        fs::write(
            dir.path().join("migrations/0001_create_users.sql"),
            "CREATE TABLE users (id INTEGER PRIMARY KEY);\n",
        )
        .expect("edit an applied migration");
        let error = run(&manifest_path, None, false).expect_err("edited history");
        let chain = format!("{error:#}");
        assert!(chain.contains("create_users"), "{chain}");
        assert!(chain.contains("immutable"), "{chain}");

        unsafe { std::env::remove_var(variable) };
    }
}
