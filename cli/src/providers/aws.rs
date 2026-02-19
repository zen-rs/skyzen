use crate::{
    args::Action,
    manifest::LoadedManifest,
    providers::{CommandPlan, ProviderPlan},
};
use anyhow::{Context, Result};

pub fn prepare(action: Action, manifest: &LoadedManifest) -> Result<ProviderPlan> {
    let config = manifest
        .data
        .aws
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing [aws] section in Skyzen.toml"))?;

    let template = config
        .template
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("template.yaml");

    let command = match action {
        Action::Dev => {
            let mut args = vec![
                "local".to_owned(),
                "start-api".to_owned(),
                "--template".to_owned(),
                template.to_owned(),
            ];
            if let Some(port) = config.local_port {
                args.push("--port".to_owned());
                args.push(port.to_string());
            }
            if let Some(env_vars) = &config.env_vars {
                args.push("--env-vars".to_owned());
                args.push(env_vars.clone());
            }

            CommandPlan {
                program: "sam".to_owned(),
                args,
                cwd: Some(manifest.root_dir.clone()),
            }
        }
        Action::Deploy => {
            let stack_name = config
                .stack_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .context("aws.stack_name is required for `skyzen deploy --provider aws`")?;

            let mut args = vec![
                "deploy".to_owned(),
                "--template-file".to_owned(),
                template.to_owned(),
                "--stack-name".to_owned(),
                stack_name.to_owned(),
                "--no-confirm-changeset".to_owned(),
                "--no-fail-on-empty-changeset".to_owned(),
            ];
            if let Some(region) = &config.region {
                args.push("--region".to_owned());
                args.push(region.clone());
            }
            if let Some(profile) = &config.profile {
                args.push("--profile".to_owned());
                args.push(profile.clone());
            }

            CommandPlan {
                program: "sam".to_owned(),
                args,
                cwd: Some(manifest.root_dir.clone()),
            }
        }
        Action::Doctor => unreachable!("doctor is handled in providers::prepare"),
    };

    Ok(ProviderPlan {
        commands: vec![command],
        generated_files: Vec::new(),
    })
}
