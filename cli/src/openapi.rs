//! `skyzen openapi` — open the application's API reference in a browser.
//!
//! The document is produced by the application itself, not by the CLI: only the compiled binary
//! knows what `#[skyzen::openapi]` collected. Running it with `SKYZEN_OPENAPI_DUMP` set makes it
//! print the document and exit, which happens *before* it wires any service, so this command needs
//! no credentials, no reachable backend and no port — and never leaves a server running.
//!
//! The page is then a local file. Scalar renders an embedded document just as happily as a fetched
//! one, so there is nothing here to serve and nothing to shut down.

use crate::{output, providers};
use anyhow::{bail, Context, Result};
use askama::Template;
use std::path::Path;

/// Where the dumped document and the rendered page are written, alongside the other files the CLI
/// generates for a project.
const GENERATED_DIR: &str = ".skyzen/gen";
const SPEC_FILE: &str = "openapi.json";
const PAGE_FILE: &str = "openapi.html";

/// What `skyzen openapi` was asked to produce.
#[derive(Debug)]
pub struct Request<'a> {
    /// Path to the project manifest, from the global `--manifest`.
    pub manifest: &'a Path,
    /// Write the document here and stop, instead of rendering a page.
    pub json: Option<&'a Path>,
    /// Write the document to standard output and stop.
    pub print: bool,
    /// Render the page but do not open a browser.
    pub no_open: bool,
    /// Describe the work without doing any of it.
    pub dry_run: bool,
}

/// Produce the document and, unless asked otherwise, open it.
///
/// # Errors
///
/// Fails when the manifest cannot be read, when the application does not build or exits non-zero,
/// or when the document cannot be written or opened.
pub fn run(request: &Request<'_>) -> Result<()> {
    let manifest = providers::load_or_empty(request.manifest)?;
    let root_dir = manifest.root_dir().to_path_buf();
    let generated = root_dir.join(GENERATED_DIR);
    let spec_path = generated.join(SPEC_FILE);

    if request.dry_run {
        describe(request, &spec_path, &generated.join(PAGE_FILE));
        return Ok(());
    }

    std::fs::create_dir_all(&generated)
        .with_context(|| format!("failed to create {}", generated.display()))?;
    let report = if request.print {
        output::step_aside
    } else {
        output::step
    };
    let spec = dump(&root_dir, &spec_path, report)?;

    if request.print {
        // The document *is* this command's output here, so it goes to stdout unprefixed —
        // `skyzen openapi --print | jq` has to be a document and nothing else.
        println!("{spec}");
        return Ok(());
    }

    if let Some(destination) = request.json {
        std::fs::write(destination, &spec)
            .with_context(|| format!("failed to write {}", destination.display()))?;
        report(format!("write {}", destination.display()));
        return Ok(());
    }

    let page_path = generated.join(PAGE_FILE);
    let page = render_page(&manifest_title(&root_dir), &spec)?;
    std::fs::write(&page_path, page)
        .with_context(|| format!("failed to write {}", page_path.display()))?;
    report(format!("write {}", page_path.display()));

    let url = format!("file://{}", page_path.display());
    if request.no_open {
        report(url);
        return Ok(());
    }

    report(format!("open {url}"));
    open::that(&page_path).with_context(|| format!("failed to open {}", page_path.display()))
}

/// Say what would happen, writing nothing and building nothing.
fn describe(request: &Request<'_>, spec_path: &Path, page_path: &Path) {
    output::dry_run(format!(
        "cargo run ({}={})",
        skyzen_core::OPENAPI_DUMP_ENV,
        spec_path.display()
    ));
    if request.print {
        output::dry_run("write the document to stdout");
    } else if let Some(destination) = request.json {
        output::dry_run(format!("write {}", destination.display()));
    } else {
        output::dry_run(format!("write {}", page_path.display()));
        if !request.no_open {
            output::dry_run(format!("open file://{}", page_path.display()));
        }
    }
}

/// Run the application so it prints its own document, and read the result back.
///
/// No child environment is prepared, unlike `skyzen dev`: the document is built from the router
/// before any service is wired, so a connection string this command would have had to demand is a
/// connection string it would never use.
fn dump(root_dir: &Path, spec_path: &Path, report: fn(String)) -> Result<String> {
    report(format!(
        "cargo run ({}={})",
        skyzen_core::OPENAPI_DUMP_ENV,
        spec_path.display()
    ));

    let command = providers::CommandPlan {
        program: "cargo".to_owned(),
        args: vec!["run".to_owned()],
        cwd: Some(root_dir.to_path_buf()),
        stdin: providers::CommandStdin::Inherit,
    };
    let env = vec![(
        skyzen_core::OPENAPI_DUMP_ENV.to_owned(),
        spec_path.display().to_string(),
    )];

    let status = providers::spawn_command(&command, &env)?
        .wait()
        .context("failed to wait for cargo run")?;
    if !status.success() {
        bail!("the application exited with {status} instead of printing its OpenAPI document");
    }

    let spec = std::fs::read_to_string(spec_path).with_context(|| {
        format!(
            "the application exited successfully but wrote no document to {}; is it built with \
             skyzen's `openapi` feature?",
            spec_path.display()
        )
    })?;
    report(format!(
        "write {} ({} operations)",
        spec_path.display(),
        operation_count(&spec)
    ));
    Ok(spec)
}

/// How many operations the document describes, for the progress line.
///
/// A path item holds one entry per method, so this counts methods rather than paths — the number a
/// reader will actually see listed.
fn operation_count(spec: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(spec)
        .ok()
        .and_then(|document| document.get("paths")?.as_object().cloned())
        .map_or(0, |paths| {
            paths
                .values()
                .filter_map(|item| item.as_object().map(serde_json::Map::len))
                .sum()
        })
}

/// The browser tab's title, which is the project directory's name.
///
/// The document's own `info.title` is what the page displays; this only has to distinguish one
/// open tab from another.
fn manifest_title(root_dir: &Path) -> String {
    root_dir.file_name().map_or_else(
        || "API reference".to_owned(),
        |name| format!("{} — API reference", name.to_string_lossy()),
    )
}

/// Render the Scalar page around an embedded document.
fn render_page(title: &str, spec: &str) -> Result<String> {
    ScalarPageTemplate { title, spec }
        .render()
        .context("failed to render the API reference page")
}

#[derive(Template)]
#[template(path = "scalar.html.tmpl", escape = "none")]
struct ScalarPageTemplate<'a> {
    title: &'a str,
    /// The document, embedded verbatim as the JSON literal Scalar is handed.
    spec: &'a str,
}

#[cfg(test)]
mod tests {
    use super::{operation_count, render_page};

    const SPEC: &str = r#"{"openapi":"3.1.0","info":{"title":"apidemo","version":"0.1.0"},
        "paths":{"/a":{"get":{},"post":{}},"/b":{"get":{}}}}"#;

    #[test]
    fn the_page_embeds_the_document_rather_than_linking_to_it() {
        let page = render_page("apidemo — API reference", SPEC).unwrap();
        assert!(page.contains("@scalar/api-reference"), "{page}");
        assert!(page.contains("apidemo — API reference"), "{page}");
        // Embedded, not linked: the page is opened over `file://`, where fetching a sibling file
        // is blocked by the browser's origin rules.
        assert!(page.contains("content:"), "{page}");
        assert!(page.contains(r#""title":"apidemo""#), "{page}");
        assert!(!page.contains(super::SPEC_FILE), "{page}");
    }

    #[test]
    fn the_progress_line_counts_methods_not_paths() {
        assert_eq!(operation_count(SPEC), 3);
    }

    #[test]
    fn an_unreadable_document_counts_nothing_rather_than_panicking() {
        assert_eq!(operation_count("not json"), 0);
    }
}
