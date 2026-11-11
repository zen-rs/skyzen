//! The native provider: run the application as an ordinary binary, and deploy it as a systemd
//! service on the Linux host the CLI runs on.

#[cfg(target_os = "linux")]
mod systemd;

use crate::{
    environment::VariableKind,
    project::Project,
    providers::{
        prepare_child_environment, Action, CommandPlan, CommandStdin, ProviderPlan, RunMode, Step,
    },
};
#[cfg(target_os = "linux")]
use anyhow::Context as _;
use anyhow::Result;
use skyzen_manifest::Manifest;

/// The plan of `skyzen deploy --provider native`: bring this host to `[native.linux]`, install
/// the systemd unit that starts the release binary and re-applies those settings before every
/// start, and (re)start it.
///
/// Nothing is compiled. A deployment runs as root, and a build run as root would leave the
/// project's `target/` owned by root; `skyzen build --provider native --release` builds it first,
/// as its owner.
///
/// # Errors
///
/// Fails off Linux, when the release binary has not been built or the service's transient user
/// could not reach it, when `[native.server]` names no listen address or one the service cannot
/// use, when a variable the application reads is set nowhere, when `[native.linux]` asks for a
/// facility this host lacks or does not report, and when the deployment would change something
/// and the CLI is not root.
#[cfg(target_os = "linux")]
pub fn deploy(manifest: &Manifest, project: &Project, dry_run: bool) -> Result<ProviderPlan> {
    use crate::providers::{resolve_variables, FileContents, GeneratedFile};
    use skyzen_manifest::NativeDatabaseBackend;

    let name = project.binary_target_name()?;
    let binary = project.target_directory().join("release").join(name);
    if !binary.is_file() {
        anyhow::bail!(
            "{} does not exist; build it first with `skyzen build --provider native --release`",
            binary.display()
        );
    }
    let skyzen = std::env::current_exe().context("cannot locate the skyzen executable")?;
    let manifest_path = std::path::absolute(manifest.path())
        .with_context(|| format!("cannot make {} an absolute path", manifest.path().display()))?;
    let root = std::path::absolute(manifest.root_dir()).with_context(|| {
        format!(
            "cannot make {} an absolute path",
            manifest.root_dir().display()
        )
    })?;
    let binary = std::path::absolute(&binary)
        .with_context(|| format!("cannot make {} an absolute path", binary.display()))?;
    {
        ensure_reachable(&root, false)?;
        ensure_reachable(&binary, true)?;
    }
    let native = manifest.data().native.as_ref();
    let sqlite = native.is_some_and(|native| {
        native
            .database
            .values()
            .any(|database| database.backend() == NativeDatabaseBackend::Sqlite)
    });
    let sandbox = systemd::Sandbox::of(
        name,
        native.and_then(|native| native.server.as_ref()),
        unprivileged_port_start()?,
        sqlite,
    )?;
    let unit = systemd::Unit::new(
        name,
        &systemd::Paths {
            root: &root,
            binary: &binary,
            skyzen: &skyzen,
            manifest: &manifest_path,
        },
        sandbox,
    )?;
    let variables = resolve_variables(manifest, VariableKind::ALL, |_| true)?;
    if !dry_run {
        crate::host::ensure_root("`skyzen deploy --provider native`")?;
    }

    let mut steps = host_steps(manifest, dry_run)?;
    let systemctl = |args: &[&str]| {
        Step::Command(CommandPlan {
            program: "systemctl".to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: None,
            stdin: CommandStdin::Inherit,
        })
    };
    let unit_name = unit.unit_name();
    steps.extend([
        systemctl(&["daemon-reload"]),
        systemctl(&["enable", &unit_name]),
        systemctl(&["restart", &unit_name]),
    ]);

    Ok(ProviderPlan {
        steps,
        generated_files: vec![
            GeneratedFile {
                path: unit.environment_file().to_path_buf(),
                contents: FileContents::Secret(systemd::environment_file_contents(&variables)?),
            },
            GeneratedFile {
                path: unit.path(),
                contents: FileContents::Public(unit.contents()?),
            },
        ],
        ..ProviderPlan::default()
    })
}

/// `skyzen deploy --provider native` installs a systemd unit, which only a Linux host has.
///
/// # Errors
///
/// Always.
#[cfg(not(target_os = "linux"))]
pub fn deploy(_manifest: &Manifest, _project: &Project, _dry_run: bool) -> Result<ProviderPlan> {
    anyhow::bail!(
        "`skyzen deploy --provider native` installs a systemd service on the Linux host it \
         runs on; this machine runs {}",
        std::env::consts::OS
    )
}

#[cfg(target_os = "linux")]
/// Where the kernel says unprivileged processes may start binding ports
/// (`ip-sysctl.rst`, `ip_unprivileged_port_start`).
const UNPRIVILEGED_PORT_START: &str = "/proc/sys/net/ipv4/ip_unprivileged_port_start";

#[cfg(target_os = "linux")]
/// The lowest port a process without `CAP_NET_BIND_SERVICE` may bind on this host.
fn unprivileged_port_start() -> Result<u16> {
    let value = std::fs::read_to_string(UNPRIVILEGED_PORT_START)
        .with_context(|| format!("cannot read {UNPRIVILEGED_PORT_START}"))?;
    value
        .trim()
        .parse()
        .with_context(|| format!("{UNPRIVILEGED_PORT_START} holds {value:?}, not a port"))
}

/// Refuse a path the service's transient user cannot reach: every ancestor directory has to be
/// searchable by others, and the path itself searchable (a directory) or readable and executable
/// (the binary), since `DynamicUser=` runs the service as a user that owns nothing.
#[cfg(target_os = "linux")]
fn ensure_reachable(path: &std::path::Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    /// `S_IXOTH` and `S_IROTH` (`inode(7)`).
    const OTHERS_EXECUTE: u32 = 0o001;
    const OTHERS_READ: u32 = 0o004;

    let mode = |path: &std::path::Path| {
        std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode())
            .with_context(|| format!("cannot read the permissions of {}", path.display()))
    };
    for ancestor in path.ancestors().skip(1) {
        if mode(ancestor)? & OTHERS_EXECUTE == 0 {
            anyhow::bail!(
                "{} is not searchable by other users, so the service's transient user cannot \
                 reach {}; `chmod o+x {}` or deploy from a directory others can reach",
                ancestor.display(),
                path.display(),
                ancestor.display()
            );
        }
    }
    let needed = if executable {
        OTHERS_READ | OTHERS_EXECUTE
    } else {
        OTHERS_EXECUTE
    };
    if mode(path)? & needed != needed {
        anyhow::bail!(
            "{} is not {} by other users, so the service's transient user cannot use it",
            path.display(),
            if executable {
                "readable and executable"
            } else {
                "searchable"
            }
        );
    }
    Ok(())
}

/// The plan of `skyzen deploy --provider native --host-only`: bring this host to
/// `[native.linux]`, and nothing else.
///
/// # Errors
///
/// Fails when `[native.linux]` asks for a facility this host lacks or does not report, and when
/// a setting has to change and the CLI is not root.
pub fn host_only(manifest: &Manifest, dry_run: bool) -> Result<ProviderPlan> {
    Ok(ProviderPlan {
        steps: host_steps(manifest, dry_run)?,
        ..ProviderPlan::default()
    })
}

/// One step per setting of `[native.linux]` this host does not have yet; none without the
/// section.
fn host_steps(manifest: &Manifest, dry_run: bool) -> Result<Vec<Step>> {
    manifest
        .data()
        .native
        .as_ref()
        .and_then(|native| native.linux.as_ref())
        .map_or_else(
            || Ok(Vec::new()),
            |section| crate::host::apply_steps(section, dry_run),
        )
}

/// Build the plan for one native action.
///
/// # Errors
///
/// Fails when the action has no native meaning, or when the manifest declares an environment
/// variable that is set nowhere.
pub fn prepare(action: &Action, manifest: &Manifest) -> Result<ProviderPlan> {
    let root_dir = manifest.root_dir().to_path_buf();

    let (args, run_mode) = match action {
        Action::Dev { runner_args } => {
            let mut args = vec!["run".to_owned()];
            if !runner_args.is_empty() {
                // `cargo run` needs the separator before the binary's own arguments, so
                // `skyzen dev -- --port 3000` reaches the application rather than cargo.
                args.push("--".to_owned());
                args.extend(runner_args.iter().cloned());
            }
            (args, RunMode::Restart)
        }
        Action::Build { release } => {
            let mut args = vec!["build".to_owned()];
            if *release {
                args.push("--release".to_owned());
            }
            (args, RunMode::Once)
        }
        other => anyhow::bail!(
            "`skyzen {}` has no native implementation",
            super::action_name(other)
        ),
    };

    // Read the manifest's `url_env` / `bucket_env` declarations *before* starting the child, so a
    // variable the application would have panicked on at its first request is a startup error
    // naming the manifest key that asked for it.
    //
    // Only for `dev`, which actually runs the application: a `build` needs nothing from the
    // runtime environment, and gating compilation on a connection string set only in production
    // would break every CI box.
    let child_env = if matches!(action, Action::Dev { .. }) {
        prepare_child_environment(manifest, VariableKind::ALL)?.child_env
    } else {
        Vec::new()
    };

    Ok(ProviderPlan {
        prepare: Vec::new(),
        steps: vec![Step::Command(CommandPlan {
            program: "cargo".to_owned(),
            args,
            cwd: Some(root_dir.clone()),
            stdin: CommandStdin::Inherit,
        })],
        generated_files: Vec::new(),
        build: None,
        run_mode,
        child_env,
        watch_root: matches!(action, Action::Dev { .. }).then_some(root_dir),
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

        let dev = prepare(
            &Action::Dev {
                runner_args: vec!["--port".to_owned(), "3000".to_owned()],
            },
            &manifest,
        )
        .expect("dev plan");
        assert_eq!(dev.run_mode, RunMode::Restart);
        // The separator is what makes the arguments reach the application, not cargo.
        assert!(dev.steps[0].describe().contains("cargo run -- --port 3000"));
        assert!(dev.watch_root.is_some());

        let build = prepare(&Action::Build { release: true }, &manifest).expect("build plan");
        assert_eq!(build.run_mode, RunMode::Once);
        assert!(build.steps[0].describe().contains("cargo build --release"));
        assert!(build.watch_root.is_none());
    }

    #[test]
    fn a_cloud_only_action_is_refused_with_a_reason() {
        let error = prepare(
            &Action::Logs {
                wrangler_args: Vec::new(),
            },
            &manifest(""),
        )
        .expect_err("native has no logs");
        assert!(
            error.to_string().contains("no native implementation"),
            "{error}"
        );
    }

    /// Without the section there is nothing to apply, on any machine; with it, the host the CLI
    /// runs on has to be Linux.
    #[test]
    fn the_host_half_applies_only_what_the_manifest_states() {
        let plan = super::host_only(&manifest(""), true).expect("nothing to apply");
        assert!(plan.steps.is_empty());

        if !cfg!(target_os = "linux") {
            let error = super::host_only(&manifest("[native.linux]\nbusy_poll = 50\n"), true)
                .expect_err("not Linux");
            assert!(error.to_string().contains("Linux"), "{error}");
        }
    }

    #[test]
    fn a_declared_variable_that_is_set_nowhere_stops_dev_before_it_starts() {
        let manifest = manifest(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [native.service.cache]\nbackend = \"redis\"\nurl_env = \"SKYZEN_NEVER_SET_URL\"\n",
        );
        // A build needs nothing from the runtime environment, so it must not be gated on a
        // connection string that only production sets.
        prepare(&Action::Build { release: true }, &manifest).expect("a build needs no env");

        let error = prepare(
            &Action::Dev {
                runner_args: Vec::new(),
            },
            &manifest,
        )
        .expect_err("the variable is not set");
        assert!(
            error.to_string().contains("SKYZEN_NEVER_SET_URL"),
            "{error}"
        );
    }
}
