//! Delivering `[[secret]]` values to a Worker.
//!
//! Two shapes, chosen per secret by the manifest: a *classic* Worker secret, which `wrangler
//! secret bulk` uploads with the deployment and `.dev.vars` supplies locally, and a Secrets Store
//! entry declared by `[cloudflare.secret.<NAME>]`, which the Worker binds rather than carries.
//!
//! A value never reaches a command line. `wrangler secret put`, `wrangler secret bulk` and the
//! `secrets-store` commands all read from standard input when standard input is not a terminal, so
//! everything here plans a [`CommandPlan`] whose [`CommandStdin::Secret`] carries the value and
//! whose printed form carries none of it.

use crate::{
    environment::{self, ResolvedVariables, VariableKind},
    providers::{
        resolve_variables, run_command, CommandPlan, CommandStdin, FileContents, GeneratedFile,
        Step, Task,
    },
};
use anyhow::{Context, Result};
use askama::Template;
use regex::Regex;
use secrecy::SecretString;
use skyzen_manifest::{CloudflareSecretSection, CloudflareSection, Manifest, VarName};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// How many entries one `secrets-store secret list` page asks for.
const PAGE_SIZE: usize = 100;

/// The scopes a Secrets Store secret needs to be readable from a Worker.
const WORKER_SCOPES: &str = "workers";

/// Everything the secret sinks for one selected environment need.
#[derive(Debug)]
pub struct SecretDelivery<'a> {
    /// The project, for its declarations and its `.env` files.
    pub manifest: &'a Manifest,
    /// The selected environment's Cloudflare section, which says which secrets are store-backed.
    pub config: &'a CloudflareSection,
    /// `--config <path> [--env <name>]`, as every other wrangler invocation takes it.
    pub config_args: &'a [String],
    /// The generated `wrangler.toml`, which is also what makes the local store the one
    /// `wrangler dev` reads.
    pub config_path: &'a str,
    /// Where to run wrangler.
    pub cwd: &'a Path,
}

impl SecretDelivery<'_> {
    /// Whether `[cloudflare.secret.<name>]` backs this secret with a Secrets Store entry.
    fn store_backed(&self, name: &VarName) -> Option<&CloudflareSecretSection> {
        self.config.secret.get(name)
    }

    /// Every declared secret that is a classic Worker secret, resolved.
    ///
    /// Store-backed secrets are left out rather than resolved and skipped: they are externally
    /// managed, so demanding a local value would refuse a deployment that is complete.
    ///
    /// # Errors
    ///
    /// Fails when one of them is set nowhere, naming every one that is.
    pub fn classic(&self) -> Result<ResolvedVariables> {
        resolve_variables(self.manifest, &[VariableKind::Secret], |variable| {
            self.store_backed(&variable.name).is_none()
        })
    }

    /// Every declared secret, resolved.
    ///
    /// # Errors
    ///
    /// Fails when one of them is set nowhere, naming every one that is.
    pub fn all(&self) -> Result<ResolvedVariables> {
        resolve_variables(self.manifest, &[VariableKind::Secret], |_| true)
    }

    /// One wrangler invocation carrying the generated configuration.
    fn wrangler(&self, head: &[&str], args: &[String]) -> CommandPlan {
        let mut all: Vec<String> = head.iter().map(|part| (*part).to_owned()).collect();
        all.extend(args.iter().cloned());
        CommandPlan {
            program: "wrangler".to_owned(),
            args: all,
            cwd: Some(self.cwd.to_path_buf()),
            stdin: CommandStdin::Inherit,
        }
    }

    /// The arguments a `secrets-store` command takes.
    ///
    /// `--env` is not one of them: a store entry belongs to an account, not to a Worker
    /// environment, and the configuration is only named so that the local store is the one
    /// `wrangler dev` reads.
    fn store_args(&self) -> Vec<String> {
        vec!["--config".to_owned(), self.config_path.to_owned()]
    }

    /// `wrangler secret bulk`, reading `{"NAME":"value"}` on its standard input.
    ///
    /// `None` when there is nothing to deliver, because an empty document is an error to wrangler
    /// and a project with no classic secrets has nothing to say about them.
    ///
    /// # Errors
    ///
    /// Fails only when the values cannot be encoded as JSON.
    pub fn bulk_step(&self, resolved: &ResolvedVariables) -> Result<Option<CommandPlan>> {
        if resolved.is_empty() {
            return Ok(None);
        }

        // The one place these values are exposed: from here they go to wrangler's standard input
        // and nowhere else.
        let payload: BTreeMap<&str, &str> = resolved
            .iter()
            .map(|(name, value)| (name.as_str(), environment::expose(value)))
            .collect();
        let json = serde_json::to_string(&payload)
            .context("failed to encode the secrets for `wrangler secret bulk`")?;

        Ok(Some(
            self.wrangler(&["secret", "bulk"], self.config_args)
                .with_stdin(SecretString::from(json)),
        ))
    }

    /// The work `skyzen secret set NAME` performs.
    ///
    /// A Secrets Store entry is externally managed, so `set` is the only command that writes one —
    /// and it writes it in the account's store rather than in the Worker's own secrets.
    pub fn set_step(&self, name: &VarName, value: &SecretString) -> Step {
        self.store_backed(name).map_or_else(
            || {
                Step::Command(
                    self.wrangler(&["secret", "put", name.as_str()], self.config_args)
                        .with_stdin(environment::duplicate(value)),
                )
            },
            |section| Step::Task(Box::new(self.seed(name, section, value, Store::Remote))),
        )
    }

    /// The steps and the file `wrangler dev` needs before it can resolve a secret.
    ///
    /// Classic secrets go into a `.dev.vars` beside the generated configuration, which is where
    /// wrangler looks for local values; store-backed ones are seeded into wrangler's *local*
    /// store, which starts out empty however full the account's is.
    ///
    /// # Errors
    ///
    /// Fails when a value cannot be written as a dotenv entry, or when the file cannot be
    /// rendered.
    pub fn local_work(
        &self,
        resolved: &ResolvedVariables,
        dev_vars_path: PathBuf,
    ) -> Result<(Vec<Step>, Option<GeneratedFile>)> {
        let mut steps = Vec::new();
        let mut classic = Vec::new();
        for (name, value) in resolved {
            match self.store_backed(name) {
                Some(section) => {
                    steps.push(Step::Task(Box::new(self.seed(
                        name,
                        section,
                        value,
                        Store::Local,
                    ))));
                }
                None => classic.push(DotenvEntry {
                    name: name.to_string(),
                    value: quote_dotenv_value(environment::expose(value)).with_context(|| {
                        format!("failed to write the value of [[secret]] {name} to .dev.vars")
                    })?,
                }),
            }
        }

        if classic.is_empty() {
            return Ok((steps, None));
        }
        let rendered = DevVarsTemplate { entries: classic }
            .render()
            .context("failed to render .dev.vars")?;
        Ok((
            steps,
            Some(GeneratedFile {
                path: dev_vars_path,
                contents: FileContents::Secret(SecretString::from(rendered)),
            }),
        ))
    }

    /// The task that writes one Secrets Store entry.
    fn seed(
        &self,
        binding: &VarName,
        section: &CloudflareSecretSection,
        value: &SecretString,
        store: Store,
    ) -> SeedSecretsStoreSecret {
        SeedSecretsStoreSecret {
            store_id: section.store_id.clone(),
            secret_name: section.secret_name.clone(),
            binding: binding.clone(),
            args: self.store_args(),
            store,
            cwd: self.cwd.to_path_buf(),
            value: environment::duplicate(value),
        }
    }
}

/// Which Secrets Store a task writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Store {
    /// The one `wrangler dev` serves from, which lives beside the configuration.
    Local,
    /// The account's, which a deployed Worker binds.
    Remote,
}

impl Store {
    /// The flag that selects it. wrangler's `secrets-store` commands act locally by default.
    fn flags(self) -> Vec<String> {
        match self {
            Self::Local => Vec::new(),
            Self::Remote => vec!["--remote".to_owned()],
        }
    }

    /// How the store is named in a description.
    const fn label(self) -> &'static str {
        match self {
            Self::Local => "wrangler's local",
            Self::Remote => "the account's",
        }
    }
}

/// Put one Secrets Store entry's value in place.
///
/// A task rather than a command because wrangler has no "write by name": `secrets-store secret
/// update` addresses an entry by an id that only `secrets-store secret list` knows, so what runs
/// second depends on what the first answered.
#[derive(Debug)]
struct SeedSecretsStoreSecret {
    /// The store to write in.
    store_id: String,
    /// The entry's name inside that store.
    secret_name: String,
    /// The `[[secret]]` this backs, for the description.
    binding: VarName,
    /// `--config <path>`, shared by all three commands.
    args: Vec<String>,
    /// Which store to write.
    store: Store,
    /// Where to run wrangler.
    cwd: PathBuf,
    /// The value.
    value: SecretString,
}

impl Task for SeedSecretsStoreSecret {
    fn describe(&self) -> String {
        format!(
            "seed {} Secrets Store entry `{}` in store {} for [[secret]] {}",
            self.store.label(),
            self.secret_name,
            self.store_id,
            self.binding
        )
    }

    fn run(&self) -> Result<()> {
        // An entry is written by id, so an existing one is updated and a first run creates it. An
        // update carries the value alone: the scopes and the comment of an entry Skyzen did not
        // create are somebody else's, and `--scopes` would replace them.
        let command = self.existing_id()?.map_or_else(
            || {
                self.command(&[
                    "create",
                    &self.store_id,
                    "--name",
                    &self.secret_name,
                    "--scopes",
                    WORKER_SCOPES,
                ])
            },
            |id| self.command(&["update", &self.store_id, "--secret-id", &id]),
        );
        run_command(
            &command.with_stdin(environment::duplicate(&self.value)),
            &[],
        )
    }
}

impl SeedSecretsStoreSecret {
    /// One `wrangler secrets-store secret <verb> …` invocation.
    fn command(&self, head: &[&str]) -> CommandPlan {
        let mut args = vec!["secrets-store".to_owned(), "secret".to_owned()];
        args.extend(head.iter().map(|part| (*part).to_owned()));
        args.extend(self.store.flags());
        args.extend(self.args.iter().cloned());
        CommandPlan {
            program: "wrangler".to_owned(),
            args,
            cwd: Some(self.cwd.clone()),
            stdin: CommandStdin::Inherit,
        }
    }

    /// The id of the entry named `secret_name`, when the store already holds one.
    ///
    /// # Errors
    ///
    /// Fails when wrangler cannot be run, when it fails for a reason other than the store being
    /// empty, or when its table cannot be read.
    fn existing_id(&self) -> Result<Option<String>> {
        let mut page = 1_usize;
        loop {
            let listing = self.command(&[
                "list",
                &self.store_id,
                "--per-page",
                &PAGE_SIZE.to_string(),
                "--page",
                &page.to_string(),
            ]);
            let output = Command::new(&listing.program)
                .args(&listing.args)
                .current_dir(&self.cwd)
                .stdin(Stdio::null())
                .output()
                .context("failed to launch wrangler to list a Secrets Store")?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !output.status.success() {
                // wrangler reports an empty page as a failure rather than an empty table, which
                // for a store nothing has been written to yet is the normal first run.
                if is_empty_listing(&stdout) || is_empty_listing(&stderr) {
                    return Ok(None);
                }
                anyhow::bail!(
                    "`{}` failed:\n{}",
                    listing.display(),
                    if stderr.trim().is_empty() {
                        stdout.trim()
                    } else {
                        stderr.trim()
                    }
                );
            }

            let rows = parse_secret_rows(&stdout)?;
            if let Some(row) = rows.iter().find(|row| row.name == self.secret_name) {
                return Ok(Some(row.id.clone()));
            }
            if rows.len() < PAGE_SIZE {
                return Ok(None);
            }
            page += 1;
        }
    }
}

/// Whether wrangler's answer means "this store holds nothing", which is not an error.
fn is_empty_listing(text: &str) -> bool {
    text.contains("returned no secrets")
}

/// One row of the table `wrangler secrets-store secret list` prints.
#[derive(Debug, PartialEq, Eq)]
struct SecretRow {
    /// The entry's name.
    name: String,
    /// The id `secrets-store secret update` addresses it by.
    id: String,
}

/// Read the rows out of wrangler's listing.
///
/// The command has no machine-readable mode: it renders a `cli-table3` box-drawing table whose
/// columns are Name, ID, Comment, Scopes, Status, Created and Modified. The header row is what the
/// two columns of interest are located by, so a column added between them changes nothing here.
///
/// # Errors
///
/// Fails when a table was printed but its header names neither `Name` nor `ID`, which means
/// wrangler's output has changed shape and a "no such entry" answer would be a guess.
fn parse_secret_rows(text: &str) -> Result<Vec<SecretRow>> {
    // Colour is off when wrangler's output is a pipe, but `FORCE_COLOR` overrides that, and a
    // coloured cell would compare unequal to the name being looked for.
    let ansi = Regex::new(r"\x1b\[[0-9;]*m").context("failed to build the ANSI escape pattern")?;
    let mut rows = text.lines().filter_map(|line| {
        let trimmed = line.trim();
        if !trimmed.starts_with('│') {
            return None;
        }
        Some(
            trimmed
                .trim_matches('│')
                .split('│')
                .map(|cell| ansi.replace_all(cell, "").trim().to_owned())
                .collect::<Vec<_>>(),
        )
    });

    let Some(header) = rows.next() else {
        return Ok(Vec::new());
    };
    let column = |title: &str| header.iter().position(|cell| cell == title);
    let (Some(name), Some(id)) = (column("Name"), column("ID")) else {
        anyhow::bail!(
            "`wrangler secrets-store secret list` printed a table with no `Name` and `ID` \
             columns, so Skyzen cannot tell whether the secret already exists:\n{}",
            text.trim()
        );
    };

    Ok(rows
        .filter_map(|cells| {
            Some(SecretRow {
                name: cells.get(name)?.clone(),
                id: cells.get(id)?.clone(),
            })
        })
        .collect())
}

/// One line of a `.dev.vars` file, with its value already quoted.
#[derive(Debug)]
struct DotenvEntry {
    /// The variable's name.
    name: String,
    /// The value, quoted for a dotenv reader.
    value: String,
}

#[derive(Template)]
#[template(path = "dev_vars.tmpl", escape = "none")]
struct DevVarsTemplate {
    /// The entries to write, in name order.
    entries: Vec<DotenvEntry>,
}

/// Quote one value so that a dotenv reader hands back exactly it.
///
/// wrangler parses `.dev.vars` with npm's `dotenv`, which strips the surrounding quotes and, for a
/// *double*-quoted value, turns `\n` and `\r` into real characters while leaving every other
/// backslash exactly where it is. It has no `\"` or `\\` escape, so escaping a quote or a
/// backslash the way a Rust or shell reader expects would put a literal backslash into the value.
/// A *single*-quoted value carries no escapes at all in either that parser or Rust's `dotenvy`, so
/// it is the faithful form for anything without a `'` in it — backslashes, double quotes and real
/// newlines included. A value that does contain one falls back to double quotes, which is faithful
/// as long as it carries neither a backslash nor a double quote.
///
/// # Errors
///
/// Fails for a value holding a single quote together with a double quote or a backslash: no
/// quoting survives that round trip, and writing an approximation would hand the application a
/// different secret than the one that was set.
fn quote_dotenv_value(value: &str) -> Result<String> {
    if !value.contains('\'') {
        return Ok(format!("'{value}'"));
    }
    if !value.contains(['"', '\\']) {
        return Ok(format!("\"{value}\""));
    }
    anyhow::bail!(
        "the value holds a single quote together with a double quote or a backslash, and \
         wrangler's `.dev.vars` reader has no escape that survives that round trip. Use a value \
         without that combination for local development; the deployed value is unaffected."
    )
}

#[cfg(test)]
mod tests {
    use super::{is_empty_listing, parse_secret_rows, quote_dotenv_value, SecretRow};

    /// The table wrangler renders, with the columns its `logger.table` derives from a listing.
    const LISTING: &str = "🔐 Listing secrets... (store-id: store_1, page: 1, per-page: 100)\n\
         ┌───────────┬──────────┬─────────┬────────┬────────┬─────────┬──────────┐\n\
         │ Name      │ ID       │ Comment │ Scopes │ Status │ Created │ Modified │\n\
         ├───────────┼──────────┼─────────┼────────┼────────┼─────────┼──────────┤\n\
         │ stripe-key│ id_one   │         │ workers│ active │ …       │ …        │\n\
         ├───────────┼──────────┼─────────┼────────┼────────┼─────────┼──────────┤\n\
         │ other     │ id_two   │         │ workers│ active │ …       │ …        │\n\
         └───────────┴──────────┴─────────┴────────┴────────┴─────────┴──────────┘\n";

    #[test]
    fn the_listing_is_read_through_its_header_row() {
        let rows = parse_secret_rows(LISTING).expect("a table wrangler could have printed");
        assert_eq!(
            rows,
            [
                SecretRow {
                    name: "stripe-key".to_owned(),
                    id: "id_one".to_owned()
                },
                SecretRow {
                    name: "other".to_owned(),
                    id: "id_two".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_coloured_listing_reads_the_same() {
        let coloured = LISTING.replace("stripe-key", "\u{1b}[34mstripe-key\u{1b}[39m");
        let rows = parse_secret_rows(&coloured).expect("colour is not part of a name");
        assert_eq!(rows[0].name, "stripe-key");
    }

    #[test]
    fn a_table_whose_columns_cannot_be_found_is_an_error_rather_than_a_guess() {
        let error = parse_secret_rows("│ Identifier │ Value │\n│ a │ b │\n")
            .expect_err("Skyzen cannot tell whether the secret exists");
        assert!(error.to_string().contains("Name"), "{error}");
    }

    #[test]
    fn an_empty_store_is_reported_as_a_failure_and_is_not_one() {
        assert!(is_empty_listing(
            "✘ [ERROR] List request returned no secrets."
        ));
        assert!(!is_empty_listing("✘ [ERROR] Authentication error"));
    }

    #[test]
    fn a_quoted_value_is_read_back_as_itself() {
        for value in [
            "sk_live_123",
            r"back\slash",
            "with \"double\" quotes",
            "line\nbreak",
            "with 'single' quotes",
            "  padded  ",
            "",
        ] {
            let quoted = quote_dotenv_value(value).expect("representable");
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join(".dev.vars");
            std::fs::write(&path, format!("STRIPE_KEY={quoted}\n")).expect("write");

            let parsed: Vec<(String, String)> = dotenvy::from_path_iter(&path)
                .expect("read")
                .map(|entry| entry.expect("parse"))
                .collect();
            assert_eq!(
                parsed,
                [("STRIPE_KEY".to_owned(), value.to_owned())],
                "{value:?} was written as {quoted}"
            );
        }
    }

    #[test]
    fn a_value_no_quoting_survives_is_refused_rather_than_corrupted() {
        let error = quote_dotenv_value("both ' and \" quotes").expect_err("not representable");
        assert!(error.to_string().contains("single quote"), "{error}");
    }
}
