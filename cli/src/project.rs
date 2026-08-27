//! What `cargo metadata` knows about the application being built.
//!
//! One place asks cargo, so the dependency check, the wasm artifact path and the wasm-bindgen
//! agreement check all agree about which package they are talking about.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// The wasm target Cloudflare Workers are built for.
pub const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// The application package `skyzen` is operating on.
#[derive(Debug, Clone)]
pub struct Project {
    root_dir: PathBuf,
    manifest_path: PathBuf,
    package: Package,
    target_directory: PathBuf,
}

impl Project {
    /// Load the package rooted at `root_dir`.
    ///
    /// This resolves nothing beyond the package's own manifest, so it works offline and without a
    /// lockfile. [`Self::resolved_dependency_version`] does the resolving call when a resolved
    /// version is actually needed.
    ///
    /// # Errors
    ///
    /// Fails when there is no `Cargo.toml`, when `cargo metadata` fails, or when the metadata does
    /// not describe the package at `root_dir`.
    pub fn load(root_dir: &Path) -> Result<Self> {
        let manifest_path = root_dir.join("Cargo.toml");
        if !manifest_path.exists() {
            anyhow::bail!(
                "missing Cargo.toml at project root {}; skyzen operates on a Rust package",
                root_dir.display()
            );
        }

        let metadata: Metadata = run_metadata(root_dir, &manifest_path, &["--no-deps"])?;
        let package = metadata
            .packages
            .into_iter()
            .find(|package| package.manifest_path == manifest_path)
            .with_context(|| {
                format!(
                    "cargo metadata described no package at {}",
                    manifest_path.display()
                )
            })?;

        Ok(Self {
            root_dir: root_dir.to_path_buf(),
            manifest_path,
            package,
            target_directory: metadata.target_directory,
        })
    }

    /// The package's `Cargo.toml`.
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    /// Cargo's build directory for this project.
    pub fn target_directory(&self) -> &Path {
        &self.target_directory
    }

    /// Whether the package declares a direct dependency on `crate_name`, on any target.
    pub fn depends_on(&self, crate_name: &str) -> bool {
        self.package
            .dependencies
            .iter()
            .any(|dependency| dependency.name == crate_name)
    }

    /// The name of the package's `cdylib` target — the wasm module a Worker build produces.
    ///
    /// # Errors
    ///
    /// Fails when the package has no `cdylib` target, which is the manifest mistake behind the
    /// otherwise baffling "no wasm artifact was produced" build failure.
    pub fn cdylib_target_name(&self) -> Result<&str> {
        self.package
            .targets
            .iter()
            .find(|target| target.crate_types.iter().any(|kind| kind == "cdylib"))
            .map(|target| target.name.as_str())
            .with_context(|| {
                format!(
                    "{} must define a [lib] target with crate-type including \"cdylib\" for Cloudflare builds",
                    self.manifest_path.display()
                )
            })
    }

    /// The version `crate_name` resolves to in the dependency graph for `target`.
    ///
    /// This is the resolving `cargo metadata` call, so it needs a lockfile or network access the
    /// same way a build does. Filtering by platform keeps the answer to the graph the Worker build
    /// will actually compile.
    ///
    /// # Errors
    ///
    /// Fails when `cargo metadata` fails, or when the graph contains more than one version of
    /// `crate_name` — an ambiguity no version check could resolve.
    pub fn resolved_dependency_version(
        &self,
        crate_name: &str,
        target: &str,
    ) -> Result<Option<String>> {
        let metadata: Metadata = run_metadata(
            &self.root_dir,
            &self.manifest_path,
            &["--filter-platform", target],
        )?;

        let mut versions: Vec<String> = metadata
            .packages
            .into_iter()
            .filter(|package| package.name == crate_name)
            .map(|package| package.version)
            .collect();
        versions.sort();
        versions.dedup();

        match versions.len() {
            0 => Ok(None),
            1 => Ok(versions.pop()),
            _ => anyhow::bail!(
                "the dependency graph for {target} contains {} versions of `{crate_name}` ({}); \
                 unify them before building",
                versions.len(),
                versions.join(", ")
            ),
        }
    }
}

fn run_metadata(root_dir: &Path, manifest_path: &Path, extra: &[&str]) -> Result<Metadata> {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .args(extra)
        .arg("--manifest-path")
        .arg(manifest_path)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .current_dir(root_dir)
        .output()
        .context("failed to launch cargo metadata")?;

    if !output.status.success() {
        anyhow::bail!(
            "cargo metadata failed for {} (arguments: {})",
            manifest_path.display(),
            extra.join(" ")
        );
    }

    serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata JSON")
}

#[derive(Debug, Clone, Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    #[serde(default)]
    target_directory: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct Package {
    name: String,
    version: String,
    manifest_path: PathBuf,
    #[serde(default)]
    dependencies: Vec<Dependency>,
    #[serde(default)]
    targets: Vec<Target>,
}

#[derive(Debug, Clone, Deserialize)]
struct Dependency {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Target {
    name: String,
    crate_types: Vec<String>,
}
