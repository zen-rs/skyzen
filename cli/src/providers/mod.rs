mod aws;
mod azure;
mod cloudflare;
mod native;

use crate::{
    args::{Action, CliOptions, Provider},
    deps::ensure_capability_deps,
    manifest::LoadedManifest,
};
use anyhow::Result;
use std::{
    env,
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
pub enum InternalStep {
    CloudflareBuild(cloudflare::CloudflareBuildPlan),
}

impl InternalStep {
    pub fn display(&self) -> String {
        match self {
            Self::CloudflareBuild(plan) => format!(
                "build Cloudflare worker artifacts into {}",
                plan.output_dir.display()
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreparedRun {
    pub action: Action,
    pub commands: Vec<CommandPlan>,
    pub generated_files: Vec<GeneratedFile>,
    pub internal_steps: Vec<InternalStep>,
    pub run_mode: RunMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunMode {
    #[default]
    Once,
    Watch,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderPlan {
    commands: Vec<CommandPlan>,
    generated_files: Vec<GeneratedFile>,
    internal_steps: Vec<InternalStep>,
    run_mode: RunMode,
}

impl ProviderPlan {
    fn into_prepared(self, action: Action) -> PreparedRun {
        PreparedRun {
            action,
            commands: self.commands,
            generated_files: self.generated_files,
            internal_steps: self.internal_steps,
            run_mode: self.run_mode,
        }
    }
}

pub fn prepare(options: &CliOptions) -> Result<PreparedRun> {
    match options.action {
        Action::New => unreachable!("new is handled by the CLI entrypoint"),
        Action::Doctor => {
            run_doctor(options.provider)?;
            Ok(PreparedRun {
                action: Action::Doctor,
                commands: Vec::new(),
                generated_files: Vec::new(),
                internal_steps: Vec::new(),
                run_mode: RunMode::Once,
            })
        }
        Action::Dev | Action::Deploy => {
            let provider = options.provider.unwrap_or(Provider::Native);
            if matches!(options.action, Action::Deploy) && provider == Provider::Native {
                anyhow::bail!("`skyzen deploy` requires a cloud provider");
            }

            if matches!(options.action, Action::Dev) && provider == Provider::Native {
                let root_dir = if options.manifest.exists() {
                    LoadedManifest::load(&options.manifest)?.root_dir
                } else {
                    env::current_dir()?
                };
                let plan = native::prepare(options.action, root_dir)?;
                return Ok(plan.into_prepared(options.action));
            }

            let manifest = LoadedManifest::load(&options.manifest)?;
            ensure_capability_deps(&manifest, Some(provider))?;
            let plan = match provider {
                Provider::Native => unreachable!("native dev is handled above"),
                Provider::Cloudflare => cloudflare::prepare(options.action, &manifest)?,
                Provider::Aws => aws::prepare(options.action, &manifest)?,
                Provider::Azure => azure::prepare(options.action, &manifest)?,
            };
            Ok(plan.into_prepared(options.action))
        }
    }
}

pub fn run_internal_step(step: &InternalStep) -> Result<()> {
    match step {
        InternalStep::CloudflareBuild(plan) => cloudflare::run_build(plan),
    }
}

fn run_doctor(provider: Option<Provider>) -> Result<()> {
    let providers = provider.map_or_else(
        || {
            vec![
                Provider::Native,
                Provider::Cloudflare,
                Provider::Aws,
                Provider::Azure,
            ]
        },
        |p| vec![p],
    );

    let mut missing = Vec::new();
    for provider in providers {
        let (label, binaries): (&str, &[&str]) = match provider {
            Provider::Native => ("native", &["cargo"]),
            Provider::Cloudflare => ("cloudflare", &["cargo", "wrangler"]),
            Provider::Aws => ("aws", &["sam"]),
            Provider::Azure => ("azure", &["func"]),
        };
        for binary in binaries {
            if binary_exists(binary) {
                println!("[ok] {label}: found `{binary}`");
            } else {
                println!("[missing] {label}: `{binary}` not found");
                missing.push(*binary);
            }
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
