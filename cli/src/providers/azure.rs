use crate::{
    args::Action,
    manifest::LoadedManifest,
    providers::{CommandPlan, ProviderPlan},
};
use anyhow::{Context, Result};

pub fn prepare(action: Action, manifest: &LoadedManifest) -> Result<ProviderPlan> {
    let config = manifest
        .data
        .azure
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing [azure] section in Skyzen.toml"))?;

    let project_dir = manifest
        .root_dir
        .join(config.project.as_deref().unwrap_or("."));
    let command = match action {
        Action::Dev => {
            let mut args = vec!["start".to_owned()];
            if let Some(port) = config.port {
                args.push("--port".to_owned());
                args.push(port.to_string());
            }

            CommandPlan {
                program: "func".to_owned(),
                args,
                cwd: Some(project_dir),
            }
        }
        Action::Deploy => {
            let app_name = config
                .app_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .context("azure.app_name is required for `skyzen deploy --provider azure`")?;
            CommandPlan {
                program: "func".to_owned(),
                args: vec![
                    "azure".to_owned(),
                    "functionapp".to_owned(),
                    "publish".to_owned(),
                    app_name.to_owned(),
                ],
                cwd: Some(project_dir),
            }
        }
        Action::Doctor => unreachable!("doctor is handled in providers::prepare"),
    };

    Ok(ProviderPlan {
        commands: vec![command],
        generated_files: Vec::new(),
    })
}
