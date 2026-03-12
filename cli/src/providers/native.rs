use std::path::PathBuf;

use anyhow::Result;

use crate::{
    args::Action,
    providers::{CommandPlan, ProviderPlan, RunMode},
};

pub fn prepare(action: Action, root_dir: PathBuf) -> Result<ProviderPlan> {
    match action {
        Action::Dev => Ok(ProviderPlan {
            commands: vec![CommandPlan {
                program: "cargo".to_owned(),
                args: vec!["run".to_owned()],
                cwd: Some(root_dir),
            }],
            generated_files: Vec::new(),
            run_mode: RunMode::Watch,
        }),
        Action::Deploy => anyhow::bail!("native deploy is not supported"),
        Action::Doctor | Action::New => unreachable!("handled outside provider planning"),
    }
}
