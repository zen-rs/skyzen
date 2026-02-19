mod aws;
mod azure;
mod cloudflare;

use crate::{
    args::{Action, CliOptions, Provider},
    manifest::LoadedManifest,
};
use anyhow::Result;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Debug, Clone)]
pub struct CommandPlan {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl CommandPlan {
    pub fn display(&self) -> String {
        let command = if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        };
        match &self.cwd {
            Some(cwd) => format!("(cd {} && {command})", cwd.display()),
            None => command,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GeneratedFile {
    pub path: PathBuf,
    pub contents: String,
}

#[derive(Debug, Clone)]
pub struct PreparedRun {
    pub action: Action,
    pub commands: Vec<CommandPlan>,
    pub generated_files: Vec<GeneratedFile>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProviderPlan {
    commands: Vec<CommandPlan>,
    generated_files: Vec<GeneratedFile>,
}

impl ProviderPlan {
    fn into_prepared(self, action: Action) -> PreparedRun {
        PreparedRun {
            action,
            commands: self.commands,
            generated_files: self.generated_files,
        }
    }
}

pub fn prepare(options: &CliOptions) -> Result<PreparedRun> {
    match options.action {
        Action::Doctor => {
            run_doctor(options.provider)?;
            Ok(PreparedRun {
                action: Action::Doctor,
                commands: Vec::new(),
                generated_files: Vec::new(),
            })
        }
        Action::Dev | Action::Deploy => {
            let manifest = LoadedManifest::load(&options.manifest)?;
            let provider = options
                .provider
                .ok_or_else(|| anyhow::anyhow!("--provider is required"))?;
            let plan = match provider {
                Provider::Cloudflare => cloudflare::prepare(options.action, &manifest)?,
                Provider::Aws => aws::prepare(options.action, &manifest)?,
                Provider::Azure => azure::prepare(options.action, &manifest)?,
            };
            Ok(plan.into_prepared(options.action))
        }
    }
}

fn run_doctor(provider: Option<Provider>) -> Result<()> {
    let providers = match provider {
        Some(p) => vec![p],
        None => vec![Provider::Cloudflare, Provider::Aws, Provider::Azure],
    };

    let mut missing = Vec::new();
    for provider in providers {
        let (label, binary) = match provider {
            Provider::Cloudflare => ("cloudflare", "wrangler"),
            Provider::Aws => ("aws", "sam"),
            Provider::Azure => ("azure", "func"),
        };
        if binary_exists(binary) {
            println!("[ok] {label}: found `{binary}`");
        } else {
            println!("[missing] {label}: `{binary}` not found");
            missing.push(binary);
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "missing required emulator/deploy tool(s): {}",
            missing.join(", ")
        )
    }
}

fn binary_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
