//! The command surface, defined with `clap`'s derive API.
//!
//! Deriving rather than hand-parsing is what supplies `--version`, `--help` on stdout with a zero
//! exit code, per-subcommand help, "did you mean" suggestions and shell completions — all of which
//! a bespoke parser has to reimplement one at a time.

use clap::{Parser, Subcommand, ValueEnum};
use skyzen_manifest::VarName;
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
    Dev {
        /// Extra arguments forwarded verbatim to the underlying runner (`wrangler dev` or
        /// `cargo run`). This is how wrangler-only flags such as `--test-scheduled` are reached.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        runner_args: Vec<String>,
    },

    /// Build and deploy the project.
    Deploy,

    /// Create the cloud resources the manifest declares but has no id for.
    Provision,

    /// Apply pending SQL migrations to every declared database.
    ///
    /// With no subcommand, pending migrations are applied. `--provider cloudflare` (the default)
    /// hands the work to `wrangler d1 migrations`; `--provider native` connects through the
    /// `[native.database.<name>].url_env` connection string and does it itself. `status` takes the
    /// same branch as applying, so it always reports on the database `migrate` would write to.
    Migrate {
        /// What to do. Applying pending migrations is the default.
        #[command(subcommand)]
        command: Option<MigrateCommand>,

        /// Apply to the local emulator's database rather than the deployed one.
        #[arg(long)]
        local: bool,
    },

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

/// The `skyzen migrate` operations beyond applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum MigrateCommand {
    /// List which migrations have been applied and which are still pending.
    ///
    /// On Cloudflare this is `wrangler d1 migrations list`. On `--provider native` it reads the
    /// `_skyzen_migrations` bookkeeping table through the connection string, creating that table if
    /// the database has never been migrated — otherwise there would be nothing to report on — but
    /// applying nothing.
    Status,
}

/// The `skyzen secret` operations.
///
/// Every one of them works on a name the manifest declares as `[[secret]]`: the CLI has no notion
/// of a secret the application does not read, and a typo is refused rather than uploaded.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum SecretCommand {
    /// Set one secret, reading its value from this command's standard input.
    Set {
        /// The secret's name, as declared by a `[[secret]]` entry.
        name: VarName,
    },
    /// Deliver every declared secret's local value to the deployment.
    ///
    /// The same delivery `skyzen deploy` performs, without rebuilding: the values come from the
    /// process environment and the project's `.env` files, and a missing one is refused.
    Push,
    /// List the secrets the deployment has.
    List,
}

/// The platforms a project can target.
///
/// Every one of these deploys the *same* application: the runtime detects where it is running, so
/// picking a provider chooses a deployment pipeline rather than a way of writing the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Provider {
    /// A native binary, run locally.
    Native,
    /// A Cloudflare Worker.
    Cloudflare,
    /// An AWS Lambda function, deployed with `cargo lambda`.
    Aws,
    /// An Azure Functions custom handler, deployed with `func`.
    Azure,
}

impl Provider {
    /// The provider's spelling on the command line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Cloudflare => "cloudflare",
            Self::Aws => "aws",
            Self::Azure => "azure",
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
    fn every_provider_with_a_deployment_adapter_is_reachable() {
        for (spelling, expected) in [
            ("native", Provider::Native),
            ("cloudflare", Provider::Cloudflare),
            ("aws", Provider::Aws),
            ("azure", Provider::Azure),
        ] {
            let parsed = parse(&["skyzen", &format!("--provider={spelling}"), "deploy"]);
            assert_eq!(parsed.provider, Some(expected), "{spelling}");
        }
    }

    #[test]
    fn a_platform_with_no_adapter_is_rejected_by_the_parser() {
        let error = Cli::try_parse_from(["skyzen", "--provider=fly", "deploy"])
            .expect_err("there is no fly.io adapter");
        let rendered = error.to_string();
        assert!(
            rendered.contains("cloudflare") && rendered.contains("aws"),
            "the error should list the providers that do work: {rendered}"
        );
    }

    #[test]
    fn dev_forwards_trailing_arguments_to_the_runner() {
        let parsed = parse(&["skyzen", "dev", "--test-scheduled"]);
        let Command::Dev { runner_args } = parsed.command else {
            panic!("expected `dev`");
        };
        assert_eq!(runner_args, vec!["--test-scheduled"]);
    }

    #[test]
    fn a_global_flag_after_a_var_arg_subcommand_is_still_a_global_flag() {
        // `skyzen dev --provider cloudflare` is written throughout the docs; a var-arg that
        // swallowed it would silently run the native path instead.
        for (args, expected) in [
            (
                vec!["skyzen", "dev", "--provider", "cloudflare"],
                Provider::Cloudflare,
            ),
            (
                vec!["skyzen", "logs", "--provider", "cloudflare"],
                Provider::Cloudflare,
            ),
        ] {
            let parsed = parse(&args);
            assert_eq!(parsed.provider, Some(expected), "{args:?}");
        }

        let parsed = parse(&[
            "skyzen",
            "dev",
            "--provider",
            "cloudflare",
            "--env",
            "staging",
        ]);
        let Command::Dev { runner_args } = parsed.command else {
            panic!("expected `dev`");
        };
        assert!(runner_args.is_empty(), "{runner_args:?}");
        assert_eq!(parsed.env.as_deref(), Some("staging"));
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
    fn migrate_applies_by_default_and_takes_a_status_subcommand() {
        let apply = parse(&["skyzen", "migrate"]);
        let Command::Migrate { command, local } = apply.command else {
            panic!("expected `migrate`");
        };
        assert_eq!(command, None, "no subcommand means apply");
        assert!(!local);

        let local_apply = parse(&["skyzen", "migrate", "--local"]);
        let Command::Migrate { command, local } = local_apply.command else {
            panic!("expected `migrate`");
        };
        assert_eq!(command, None);
        assert!(local);

        let status = parse(&["skyzen", "migrate", "status"]);
        let Command::Migrate { command, .. } = status.command else {
            panic!("expected `migrate`");
        };
        assert_eq!(command, Some(super::MigrateCommand::Status));
    }

    #[test]
    fn migrate_still_sees_the_global_provider_flag() {
        // `--provider native` is what selects the in-process runner over `wrangler d1 migrations
        // apply`, so an optional subcommand that swallowed the flag would silently run the wrong
        // path.
        for args in [
            vec!["skyzen", "migrate", "--provider", "native"],
            vec!["skyzen", "migrate", "status", "--provider", "native"],
        ] {
            let parsed = parse(&args);
            assert_eq!(parsed.provider, Some(Provider::Native), "{args:?}");
            assert!(
                matches!(parsed.command, Command::Migrate { .. }),
                "{args:?}"
            );
        }
    }

    #[test]
    fn an_unknown_migrate_subcommand_is_rejected() {
        Cli::try_parse_from(["skyzen", "migrate", "rollback"])
            .expect_err("there is no rollback; migrations are forward-only");
    }

    #[test]
    fn a_secret_is_named_by_an_environment_variable_name_or_it_is_not_named() {
        let parsed = parse(&["skyzen", "secret", "set", "STRIPE_KEY"]);
        let Command::Secret { command } = parsed.command else {
            panic!("expected `secret`");
        };
        assert_eq!(
            command,
            super::SecretCommand::Set {
                name: "STRIPE_KEY".parse().expect("a name")
            }
        );

        // The manifest's rule, applied by the parser: a name an operating system would not accept
        // is refused before anything is read from standard input.
        Cli::try_parse_from(["skyzen", "secret", "set", "STRIPE-KEY"])
            .expect_err("not an environment variable name");

        let push = parse(&["skyzen", "secret", "push"]);
        let Command::Secret { command } = push.command else {
            panic!("expected `secret`");
        };
        assert_eq!(command, super::SecretCommand::Push);
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
