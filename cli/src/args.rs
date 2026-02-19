use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Dev,
    Deploy,
    Doctor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Cloudflare,
    Aws,
    Azure,
}

#[derive(Debug, Clone)]
pub struct CliOptions {
    pub action: Action,
    pub provider: Option<Provider>,
    pub manifest: PathBuf,
    pub dry_run: bool,
}

impl CliOptions {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut args = args.into_iter();
        let _ = args.next();

        let Some(action_raw) = args.next() else {
            print_usage_and_exit();
        };
        let action = parse_action(&action_raw)?;

        let mut provider = None;
        let mut manifest = PathBuf::from("Skyzen.toml");
        let mut dry_run = false;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--provider" | "-p" => {
                    let value = args
                        .next()
                        .context("missing value for --provider (expected cloudflare|aws|azure)")?;
                    provider = Some(parse_provider(&value)?);
                }
                "--manifest" | "-m" => {
                    let value = args
                        .next()
                        .context("missing value for --manifest (expected path)")?;
                    manifest = PathBuf::from(value);
                }
                "--dry-run" => {
                    dry_run = true;
                }
                "--help" | "-h" => {
                    print_usage_and_exit();
                }
                unknown => {
                    anyhow::bail!("unsupported argument: {unknown}");
                }
            }
        }

        match action {
            Action::Doctor => {}
            Action::Dev | Action::Deploy => {
                if provider.is_none() {
                    anyhow::bail!("--provider is required for dev/deploy");
                }
            }
        }

        Ok(Self {
            action,
            provider,
            manifest,
            dry_run,
        })
    }
}

fn parse_action(value: &str) -> Result<Action> {
    match value {
        "dev" => Ok(Action::Dev),
        "deploy" => Ok(Action::Deploy),
        "doctor" => Ok(Action::Doctor),
        _ => anyhow::bail!("unsupported action: {value} (expected dev|deploy|doctor)"),
    }
}

fn parse_provider(value: &str) -> Result<Provider> {
    match value {
        "cloudflare" => Ok(Provider::Cloudflare),
        "aws" => Ok(Provider::Aws),
        "azure" => Ok(Provider::Azure),
        _ => anyhow::bail!("unsupported provider: {value} (expected cloudflare|aws|azure)"),
    }
}

fn print_usage_and_exit() -> ! {
    eprintln!(
        "Usage:
  skyzen dev --provider <cloudflare|aws|azure> [--manifest Skyzen.toml] [--dry-run]
  skyzen deploy --provider <cloudflare|aws|azure> [--manifest Skyzen.toml] [--dry-run]
  skyzen doctor [--provider <cloudflare|aws|azure>]"
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
            "--provider".to_string(),
            "cloudflare".to_string(),
            "--dry-run".to_string(),
        ];
        let parsed = CliOptions::parse(args).expect("parse should succeed");
        assert_eq!(parsed.action, Action::Dev);
        assert_eq!(parsed.provider, Some(Provider::Cloudflare));
        assert!(parsed.dry_run);
    }

    #[test]
    fn deploy_requires_provider() {
        let args = vec!["skyzen".to_string(), "deploy".to_string()];
        let error = CliOptions::parse(args).expect_err("parse should fail");
        assert!(error.to_string().contains("--provider"));
    }
}
