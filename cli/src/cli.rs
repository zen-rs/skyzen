//! The command surface, defined with `clap`'s derive API.
//!
//! Deriving rather than hand-parsing is what supplies `--version`, `--help` on stdout with a zero
//! exit code, per-subcommand help, "did you mean" suggestions and shell completions — all of which
//! a bespoke parser has to reimplement one at a time.

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Unified local emulation and deployment CLI for Skyzen.
#[derive(Debug, Parser)]
#[command(name = "skyzen", version, about, propagate_version = true)]
pub struct Cli {
    /// Path to the project manifest.
    #[arg(long, short = 'm', global = true, default_value = "Skyzen.toml")]
    pub manifest: PathBuf,

    /// Target platform.
    #[arg(long, short = 'p', global = true)]
    pub provider: Option<Provider>,

    /// Named environment from `[cloudflare.env.<name>]`, forwarded to wrangler as `--env`.
    #[arg(long, short = 'e', global = true)]
    pub env: Option<String>,

    /// Print what would happen without writing files or running the deployment.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The verbs `skyzen` accepts.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold a new Skyzen project.
    New {
        /// Directory to create the project in. `.` scaffolds into the current directory.
        path: PathBuf,

        /// Starting point to generate.
        #[arg(long, short = 't', default_value = "api")]
        template: Template,

        /// Reuse a non-empty directory, keeping every file that already exists.
        #[arg(long, short = 'f')]
        force: bool,

        /// Reuse a non-empty directory and replace files that already exist.
        #[arg(long, conflicts_with = "force")]
        overwrite: bool,
    },

    /// Add the crates a capability needs to the project's Cargo.toml, via `cargo add`.
    Add {
        /// Capabilities to add. Run `skyzen add --help` for the full list.
        #[arg(required = true)]
        capabilities: Vec<String>,

        /// Print the `cargo add` invocations without running them.
        #[arg(long)]
        list: bool,
    },

    /// Build deployment artifacts without running or deploying anything.
    Build {
        /// Build optimized artifacts, as `deploy` does.
        #[arg(long)]
        release: bool,
    },

    /// Run the project locally, rebuilding on source changes.
    Dev,

    /// Build and deploy the project.
    Deploy,

    /// Create the cloud resources the manifest declares but has no id for.
    Provision,

    /// Stream live logs from the deployed Worker (`wrangler tail`).
    Logs {
        /// Extra arguments forwarded verbatim to `wrangler tail`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        wrangler_args: Vec<String>,
    },

    /// Manage the deployed Worker's secrets.
    Secret {
        /// The secret operation to perform.
        #[command(subcommand)]
        command: SecretCommand,
    },

    /// Check that the toolchain and the manifest can actually deploy this project.
    Doctor,

    /// Print a shell completion script.
    Completions {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

/// The `skyzen secret` operations.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum SecretCommand {
    /// Set a secret, reading its value from stdin.
    Set {
        /// The secret's name, as seen from `env` in the Worker.
        name: String,
    },
    /// List the secrets the deployed Worker has.
    List,
}

/// The platforms a project can target.
///
/// AWS and Azure have service implementations (`skyzen-aws`, `skyzen-azure`) but no deployment
/// adapter yet, so they are deliberately absent rather than accepted and then rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Provider {
    /// A native binary, run locally.
    Native,
    /// A Cloudflare Worker.
    Cloudflare,
}

impl Provider {
    /// The provider's spelling on the command line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Cloudflare => "cloudflare",
        }
    }
}

/// The starting points `skyzen new` can generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Template {
    /// A JSON API with a portable KV service wired for both native and Cloudflare.
    Api,
    /// Two routes and nothing else.
    Minimal,
    /// A Worker with a queue consumer and a cron trigger.
    ServerlessEvents,
    /// A Worker with a WebSocket-serving Durable Object.
    DurableRealtime,
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, Provider, Template};
    use clap::{CommandFactory, Parser};

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("parse should succeed")
    }

    #[test]
    fn the_command_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_are_accepted_before_and_after_the_subcommand() {
        let before = parse(&["skyzen", "--dry-run", "doctor"]);
        assert!(before.dry_run);
        assert!(matches!(before.command, Command::Doctor));

        let after = parse(&["skyzen", "doctor", "--dry-run"]);
        assert!(after.dry_run);
    }

    #[test]
    fn the_legacy_flag_spellings_still_parse() {
        let parsed = parse(&[
            "skyzen",
            "--provider=cloudflare",
            "--manifest=custom.toml",
            "deploy",
        ]);
        assert_eq!(parsed.provider, Some(Provider::Cloudflare));
        assert_eq!(parsed.manifest, std::path::PathBuf::from("custom.toml"));
        assert!(matches!(parsed.command, Command::Deploy));

        let short = parse(&["skyzen", "-p", "native", "dev"]);
        assert_eq!(short.provider, Some(Provider::Native));
    }

    #[test]
    fn new_takes_a_positional_path_and_a_template() {
        let parsed = parse(&[
            "skyzen",
            "new",
            "demo-app",
            "--template",
            "serverless-events",
            "--force",
        ]);
        let Command::New {
            path,
            template,
            force,
            overwrite,
        } = parsed.command
        else {
            panic!("expected `new`");
        };
        assert_eq!(path, std::path::PathBuf::from("demo-app"));
        assert_eq!(template, Template::ServerlessEvents);
        assert!(force);
        assert!(!overwrite);
    }

    #[test]
    fn an_unreachable_provider_is_rejected_by_the_parser() {
        let error = Cli::try_parse_from(["skyzen", "--provider=aws", "deploy"])
            .expect_err("aws has no deployment adapter yet");
        let rendered = error.to_string();
        assert!(
            rendered.contains("native") && rendered.contains("cloudflare"),
            "the error should list the providers that do work: {rendered}"
        );
    }

    #[test]
    fn logs_forwards_trailing_arguments_to_wrangler() {
        let parsed = parse(&["skyzen", "logs", "--format", "json"]);
        let Command::Logs { wrangler_args } = parsed.command else {
            panic!("expected `logs`");
        };
        assert_eq!(wrangler_args, vec!["--format", "json"]);
    }

    #[test]
    fn help_and_version_render_on_stdout() {
        // The hand-rolled parser this replaced wrote usage to stderr and exited 2, so piping help
        // into a pager showed nothing and shell scripts saw a failure.
        for flag in ["--help", "--version"] {
            let error = Cli::try_parse_from(["skyzen", flag]).expect_err("clap reports these");
            assert!(
                !error.use_stderr(),
                "`{flag}` must render on stdout, not stderr"
            );
        }
    }
}
