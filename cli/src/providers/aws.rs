//! The AWS Lambda provider.
//!
//! Everything here is `cargo lambda`: it cross-compiles the binary to a Linux target, packages it
//! as `bootstrap`, and creates or updates the function. Skyzen's job is to turn `[aws]` into its
//! flags, so that what is deployed is what the manifest says — and to stop before doing anything
//! when the two disagree.
//!
//! The application itself needs no AWS-specific code: built with the `lambda` feature, the same
//! binary notices `AWS_LAMBDA_RUNTIME_API` and serves invocations instead of binding a port.
//!
//! The one thing `cargo lambda` is *not* asked to do is carry the function's environment: a
//! `--env-var NAME=value` flag is printed by every progress line and visible in the process table,
//! so the variables are delivered afterwards through the SDK instead. See [`lambda_env`].

mod lambda_env;

use crate::{
    environment::VariableKind,
    output,
    project::Project,
    providers::{
        prepare_child_environment, secrets::Delivery, Action, CommandPlan, CommandStdin,
        ProviderPlan, RunMode, SecretAction, Step,
    },
};
use anyhow::Result;
use lambda_env::{LambdaEnvironment, LambdaEnvironmentNames};
use skyzen_manifest::{AwsSection, Manifest};

/// The feature the application must be built with for a Lambda deployment to serve anything.
pub const LAMBDA_FEATURE: &str = "lambda";

/// Build the plan for one AWS action.
///
/// # Errors
///
/// Fails when the project cannot name the binary to deploy, when the action has no AWS meaning, or
/// when the manifest declares an environment variable that is set nowhere.
pub fn prepare(action: &Action, manifest: &Manifest, project: &Project) -> Result<ProviderPlan> {
    let config = manifest.data().aws.clone().unwrap_or_default();
    let root_dir = manifest.root_dir().to_path_buf();
    let binary = project.binary_target_name()?.to_owned();
    let function = function_name(&config, &binary);

    ensure_lambda_feature(project);
    if matches!(action, Action::Deploy) {
        report_event_source(manifest, &function);
    }

    // `cargo lambda deploy` reads AWS credentials from the environment, and a `.env` entry the
    // shell does not already hold is part of that environment.
    let mut child_env = Vec::new();

    let steps = match action {
        Action::Build { release } => {
            vec![Step::Command(build_command(&config, &root_dir, *release))]
        }
        // A deploy always builds optimized first: `cargo lambda deploy` uploads whatever is in
        // `target/lambda`, so deploying without building would ship the previous build.
        Action::Deploy => {
            let prepared = prepare_child_environment(manifest, VariableKind::ALL)?;
            child_env = prepared.child_env;
            let mut steps = vec![
                Step::Command(build_command(&config, &root_dir, true)),
                Step::Command(deploy_command(&config, &root_dir, &binary)),
            ];
            let delivery = Delivery::from_resolved(&prepared.resolved).with_defaults(&config.env);
            if !delivery.is_empty() {
                steps.push(Step::Task(Box::new(LambdaEnvironment::new(
                    function, delivery,
                ))));
            }
            steps
        }
        Action::Logs { wrangler_args } => {
            vec![Step::Command(logs_command(
                &root_dir,
                &function,
                wrangler_args,
            ))]
        }
        Action::Secret(secret) => vec![secret_step(secret, manifest, &config, function)?],
        other => anyhow::bail!(
            "`skyzen {}` has no AWS implementation{}",
            super::action_name(other),
            unsupported_hint(other)
        ),
    };

    Ok(ProviderPlan {
        steps,
        generated_files: Vec::new(),
        build: None,
        run_mode: RunMode::Once,
        child_env,
        watch_root: None,
        execute_despite_dry_run: false,
    })
}

/// The one step a `skyzen secret` action performs.
///
/// Every one of them is the same `UpdateFunctionConfiguration` call: Lambda has no secret store of
/// its own, so a function's environment *is* where its values live, and delivering one is
/// delivering all of them merged over what is already there.
///
/// # Errors
///
/// Fails when a declared variable is set nowhere, or when there is nothing at all to push.
fn secret_step(
    action: &SecretAction,
    manifest: &Manifest,
    config: &AwsSection,
    function: String,
) -> Result<Step> {
    let delivery = match action {
        SecretAction::Set { name, value } => Delivery::one(name.as_str(), value),
        SecretAction::Push => {
            let resolved = prepare_child_environment(manifest, VariableKind::ALL)?.resolved;
            let delivery = Delivery::from_resolved(&resolved).with_defaults(&config.env);
            if delivery.is_empty() {
                anyhow::bail!(
                    "there is nothing to push: Skyzen.toml declares no [[secret]], no native \
                     wiring variable and no [aws.env] entry"
                );
            }
            delivery
        }
        SecretAction::List => {
            return Ok(Step::Task(Box::new(LambdaEnvironmentNames::new(function))))
        }
    };
    Ok(Step::Task(Box::new(LambdaEnvironment::new(
        function, delivery,
    ))))
}

/// The function a deploy acts on: the manifest's name, or the binary's own.
///
/// The same rule `cargo lambda deploy` applies to the positional argument it is given, computed
/// once so the upload, the log tail and the environment delivery cannot name different functions.
fn function_name(config: &AwsSection, binary: &str) -> String {
    config
        .function_name
        .clone()
        .unwrap_or_else(|| binary.to_owned())
}

/// What to suggest when an action has no AWS counterpart.
const fn unsupported_hint(action: &Action) -> &'static str {
    match action {
        Action::Dev { .. } => {
            ": run it as an ordinary server with `skyzen dev`, or invoke it with `cargo lambda watch`"
        }
        Action::Migrate { .. } => {
            ": point `skyzen migrate` at the database directly rather than through the function"
        }
        _ => "",
    }
}

/// `cargo lambda build`, cross-compiling to the architecture the manifest declares.
///
/// The target triple is named outright rather than left to `cargo lambda`'s default, so the
/// architecture in the manifest is the architecture that gets built and later deployed.
fn build_command(config: &AwsSection, root_dir: &std::path::Path, release: bool) -> CommandPlan {
    let mut args = vec![
        "lambda".to_owned(),
        "build".to_owned(),
        "--target".to_owned(),
        config.architecture.target_triple().to_owned(),
    ];
    if release {
        args.push("--release".to_owned());
    }
    // The adapter is optional in the framework, so a Lambda build has to ask for it explicitly.
    args.push("--features".to_owned());
    args.push(LAMBDA_FEATURE.to_owned());

    CommandPlan {
        program: "cargo".to_owned(),
        args,
        cwd: Some(root_dir.to_path_buf()),
        stdin: CommandStdin::Inherit,
    }
}

/// `cargo lambda deploy`, carrying the sizing and the URL the manifest declares.
fn deploy_command(config: &AwsSection, root_dir: &std::path::Path, binary: &str) -> CommandPlan {
    let mut args = vec!["lambda".to_owned(), "deploy".to_owned()];

    args.push("--binary-name".to_owned());
    args.push(binary.to_owned());

    if let Some(memory) = config.memory_mb {
        args.push("--memory".to_owned());
        args.push(memory.to_string());
    }
    if let Some(timeout) = config.timeout {
        args.push("--timeout".to_owned());
        args.push(timeout.as_secs().to_string());
    }
    // No `--env-var`: `[aws.env]` and every declared variable are delivered by the SDK step that
    // follows, so no value reaches this command line.
    // Named in both directions: turning `url` off should *remove* the URL rather than leave the
    // one an earlier deploy created still serving the internet.
    args.push(
        if config.url {
            "--enable-function-url"
        } else {
            "--disable-function-url"
        }
        .to_owned(),
    );

    // The function name is positional and defaults to the binary's, so it is only passed when the
    // manifest asks for a different one.
    if let Some(name) = &config.function_name {
        args.push(name.clone());
    }

    CommandPlan {
        program: "cargo".to_owned(),
        args,
        cwd: Some(root_dir.to_path_buf()),
        stdin: CommandStdin::Inherit,
    }
}

/// Follow the function's `CloudWatch` log group.
///
/// `cargo lambda` has no `logs` subcommand — it covers building, deploying and invoking — so this
/// is the AWS CLI's log tail against the log group Lambda creates for the function.
fn logs_command(root_dir: &std::path::Path, function: &str, extra: &[String]) -> CommandPlan {
    let mut args = vec![
        "logs".to_owned(),
        "tail".to_owned(),
        format!("/aws/lambda/{function}"),
        "--follow".to_owned(),
    ];
    args.extend(extra.iter().cloned());

    CommandPlan {
        program: "aws".to_owned(),
        args,
        cwd: Some(root_dir.to_path_buf()),
        stdin: CommandStdin::Inherit,
    }
}

/// Warn when the project's own manifest does not turn the Lambda adapter on.
///
/// A warning rather than an error: the feature can also arrive through another crate in the graph,
/// which `cargo metadata --no-deps` cannot see. The binary itself refuses to start inside Lambda
/// without it, naming the same feature, so a false negative here costs one failed invocation
/// rather than a silent one.
fn ensure_lambda_feature(project: &Project) {
    if !project.depends_on("skyzen") || project.dependency_enables("skyzen", LAMBDA_FEATURE) {
        return;
    }
    output::warn(format!(
        "the `skyzen` dependency does not enable `features = [\"{LAMBDA_FEATURE}\"]`; without it \
         the binary refuses to start inside Lambda"
    ));
}

/// Report what `skyzen doctor --provider aws` can tell from the manifest alone.
pub fn check_manifest(manifest: &Manifest) -> usize {
    // The Lambda binary is the native binary, so a deploy delivers both kinds — and, unlike
    // `[aws.env]`, they have to have a value here for the deploy to run at all.
    super::report_runtime_variables(manifest, VariableKind::ALL, "aws", "skyzen deploy");

    let Some(config) = manifest.data().aws.as_ref() else {
        output::warn("Skyzen.toml has no [aws] section; the defaults deploy an arm64 function with a Function URL");
        return 0;
    };

    output::ok(format!(
        "aws: deploying {} to {}{}",
        config
            .function_name
            .as_deref()
            .unwrap_or("the binary's own name"),
        config.architecture.as_str(),
        if config.url {
            " with a Function URL"
        } else {
            " with no Function URL"
        }
    ));
    0
}

/// Say what a queue-consuming Lambda still needs, when the manifest says it consumes one.
///
/// `cargo lambda deploy` uploads a function; it does not subscribe one to a queue. Left unsaid,
/// the first symptom is a deployment that looks complete and never receives a message.
fn report_event_source(manifest: &Manifest, function: &str) {
    let consumes_a_queue = manifest
        .data()
        .native
        .as_ref()
        .is_some_and(|native| !native.queue_consumer.is_empty());
    if !consumes_a_queue {
        return;
    }

    output::warn(format!(
        "this application consumes a queue: Lambda delivers SQS batches only through an event \
         source mapping, which this deploy does not create. Run:\n    {}",
        event_source_hint(function)
    ));
}

/// The command a queue-consuming Lambda still needs a human to run.
///
/// An SQS event source mapping is not something `cargo lambda deploy` creates, and without
/// `ReportBatchItemFailures` the partial batch response Skyzen returns is ignored and the whole
/// batch is retried. Printed rather than run: it needs the queue's ARN, which the manifest does
/// not carry.
fn event_source_hint(function: &str) -> String {
    format!(
        "aws lambda create-event-source-mapping --function-name {function} \
         --event-source-arn <queue-arn> --function-response-types ReportBatchItemFailures"
    )
}

#[cfg(test)]
mod tests {
    use super::{event_source_hint, prepare};
    use crate::providers::{Action, SecretAction, Step};
    use secrecy::SecretString;
    use skyzen_manifest::{Manifest, VarName};

    fn manifest(source: &str) -> Manifest {
        Manifest::parse(source, "Skyzen.toml", "/tmp/app").expect("valid manifest")
    }

    /// A project whose `cargo metadata` says it builds one binary called `demo`.
    fn project() -> crate::project::Project {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin-app");
        crate::project::Project::load(&dir).expect("the fixture package loads")
    }

    fn planned(action: &Action, source: &str) -> Vec<String> {
        prepare(action, &manifest(source), &project())
            .expect("plan")
            .steps
            .iter()
            .map(Step::describe)
            .collect()
    }

    #[test]
    fn a_build_cross_compiles_to_the_declared_architecture_with_the_adapter_enabled() {
        let arm = planned(&Action::Build { release: true }, "");
        assert_eq!(arm.len(), 1);
        assert!(
            arm[0].contains("cargo lambda build --target aarch64-unknown-linux-gnu --release"),
            "{arm:?}"
        );
        assert!(arm[0].contains("--features lambda"), "{arm:?}");

        let intel = planned(
            &Action::Build { release: true },
            "[aws]\narchitecture = \"x86_64\"\n",
        );
        assert!(
            intel[0].contains("--target x86_64-unknown-linux-gnu"),
            "{intel:?}"
        );
    }

    #[test]
    fn a_deploy_builds_first_and_then_carries_the_manifests_configuration() {
        let planned = planned(
            &Action::Deploy,
            "[aws]\nfunction_name = \"skyzen-api\"\nmemory_mb = 512\ntimeout = \"45s\"\n\n\
             [aws.env]\nRUST_LOG = \"info\"\n",
        );

        assert_eq!(planned.len(), 3, "{planned:?}");
        assert!(planned[0].contains("cargo lambda build"), "{planned:?}");

        let deploy = &planned[1];
        assert!(deploy.contains("cargo lambda deploy"), "{deploy}");
        assert!(deploy.contains("--binary-name demo"), "{deploy}");
        assert!(deploy.contains("--memory 512"), "{deploy}");
        // humantime in, seconds out: that is the unit Lambda takes.
        assert!(deploy.contains("--timeout 45"), "{deploy}");
        assert!(deploy.contains("--enable-function-url"), "{deploy}");
        assert!(deploy.ends_with("skyzen-api)"), "{deploy}");
    }

    #[test]
    fn nothing_a_deploy_runs_carries_a_value_on_its_command_line() {
        // The whole reason `[aws.env]` no longer reaches `cargo lambda deploy`: its command line
        // is printed by every progress line and by `--dry-run`, and is visible in the process
        // table of whatever machine runs the deploy.
        let planned = planned(&Action::Deploy, "[aws]\n\n[aws.env]\nRUST_LOG = \"info\"\n");

        for step in &planned {
            assert!(!step.contains("--env-var"), "{planned:?}");
            assert!(!step.contains("info"), "{planned:?}");
        }
    }

    #[test]
    fn a_deploy_ends_by_delivering_the_variables_it_names_and_no_values() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(".env"), "STRIPE_KEY=sk_live_123\n").expect("write .env");
        let manifest = Manifest::parse(
            "[[secret]]\nname = \"STRIPE_KEY\"\n\n[aws]\nfunction_name = \"skyzen-api\"\n\n\
             [aws.env]\nRUST_LOG = \"info\"\n",
            dir.path().join("Skyzen.toml"),
            dir.path(),
        )
        .expect("valid manifest");

        let plan = prepare(&Action::Deploy, &manifest, &project()).expect("plan");
        let last = plan
            .steps
            .last()
            .expect("a deploy delivers its environment");
        let described = last.describe();

        assert!(matches!(last, Step::Task(_)), "{described}");
        assert!(described.contains("skyzen-api"), "{described}");
        assert!(described.contains("RUST_LOG"), "{described}");
        assert!(described.contains("STRIPE_KEY"), "{described}");
        assert!(!described.contains("sk_live_123"), "{described}");
        assert!(!described.contains("info"), "{described}");
    }

    #[test]
    fn a_deploy_refuses_when_a_declared_variable_is_set_nowhere() {
        let error = prepare(
            &Action::Deploy,
            &manifest(
                "[[secret]]\nname = \"SKYZEN_TEST_AWS_UNSET_SECRET\"\n\n\
                 [[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
                 [native.service.cache]\nbackend = \"redis\"\n\
                 url_env = \"SKYZEN_TEST_AWS_UNSET_URL\"\n",
            ),
            &project(),
        )
        .expect_err("the deployed function would panic at cold start");

        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("SKYZEN_TEST_AWS_UNSET_SECRET"),
            "{rendered}"
        );
        assert!(rendered.contains("SKYZEN_TEST_AWS_UNSET_URL"), "{rendered}");
    }

    #[test]
    fn setting_one_secret_delivers_that_pair_and_names_no_value() {
        let plan = prepare(
            &Action::Secret(SecretAction::Set {
                name: VarName::try_from("STRIPE_KEY".to_owned()).expect("a name"),
                value: SecretString::from("sk_live_123"),
            }),
            &manifest(
                "[[secret]]\nname = \"STRIPE_KEY\"\n\n[aws]\nfunction_name = \"skyzen-api\"\n",
            ),
            &project(),
        )
        .expect("plan");

        let described = plan.steps[0].describe();
        assert!(described.contains("STRIPE_KEY"), "{described}");
        assert!(described.contains("skyzen-api"), "{described}");
        assert!(!described.contains("sk_live_123"), "{described}");
    }

    #[test]
    fn listing_reads_the_deployed_functions_own_environment() {
        let plan = prepare(
            &Action::Secret(SecretAction::List),
            &manifest("[aws]\nfunction_name = \"skyzen-api\"\n"),
            &project(),
        )
        .expect("plan");

        assert!(
            plan.steps[0].describe().contains("skyzen-api"),
            "{:?}",
            plan.steps[0]
        );
    }

    #[test]
    fn pushing_with_nothing_declared_says_so_rather_than_calling_aws() {
        let error = prepare(
            &Action::Secret(SecretAction::Push),
            &manifest("[aws]\n"),
            &project(),
        )
        .expect_err("there is nothing to deliver");

        assert!(format!("{error:#}").contains("nothing to push"), "{error}");
    }

    #[test]
    fn turning_the_url_off_removes_it_rather_than_leaving_the_old_one_serving() {
        let planned = planned(&Action::Deploy, "[aws]\nurl = false\n");

        assert!(planned[1].contains("--disable-function-url"), "{planned:?}");
        assert!(!planned[1].contains("--enable-function-url"), "{planned:?}");
    }

    #[test]
    fn logs_tail_the_functions_own_log_group() {
        let planned = planned(
            &Action::Logs {
                wrangler_args: vec!["--since".to_owned(), "10m".to_owned()],
            },
            "[aws]\nfunction_name = \"skyzen-api\"\n",
        );

        assert!(
            planned[0].contains("aws logs tail /aws/lambda/skyzen-api --follow"),
            "{planned:?}"
        );
        assert!(planned[0].contains("--since 10m"), "{planned:?}");
    }

    #[test]
    fn logs_fall_back_to_the_binary_name_the_deploy_would_have_used() {
        let planned = planned(
            &Action::Logs {
                wrangler_args: Vec::new(),
            },
            "",
        );

        assert!(planned[0].contains("/aws/lambda/demo"), "{planned:?}");
    }

    #[test]
    fn an_action_with_no_aws_meaning_says_what_to_do_instead() {
        let error = prepare(
            &Action::Dev {
                runner_args: Vec::new(),
            },
            &manifest(""),
            &project(),
        )
        .expect_err("there is no `cargo lambda` dev server for this");

        assert!(error.to_string().contains("skyzen dev"), "{error}");
    }

    #[test]
    fn the_event_source_hint_names_the_setting_partial_batch_responses_need() {
        let hint = event_source_hint("skyzen-api");

        assert!(hint.contains("ReportBatchItemFailures"), "{hint}");
        assert!(hint.contains("skyzen-api"), "{hint}");
    }
}
