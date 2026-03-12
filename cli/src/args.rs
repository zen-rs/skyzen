use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    New,
    Dev,
    Deploy,
    Doctor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Native,
    Cloudflare,
    Aws,
    Azure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    Api,
    ServerlessEvents,
    DurableRealtime,
}

#[derive(Debug, Clone)]
pub struct CliOptions {
    pub action: Action,
    pub provider: Option<Provider>,
    pub manifest: PathBuf,
    pub dry_run: bool,
    pub template: Template,
    pub path: Option<PathBuf>,
    pub force: bool,
}

impl CliOptions {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut args = args.into_iter();
        let _ = args.next();

        let mut action = None;
        let mut provider = None;
        let mut manifest = PathBuf::from("Skyzen.toml");
        let mut dry_run = false;
        let mut template = Template::Api;
        let mut path = None;
        let mut force = false;

        while let Some(arg) = args.next() {
            if let Some(value) = arg.strip_prefix("--provider=") {
                provider = Some(parse_provider(value)?);
                continue;
            }

            if let Some(value) = arg.strip_prefix("--manifest=") {
                manifest = PathBuf::from(value);
                continue;
            }

            if let Some(value) = arg.strip_prefix("--template=") {
                template = parse_template(value)?;
                continue;
            }

            match arg.as_str() {
                "--provider" | "-p" => {
                    let value = args.next().context(
                        "missing value for --provider (expected native|cloudflare|aws|azure)",
                    )?;
                    provider = Some(parse_provider(&value)?);
                }
                "--manifest" | "-m" => {
                    let value = args
                        .next()
                        .context("missing value for --manifest (expected path)")?;
                    manifest = PathBuf::from(value);
                }
                "--template" | "-t" => {
                    let value = args
                        .next()
                        .context("missing value for --template (expected api)")?;
                    template = parse_template(&value)?;
                }
                "--dry-run" => {
                    dry_run = true;
                }
                "--force" | "-f" => {
                    force = true;
                }
                "--help" | "-h" => {
                    print_usage_and_exit();
                }
                unknown if unknown.starts_with('-') => {
                    anyhow::bail!("unsupported argument: {unknown}");
                }
                action_raw => {
                    if action.is_none() {
                        action = Some(parse_action(action_raw)?);
                    } else if path.is_none() {
                        path = Some(PathBuf::from(action_raw));
                    } else {
                        anyhow::bail!("unexpected positional argument: {action_raw}");
                    }
                }
            }
        }

        let Some(action) = action else {
            print_usage_and_exit();
        };

        match action {
            Action::New => {
                if path.is_none() {
                    anyhow::bail!("project path is required for `skyzen new`");
                }
            }
            Action::Doctor => {}
            Action::Dev | Action::Deploy => {
                if action == Action::Deploy && provider.is_none() {
                    anyhow::bail!("--provider is required for deploy");
                }
            }
        }

        Ok(Self {
            action,
            provider,
            manifest,
            dry_run,
            template,
            path,
            force,
        })
    }
}

fn parse_action(value: &str) -> Result<Action> {
    match value {
        "new" => Ok(Action::New),
        "dev" => Ok(Action::Dev),
        "deploy" => Ok(Action::Deploy),
        "doctor" => Ok(Action::Doctor),
        _ => anyhow::bail!("unsupported action: {value} (expected new|dev|deploy|doctor)"),
    }
}

fn parse_provider(value: &str) -> Result<Provider> {
    match value {
        "native" => Ok(Provider::Native),
        "cloudflare" => Ok(Provider::Cloudflare),
        "aws" => Ok(Provider::Aws),
        "azure" => Ok(Provider::Azure),
        _ => anyhow::bail!("unsupported provider: {value} (expected native|cloudflare|aws|azure)"),
    }
}

fn parse_template(value: &str) -> Result<Template> {
    match value {
        "api" => Ok(Template::Api),
        "serverless-events" => Ok(Template::ServerlessEvents),
        "durable-realtime" => Ok(Template::DurableRealtime),
        _ => anyhow::bail!(
            "unsupported template: {value} (expected api|serverless-events|durable-realtime)"
        ),
    }
}

fn print_usage_and_exit() -> ! {
    eprintln!(
        "Usage:
  skyzen new <path> [--template api|serverless-events|durable-realtime] [--force]
  skyzen dev [--provider <native|cloudflare|aws|azure>] [--manifest Skyzen.toml] [--dry-run]
  skyzen deploy --provider <cloudflare|aws|azure> [--manifest Skyzen.toml] [--dry-run]
  skyzen doctor [--provider <native|cloudflare|aws|azure>]"
    );
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dev_options() {
        let args = vec![
            "skyzen".to_string(),
            "dev".to_string(),
            "--dry-run".to_string(),
        ];
        let parsed = CliOptions::parse(args).expect("parse should succeed");
        assert_eq!(parsed.action, Action::Dev);
        assert_eq!(parsed.provider, None);
        assert!(parsed.dry_run);
    }

    #[test]
    fn deploy_requires_provider() {
        let args = vec!["skyzen".to_string(), "deploy".to_string()];
        let error = CliOptions::parse(args).expect_err("parse should fail");
        assert!(error.to_string().contains("--provider"));
    }

    #[test]
    fn parses_global_flag_before_action() {
        let args = vec![
            "skyzen".to_string(),
            "--dry-run".to_string(),
            "doctor".to_string(),
        ];
        let parsed = CliOptions::parse(args).expect("parse should succeed");
        assert_eq!(parsed.action, Action::Doctor);
        assert!(parsed.dry_run);
    }

    #[test]
    fn parses_equals_style_flags() {
        let args = vec![
            "skyzen".to_string(),
            "--provider=aws".to_string(),
            "--manifest=custom.toml".to_string(),
            "deploy".to_string(),
        ];
        let parsed = CliOptions::parse(args).expect("parse should succeed");
        assert_eq!(parsed.action, Action::Deploy);
        assert_eq!(parsed.provider, Some(Provider::Aws));
        assert_eq!(parsed.manifest, PathBuf::from("custom.toml"));
    }

    #[test]
    fn parses_new_options() {
        let args = vec![
            "skyzen".to_string(),
            "new".to_string(),
            "demo-app".to_string(),
            "--template".to_string(),
            "serverless-events".to_string(),
            "--force".to_string(),
        ];
        let parsed = CliOptions::parse(args).expect("parse should succeed");
        assert_eq!(parsed.action, Action::New);
        assert_eq!(parsed.template, Template::ServerlessEvents);
        assert_eq!(parsed.path, Some(PathBuf::from("demo-app")));
        assert!(parsed.force);
    }
}
