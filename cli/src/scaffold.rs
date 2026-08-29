//! `skyzen new`: generating a project.
//!
//! Templates carry no dependency versions. They used to hardcode `skyzen = "0.1"`, so a CLI
//! released alongside skyzen 0.3 still scaffolded a project pinned to 0.1; the versions are now
//! resolved by `cargo add`, which is the only thing that actually knows what the registry has.
//! The one exception is `wasm-bindgen`, which is pinned to the version this binary's bindings
//! generator was built from — that is a correctness constraint, not a freshness one.

use crate::{
    cli::Template, environment, output, providers::cloudflare::build::embedded_wasm_bindgen_version,
};
use anyhow::{Context, Result};
use askama::Template as _;
use skyzen_manifest::Manifest;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use time::{macros::format_description, OffsetDateTime};

/// The values every scaffold template substitutes.
#[derive(Debug)]
pub struct ScaffoldContext {
    /// The cargo package name, taken from the target directory.
    pub package_name: String,
    /// The Worker name, which cannot contain underscores.
    pub worker_name: String,
    /// Today, as the Workers compatibility date.
    pub compatibility_date: String,
    /// The wasm-bindgen version this binary's generator was built from.
    pub wasm_bindgen_version: String,
}

/// One crate `skyzen new` asks cargo to add to the generated project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DependencySpec {
    /// The crate name.
    pub name: &'static str,
    /// Features to enable.
    pub features: &'static [&'static str],
}

impl DependencySpec {
    /// The `cargo add` arguments for this dependency.
    pub fn cargo_add_args(&self) -> Vec<String> {
        let mut args = vec!["add".to_owned(), self.name.to_owned()];
        if !self.features.is_empty() {
            args.push("--features".to_owned());
            args.push(self.features.join(","));
        }
        args
    }
}

/// Declare one template's files, generating the askama struct for each.
macro_rules! template_set {
    ($fn_name:ident, [$(($struct:ident, $path:literal, $output:literal)),* $(,)?]) => {
        $(
            // Every template takes the context so the file list stays uniform, even the few
            // (`.gitignore`, `lib.rs`, `main.rs`) that substitute nothing today.
            #[derive(askama::Template)]
            #[template(path = $path, escape = "none")]
            #[allow(dead_code)]
            struct $struct<'a> {
                ctx: &'a ScaffoldContext,
            }
        )*

        fn $fn_name(root: &Path, ctx: &ScaffoldContext) -> Result<Vec<(PathBuf, String)>> {
            Ok(vec![
                $((
                    root.join($output),
                    $struct { ctx }
                        .render()
                        .with_context(|| format!("failed to render {}", $path))?,
                ),)*
            ])
        }
    };
}

template_set!(
    minimal_files,
    [
        (MinimalCargoToml, "minimal/Cargo.toml.tmpl", "Cargo.toml"),
        (MinimalSkyzenToml, "minimal/Skyzen.toml.tmpl", "Skyzen.toml"),
        (MinimalGitignore, "minimal/gitignore.tmpl", ".gitignore"),
        (MinimalApp, "minimal/src/app.rs.tmpl", "src/app.rs"),
        (MinimalLib, "minimal/src/lib.rs.tmpl", "src/lib.rs"),
        (MinimalMain, "minimal/src/main.rs.tmpl", "src/main.rs"),
    ]
);

template_set!(
    api_files,
    [
        (ApiCargoToml, "api/Cargo.toml.tmpl", "Cargo.toml"),
        (ApiSkyzenToml, "api/Skyzen.toml.tmpl", "Skyzen.toml"),
        (ApiGitignore, "api/gitignore.tmpl", ".gitignore"),
        (ApiApp, "api/src/app.rs.tmpl", "src/app.rs"),
        (ApiLib, "api/src/lib.rs.tmpl", "src/lib.rs"),
        (ApiMain, "api/src/main.rs.tmpl", "src/main.rs"),
        (
            ApiMigration,
            "api/migrations/0001_create_greetings.sql.tmpl",
            "migrations/0001_create_greetings.sql"
        ),
    ]
);

template_set!(
    serverless_events_files,
    [
        (
            EventsCargoToml,
            "serverless-events/Cargo.toml.tmpl",
            "Cargo.toml"
        ),
        (
            EventsSkyzenToml,
            "serverless-events/Skyzen.toml.tmpl",
            "Skyzen.toml"
        ),
        (
            EventsGitignore,
            "serverless-events/gitignore.tmpl",
            ".gitignore"
        ),
        (EventsApp, "serverless-events/src/app.rs.tmpl", "src/app.rs"),
        (EventsLib, "serverless-events/src/lib.rs.tmpl", "src/lib.rs"),
        (
            EventsMain,
            "serverless-events/src/main.rs.tmpl",
            "src/main.rs"
        ),
    ]
);

template_set!(
    durable_realtime_files,
    [
        (
            DurableCargoToml,
            "durable-realtime/Cargo.toml.tmpl",
            "Cargo.toml"
        ),
        (
            DurableSkyzenToml,
            "durable-realtime/Skyzen.toml.tmpl",
            "Skyzen.toml"
        ),
        (
            DurableGitignore,
            "durable-realtime/gitignore.tmpl",
            ".gitignore"
        ),
        (DurableApp, "durable-realtime/src/app.rs.tmpl", "src/app.rs"),
        (
            DurableObject,
            "durable-realtime/src/durable_object.rs.tmpl",
            "src/durable_object.rs"
        ),
        (DurableLib, "durable-realtime/src/lib.rs.tmpl", "src/lib.rs"),
        (
            DurableMain,
            "durable-realtime/src/main.rs.tmpl",
            "src/main.rs"
        ),
    ]
);

/// The crates each template's code needs.
pub const fn dependencies(template: Template) -> &'static [DependencySpec] {
    const SKYZEN: DependencySpec = DependencySpec {
        name: "skyzen",
        features: &[],
    };
    const SKYZEN_WS: DependencySpec = DependencySpec {
        name: "skyzen",
        features: &["ws"],
    };
    const SERVICES: DependencySpec = DependencySpec {
        name: "skyzen-services",
        features: &[],
    };
    const TEST: DependencySpec = DependencySpec {
        name: "skyzen-test",
        features: &[],
    };
    const CLOUDFLARE: DependencySpec = DependencySpec {
        name: "skyzen-cloudflare",
        features: &[],
    };
    const SERDE: DependencySpec = DependencySpec {
        name: "serde",
        features: &["derive"],
    };
    const TRACING: DependencySpec = DependencySpec {
        name: "tracing",
        features: &[],
    };
    const FUTURES: DependencySpec = DependencySpec {
        name: "futures-util",
        features: &[],
    };
    /// A payload carried by `Json`, `Form` or `Query` has to implement `ToSchema`, and utoipa's
    /// derive expands to `::utoipa::…` paths — so the crate that writes `#[derive(ToSchema)]`
    /// needs the dependency itself. `skyzen::ToSchema` re-exports the trait, which is what the
    /// bound is written against; it cannot re-export the crate the expansion names.
    const UTOIPA: DependencySpec = DependencySpec {
        name: "utoipa",
        features: &[],
    };

    match template {
        Template::Minimal => &[SKYZEN],
        // The api template declares a portable KV service, so it needs the wrappers, the
        // in-process backend `[native.service.cache]` names, and the Cloudflare one.
        Template::Api => &[SKYZEN, SERVICES, TEST, CLOUDFLARE, SERDE, UTOIPA],
        Template::ServerlessEvents => &[SKYZEN, SERVICES, CLOUDFLARE, SERDE, TRACING],
        Template::DurableRealtime => &[SKYZEN_WS, CLOUDFLARE, SERDE, FUTURES],
    }
}

fn template_files(
    template: Template,
    root: &Path,
    ctx: &ScaffoldContext,
) -> Result<Vec<(PathBuf, String)>> {
    match template {
        Template::Minimal => minimal_files(root, ctx),
        Template::Api => api_files(root, ctx),
        Template::ServerlessEvents => serverless_events_files(root, ctx),
        Template::DurableRealtime => durable_realtime_files(root, ctx),
    }
}

/// What to do about files already in the target directory.
///
/// `--force` used to mean "replace whatever is there", which is not what "reuse an existing
/// target directory" suggests and not what `cargo new` does. It now keeps what it finds, and
/// replacing is a separate, louder flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExistingFiles {
    /// Refuse to scaffold into a non-empty directory.
    #[default]
    Refuse,
    /// Reuse the directory, keeping every file that is already there (`--force`).
    Keep,
    /// Reuse the directory, replacing files that are already there (`--overwrite`).
    Replace,
}

/// What `skyzen new` was asked to do.
#[derive(Debug)]
pub struct ScaffoldRequest<'a> {
    /// The directory to scaffold into.
    pub path: &'a Path,
    /// Which starting point to generate.
    pub template: Template,
    /// How to treat files that are already there.
    pub existing: ExistingFiles,
    /// Print what would be written instead of writing it.
    pub dry_run: bool,
    /// Run `cargo add` in the generated project.
    ///
    /// The scaffold compile tests turn this off: they must not touch the network, and they wire
    /// the workspace's own crates in by path instead.
    pub install_dependencies: bool,
}

/// Generate a project.
///
/// # Errors
///
/// Fails when the target directory is unusable, when the package name is not a valid cargo name,
/// when a template cannot render, or when a file cannot be written.
pub fn create_project(request: &ScaffoldRequest<'_>) -> Result<()> {
    let root = resolve_root(request.path)?;
    let package_name = package_name(&root)?;
    validate_package_name(&package_name)?;
    ensure_target_dir(&root, request)?;

    let ctx = ScaffoldContext {
        worker_name: package_name.replace('_', "-"),
        package_name,
        compatibility_date: OffsetDateTime::now_utc()
            .format(&format_description!("[year]-[month]-[day]"))
            .context("failed to format the Workers compatibility date")?,
        wasm_bindgen_version: embedded_wasm_bindgen_version(),
    };

    let mut files = template_files(request.template, &root, &ctx)?;
    files.push(env_example(&root, &files)?);

    if request.dry_run {
        for (path, contents) in &files {
            output::dry_run(format!("write {}", path.display()));
            println!("{contents}");
        }
        for dependency in dependencies(request.template) {
            output::dry_run(format!("cargo {}", dependency.cargo_add_args().join(" ")));
        }
        return Ok(());
    }

    for (path, contents) in &files {
        write_file(path, contents, request.existing)?;
    }

    if request.install_dependencies {
        install_dependencies(&root, request.template)?;
    }

    Ok(())
}

/// Render the `.env.example` from the manifest the template just produced.
///
/// Deriving it rather than shipping a static file keeps it honest: a template that adds a
/// `url_env` gets the variable listed without anyone remembering to update a second file.
fn env_example(root: &Path, files: &[(PathBuf, String)]) -> Result<(PathBuf, String)> {
    let manifest_path = root.join("Skyzen.toml");
    let source = files
        .iter()
        .find(|(path, _)| *path == manifest_path)
        .map(|(_, contents)| contents.as_str())
        .context("the template produced no Skyzen.toml")?;
    let manifest = Manifest::parse(source, &manifest_path, root)?;
    Ok((
        root.join(".env.example"),
        environment::render_example(manifest.data())?,
    ))
}

fn write_file(path: &Path, contents: &str, existing: ExistingFiles) -> Result<()> {
    if path.exists() && existing != ExistingFiles::Replace {
        output::step(format!("kept existing {}", path.display()));
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    output::step(format!("wrote {}", path.display()));
    Ok(())
}

/// Resolve the target directory, so `skyzen new .` works.
fn resolve_root(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()));
    }
    // A path that does not exist yet cannot be canonicalized, but its parent can, which is what
    // turns `./demo` and `../demo` into something with a usable final component.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let name = path
        .file_name()
        .context("the project path must end with a directory name")?;
    match parent {
        Some(parent) if parent.exists() => Ok(parent
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", parent.display()))?
            .join(name)),
        _ => Ok(path.to_path_buf()),
    }
}

fn package_name(root: &Path) -> Result<String> {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .with_context(|| {
            format!(
                "cannot derive a package name from {}; the path must end with a directory name",
                root.display()
            )
        })
}

/// Reject a name cargo would reject, before any file is written.
///
/// The old scaffolder took the directory name with only an emptiness check, so `skyzen new "my
/// app"` wrote `name = "my app"` into `Cargo.toml` and failed later, inside cargo, with a
/// generated project already on disk.
fn validate_package_name(name: &str) -> Result<()> {
    /// Keywords cargo refuses outright, because a crate name has to be a usable identifier.
    const RESERVED: &[&str] = &[
        "crate", "self", "super", "extern", "as", "async", "await", "break", "const", "continue",
        "dyn", "else", "enum", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
        "mod", "move", "mut", "pub", "ref", "return", "static", "struct", "trait", "true", "type",
        "unsafe", "use", "where", "while", "test",
    ];

    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        anyhow::bail!("the package name cannot be empty");
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        anyhow::bail!(
            "`{name}` cannot be used as a package name: it must start with a letter or underscore"
        );
    }
    if let Some(invalid) = name.chars().find(|character| {
        !character.is_ascii_alphanumeric() && *character != '-' && *character != '_'
    }) {
        anyhow::bail!(
            "`{name}` cannot be used as a package name: `{invalid}` is not allowed; use letters, \
             digits, `-` and `_`"
        );
    }
    if RESERVED.contains(&name) {
        anyhow::bail!("`{name}` cannot be used as a package name: it is a Rust keyword");
    }
    Ok(())
}

fn ensure_target_dir(root: &Path, request: &ScaffoldRequest<'_>) -> Result<()> {
    if !root.exists() {
        if request.dry_run {
            return Ok(());
        }
        return fs::create_dir_all(root)
            .with_context(|| format!("failed to create {}", root.display()));
    }

    if request.existing == ExistingFiles::Refuse {
        let mut entries =
            fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?;
        if entries.next().is_some() {
            anyhow::bail!(
                "target directory `{}` already exists and is not empty; pass --force to keep the \
                 files that are already there, or --overwrite to replace them",
                root.display()
            );
        }
        return Ok(());
    }

    // Overwriting inside a dirty worktree destroys work that is not recoverable from git.
    if request.existing == ExistingFiles::Replace && is_dirty_git_worktree(root) {
        anyhow::bail!(
            "`{}` is a git worktree with uncommitted changes; commit or stash them before \
             scaffolding with --overwrite",
            root.display()
        );
    }

    Ok(())
}

fn is_dirty_git_worktree(root: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

/// Let cargo resolve the dependency versions, in the project that was just generated.
fn install_dependencies(root: &Path, template: Template) -> Result<()> {
    let specs = dependencies(template);
    for spec in specs {
        let args = spec.cargo_add_args();
        output::step(format!("cargo {}", args.join(" ")));
        let status = Command::new("cargo")
            .args(&args)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();

        let failed = match status {
            Ok(status) => !status.success(),
            Err(error) => {
                output::warn(format!("failed to launch cargo: {error}"));
                true
            }
        };

        if failed {
            let commands = specs
                .iter()
                .map(|spec| format!("  cargo {}", spec.cargo_add_args().join(" ")))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "the project was generated, but its dependencies could not be added (offline?). \
                 Run these in {}:\n{commands}",
                root.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
