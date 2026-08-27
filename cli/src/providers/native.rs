//! The native provider: run the application as an ordinary binary.

use crate::providers::{prepare_child_environment, Action, CommandPlan, ProviderPlan, RunMode};
use anyhow::Result;
use skyzen_manifest::Manifest;

/// Build the plan for one native action.
///
/// # Errors
///
/// Fails when the action has no native meaning, or when the manifest declares an environment
/// variable that is set nowhere.
pub fn prepare(action: &Action, manifest: &Manifest) -> Result<ProviderPlan> {
    let root_dir = manifest.root_dir().to_path_buf();

    let (args, run_mode) = match action {
        Action::Dev => (vec!["run".to_owned()], RunMode::Restart),
        Action::Build { release } => {
            let mut args = vec!["build".to_owned()];
            if *release {
                args.push("--release".to_owned());
            }
            (args, RunMode::Once)
        }
        other => anyhow::bail!(
            "`skyzen {}` has no native implementation; native applications are deployed as \
             ordinary binaries",
            super::action_name(other)
        ),
    };

    // Read the manifest's `url_env` / `bucket_env` declarations *before* starting the child, so a
    // variable the application would have panicked on at its first request is a startup error
    // naming the manifest key that asked for it.
    let child_env = prepare_child_environment(manifest)?;

    Ok(ProviderPlan {
        commands: vec![CommandPlan {
            program: "cargo".to_owned(),
            args,
            cwd: Some(root_dir.clone()),
        }],
        generated_files: Vec::new(),
        build: None,
        run_mode,
        child_env,
        watch_root: matches!(action, Action::Dev).then_some(root_dir),
        execute_despite_dry_run: false,
    })
}

#[cfg(test)]
mod tests {
    use super::prepare;
    use crate::providers::{Action, RunMode};
    use skyzen_manifest::Manifest;

    fn manifest(source: &str) -> Manifest {
        Manifest::parse(source, "Skyzen.toml", "/tmp/app").expect("valid manifest")
    }

    #[test]
    fn dev_supervises_cargo_run_and_build_does_not() {
        let manifest = manifest("");

        let dev = prepare(&Action::Dev, &manifest).expect("dev plan");
        assert_eq!(dev.run_mode, RunMode::Restart);
        assert!(dev.commands[0].display().contains("cargo run"));
        assert!(dev.watch_root.is_some());

        let build = prepare(&Action::Build { release: true }, &manifest).expect("build plan");
        assert_eq!(build.run_mode, RunMode::Once);
        assert!(build.commands[0]
            .display()
            .contains("cargo build --release"));
        assert!(build.watch_root.is_none());
    }

    #[test]
    fn a_cloud_only_action_is_refused_with_a_reason() {
        let error = prepare(&Action::Deploy, &manifest("")).expect_err("native cannot deploy");
        assert!(
            error.to_string().contains("no native implementation"),
            "{error}"
        );
    }

    #[test]
    fn a_declared_variable_that_is_set_nowhere_stops_dev_before_it_starts() {
        let manifest = manifest(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [native.service.cache]\nbackend = \"redis\"\nurl_env = \"SKYZEN_NEVER_SET_URL\"\n",
        );
        let error = prepare(&Action::Dev, &manifest).expect_err("the variable is not set");
        assert!(
            error.to_string().contains("SKYZEN_NEVER_SET_URL"),
            "{error}"
        );
    }
}
